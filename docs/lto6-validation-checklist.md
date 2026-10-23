# LTO-6 Hardware Validation Checklist

Rewritten 2026-09-24 (ADR-0012's 2026-09-24 amendment, item 10). This is the procedure
for validating tapectl against a real drive and cartridge, re-runnable for any new drive,
cartridge batch, or host. Every item below has been run on real hardware at least once;
the record for each is named so a re-run can be compared with it. The pre-2026-09
version of this file (v1's "layered EOT recovery" and `/dev/nst0` addressing) is
superseded: Layout v2 has no end-of-tape salvage (ADR-0007), and no device is ever
addressed by number.

**The validation drive:** the HP Ultrium 6-SCSI, serial HUJ808A5L4, at
`/dev/tape/by-id/scsi-HUJ808A5L4-nst` on whichever machine holds it. Since 2026-09-28 it
belongs to production on `home2` and is detached from the development VM (ADR-0012); to
run this checklist on the VM, lend it back first (`docs/lto6-drive-passthrough.md`) and
detach it again before any production contact.
mhvtl's drives are `scsi-XYZZY_A*-nst`. `/dev/nstN` numbering moves across reboots and
after module reloads; every command below takes the by-id path. The expendable test
cartridge is FUJIFILM LTO-6, medium serial `EW7VWMVKF6`; it is never a production tape.

## What mhvtl cannot tell you (why this session exists)

- **End of tape.** mhvtl accepts writes past its configured capacity and silently loses
  the overflow (dry-run 2026-07-20). Measured again on 2026-10-20 (#420, a 500 MB LTO-8
  cartridge): st now returns ENOSPC once per block from 2 MiB before capacity and the retry
  lands each block, as a real drive's early warning does; but data past capacity is lost
  with no error (a file crossing it reads back truncated, one past it reads back empty),
  while filemarks are kept. So mhvtl shows the early warning, never what a drive keeps past
  it. Only a real drive answers where host writes stop, and how.
- **Capacity units and MAM.** mhvtl's MAM and page 0x0c/0x17 are static fictions.
- **Feed-rate effects.** mhvtl writes to disk; a real drive speed-matches down to about
  54 MB/s and shoe-shines below that, which costs tape.
- **`weof` at BOT.** mhvtl returns the old File 0 bytes; the real drive reads an EMPTY
  File 0 (issue #327). The refusal and the `--force` path are the same either way.
- **TapeAlert read-to-clear.** Still unanswered on the HP drive (every flag has read 0);
  `report health` surfaces the first non-zero one (#340).

## Pre-flight (no tape motion)

- [ ] `ls -l /dev/tape/by-id/` shows the drive by serial; `readlink -f` it and confirm
      the sg node from sysfs: `ls /sys/class/scsi_tape/<nstN>/device/scsi_generic/`.
      tapectl checks this pairing itself (#329): `tapectl config check` warns and
      `volume write` refuses if `device_sg` is not that node.
- [ ] `mt -f <by-id> status` succeeds; `sg_inq <sg>` reports the vendor and model
      (the model gives the drive's generation: "Ultrium 6-SCSI" → LTO-6).
- [ ] `sg_read_attr <sg>` returns the medium serial and the capacity attributes;
      `sg_logs --page=0x00 --maxlen=65532 --raw <sg>` lists the supported pages (22 on
      the HP; the fixtures under `tests/fixtures/sg_logs/hp_lto6_*` are the 2026-09-23
      captures). Read pages with `--maxlen` so each read is ONE LOG SENSE (#328); do not
      read 0x2E by hand at all — tapectl's sweep journals it once per contact.
- [ ] `mt -f <by-id> setblk 524288 && mt -f <by-id> status` reports the 512 KiB block
      size. (Record: 512 K vs 1 M is a wash on this drive — 114.0 vs 114.4 MiB/s,
      `docs/lto6-session-journal-2026-09-10.md`.)
- [ ] Compression as found: `sg_logs`/mode page 0x0f `DCE`. tapectl disables it per
      write; the record shows `DCE 1→0` verified.
- [ ] `dar --version` ≥ 2.6; `age` present (RESTORE.sh and the rehearsal need it).
- [ ] The mhvtl gate is GREEN on this checkout (`TAPECTL_GATE_TAPE=/dev/nst3
      TAPECTL_MHVTL=1 bash scripts/mhvtl-verify-gate.sh`, 40 checks since #355 added
      `empty_drive_refused`). The gate always runs `cargo build` and tests the
      checkout's own debug binary; it has no `TAPECTL_BIN`, so a green gate says
      nothing about an installed release binary. The rehearsal's `TAPECTL_BIN=` run
      below is what tests that one. The gate is the one exception to by-id addressing: it
      wants the `/dev/nstN` spelling of an **LTO-8** emulated drive (`lsscsi` shows
      `ULT3580-TD8`; its LTO-8 backend refuses a TD6 drive's LTO-6 media), so read the
      number off `ls -l /dev/tape/by-id/` and `lsscsi` first; `nst3` is only an example.
- [ ] Nothing else will touch the drive: every harness takes `/tmp/tapectl-tape.lock`.
- [ ] **The host is quiet** for the duration: CI runners and their timers paused, no
      heavy builds on the staging disk (`docs/operator-guide.md`, "A quiet host while
      the tape runs").

## The rehearsal (the round trip, every restore path, the heir script)

This replaces the hand-run round-trip of earlier versions. It erases the named cartridge.

```bash
TAPECTL_BIN=/usr/local/bin/tapectl scripts/lifecycle-suite.sh --scenario first-year \
    --device /dev/tape/by-id/scsi-HUJ808A5L4-nst --erase short --single-cartridge \
    --i-will-lose-the-cartridge EW7VWMVKF6
```

- [ ] `first-year` GREEN (47 checks; record: 47/47 on 2026-09-23, on the debug and the
      release binary). It writes a multi-tenant volume, verifies, and restores every unit
      ten ways, including `RESTORE.sh` run off the tape with tenant, operator, backup and
      escrow keys, and the raw dump.
- [ ] Optionally every single-cartridge scenario in turn (record: 15 scenarios, 342
      checks, 0 failed, 27 structural skips, `docs/runs/2026-09-23-real-drive-rehearsal.md`).
      `compaction` needs four cartridges; `cartridge-displacement` and
      `collection-second-copy` need two.
- [ ] The forensic record: `scripts/realdrive-forensics.py <run>/<scenario>/home/tapectl.db
      HUJ808A5L4` — every contact names the drive, every closed contact swept exactly the
      pages page 0x00 listed, each once, raw bytes kept, counters present, the cartridge
      sized from its detected generation (2.5e12 for LTO-6).
- [ ] The release binary, not only debug: `TAPECTL_BIN=` as above; `first-run.sh` step 12
      does this and records a marker per binary that step 13 requires.

## Disaster recovery from the real tape

- [ ] Into a bare home with NO drive configured (the heir's case):
      `tapectl --home <tmp> catalog rebuild --from-volume --device <by-id> --key <operator or escrow secret>`
      → the units, snapshots, stage sets, writes, tenants and the cartridge appear; the
      contact says "no LTO backend is configured on this host" (record: 2026-09-23).
- [ ] `tapectl --home <tmp> restore unit --unit <name> --from <label> --to <dir> --device <by-id>`
      then `diff -r --no-dereference <source> <dir>` — identical (record: 2026-09-23).
- [ ] The heir script alone (no tapectl), extracted as File 1 tells an heir to, with the
      device named on every command:
      `mt -f <by-id> rewind && mt -f <by-id> fsf 2 && dd if=<by-id> bs=512k | tr -d '\0' > RESTORE.sh && chmod +x RESTORE.sh`.
      RESTORE.sh reads `/dev/nst0` unless `TAPE_DEVICE` names another drive, so run it as
      `TAPE_DEVICE=<by-id> ./RESTORE.sh --info`, then `--find-envelope --key <key>` and
      `--restore --key <key> --to <dir>` the same way.
      Every key the tenant holds must open its own leg (#288). This is the lifecycle
      suite's `restore_sh_*` checks, green on hardware.

## End of tape (measured once; re-run only for a new drive or media type)

There is no EOT salvage in Layout v2: a real EOT is a clean abort to an unsealed tape,
and the pre-flight gate (generation table × the fill ceiling, 97% by default) is the capacity defence. What
to measure is where the drive stops host writes, with `scripts/lto-profile.sh` (the fill is its last
measurement; `scripts/lto6-fill.sh` remains as a wrapper that runs the fill alone, and
`scripts/lto6-measure.sh` as one that runs the minutes-long measurements alone):

```bash
scripts/lto-profile.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst --i-will-lose-the-cartridge EW7VWMVKF6
```

`docs/lto-profiling.md` is the operator-facing account: what each measurement takes, the run
directory and the `profile.json` report, and `scripts/lto-profile.sh --emit RUN_DIR...`, which
prints the drive's `[[backends.lto]]` block (`fill_ceiling` and `post_warning_bytes`) from one or more
runs. `--quick` leaves the fill out for a re-check that takes minutes.

- [ ] One continuous stream (a chunk-per-`dd` loop stops the drive at every close and
      measured 77 MB/s; the stream ran at 163 MB/s).
- [ ] Record: **2,501,995,134,976 bytes accepted before ENOSPC** (2.5020 TB), 1.0008 × the
      2.5 TB planning figure; page 0x0c BOP→EOD 2,513,648 MB; native per data byte
      1.0047; MAM "remaining" still 101,850 MiB at ENOSPC — the early-warning reserve,
      NOT host-writable space. Page 0x17's "used" read at EOD before a rewind is partial.
      (`docs/runs/2026-09-23-real-drive-rehearsal.md`, "The end-of-tape fill".)
- [ ] The capacity units: MAM's attributes are MiB; page 0x17 is decimal MB; the two
      agree to the megabyte (`docs/runs/2026-09-23-lto6-capacity-measurement.md`, #182).
- [ ] The feed-rate effect: 1.48 native bytes per data byte behind a bursty pipe, 1.000
      to 1.0047 for steady feeds (#323). tapectl records the ratio after every write and
      warns above 1.05 on a real drive (#338).

### Past the early warning (#420, #479; not yet run on the real drive)

Since 2026-10-17 the fill does not stop at the first refusal. One python process
(`scripts/lto6-fill-writer.py`, driven by `scripts/lto-profile.sh`) holds the drive open from BOT (closing st writes a filemark),
turns hardware compression off as tapectl does, and at the first ENOSPC records what st does
next: it writes one 1 GiB slice (2048 blocks) a block at a time, logging every return, stops
after 8 consecutive refusals or any other error, then writes a filemark, a seal-sized block and
a filemark, and reads the slice and the seal back, comparing them with what was written: by
LOCATE, or by spacing from BOT when READ POSITION or LOCATE fails (#487, `docs/lto-profiling.md`). It then keeps writing to the hard end (capped at 110 GB past the warning, about 12
minutes), measuring the room from early warning to the hard end, which bounds the slice size
(ADR-0012 item 100); `--no-hard-end` skips that walk and `--stop-at-warning` keeps the old
stop. The script names the cartridge by its medium serial (checked against MAM before anything is
written) and refuses while
`tapectl-drive-poll.timer` is active, on a rewinding node, or without room for its 2 GiB
keystream in the out dir (`/scratch/tapectl-lto6`, or `~/.cache/tapectl/lto6` where there is
no `/scratch`). About 5 hours in all, since the full run also makes the minutes-long measurements first;
`PROFILE.txt` in the run directory (`profile-<timestamp>-<serial>/`) is what to read, `fill/summary.txt`
the fill's own account, and exit 1 ("MEASUREMENT COMPLETE (readback: FAIL)") is a finding, not a failure.

- [ ] Record: what st returned after the first refusal (refused once then accepted, kept
      refusing, or short counts); whether the whole slice, the filemark and the seal block
      were accepted; each readback check.
- [ ] Record the room from early warning to the hard end, beside page 0x0c's "minimum native
      capacity from EW to EOP" (99,008 MB on 2026-09-23). The report states both and takes the
      lower as `post_warning_bytes` (ADR-0012 item 100); `--emit` prints the block.
- [ ] Record the cartridge's wear beside the end-of-tape point (`profile.json`, `wear`): MAM's
      load count and lifetime writes as found and at the end, and the drive's rewrite counters
      (ADR-0012 item 98). Recorded for a later fit, not fitted.
- [ ] Paste the emitted block into a test config and run `tapectl config check`.
- [ ] Erase the cartridge afterwards before it serves as the step-12 test cartridge again.

### The measurements that take minutes (`scripts/lto-profile.sh --quick`)

- [ ] Block size: 512 K and 1 M accepted, two passes in alternating order (record: a wash on this
      drive, 114.0 vs 114.4 MiB/s).
- [ ] Hardware compression as found (mode page 0x0f, DCE), and the Control Data Protection page
      (0x0a/0xf0): MODE SENSE only.
- [ ] MAM's capacity unit and over-report (record: MiB, over-report 0 bytes on 2026-09-23).
- [ ] End of data: a forward read past it errors, it does not return stale data (§3.2's assumption;
      not yet measured on the real drive).

## The short seal on the real drive (1.6.0 acceptance; #420)

ADR-0012 items 110, 114, 115 and 117: a write that meets the drive's real early warning seals
short, and what it wrote past the warning (at most one slice and the seal) reads back. A failed
readback here stops #420; it does not re-scope it. `scripts/short-seal-acceptance.sh` runs it on
the test cartridge EW7VWMVKF6 in a throwaway home, and the CTO authorizes each run (item 117).
`--print-plan` prints the operator's part and the drive time without touching anything.

**How the tape is filled** (item 115). Three small real units and, sorting between them, one
filler unit (`volume write` orders the batch by name: `ssa-a-docs`, `ssa-b-photos`,
`ssa-m-filler`, `ssa-z-tail`). The filler is staged normally from a source a little larger than
one slice, so its stage set is real (recipients, escrow receipt, dar catalogue) and its slice 1 is
a full 1 GiB slice. `examples/short_seal_filler.rs` (`fill`) then gives it about 2,370 slice rows,
all naming that one staged file. The write's checks read row counts and file sizes, and its
hashing streams the file each time, so no 2.6 TB staging area is needed. The filler is cut and
never restored. Its one file is moved to `/dev/shm` when there is room, because the write evicts
each slice from the page cache behind it.

**The capacity.** The run registers the cartridge at 2.55 TB (`cartridge register --capacity`,
in the throwaway catalog only), not the generation table's 2.5 TB:
- item 113 lets a plan reach 100% of the cartridge's capacity and no further;
- this cartridge's first refusal came at 2,502,136,168,448 bytes (2026-10-20) and
  2,501,995,134,976 (2026-09-23), 1.0009 of 2.5 TB;
- so a plan bounded at 2.5 TB ends about 2 GB before the warning, seals full and proves nothing.

At 2.55 TB the fill ceiling (97%, 2.47 TB) is below the warning and the plan bound is above it.
The filler is planned past the ceiling (item 113) and the warning lands inside it. The drive's
`capacity_override` is not used: it is for virtual drives. Production keeps the table figure.

**Before the run.**
- [ ] The whole chain is green on the candidate (item 117), including the mhvtl rehearsal:
      `scripts/short-seal-acceptance.sh --mhvtl --device /dev/tape/by-id/scsi-XYZZY_A3-nst`
      (an LTO-6 emulation; the store's test hook `TAPECTL_TEST_EARLY_WARNING_AT` stands in for
      the warning, because mhvtl loses data past its medium's end instead of refusing it).
- [ ] The candidate binary and its `short_seal_filler` example are built the same way and copied
      to home2 (`cargo build --example short_seal_filler`, statically for musl there).
- [ ] EW7VWMVKF6 loaded; `sudo systemctl stop tapectl-drive-poll.timer`; nothing else uses the
      drive; the host is quiet for the whole run (`docs/operator-guide.md`, "A quiet host while
      the tape runs").

```bash
TAPECTL_HOME="$HOME/.cache/tapectl/ssa-home-$(date +%Y%m%d)" \
TAPECTL_BIN=/path/to/tapectl-1.6.0 FIXTURE_BIN=/path/to/short_seal_filler \
  scripts/short-seal-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
    --i-will-lose-the-cartridge EW7VWMVKF6
```

It refuses (exit 2, nothing touched) when any of these fails:
- `TAPECTL_HOME` is unset, not empty, or one of production's homes. Those are `/etc/tapectl/home`'s
  target, the service user's home, `/srv/archive_meta/tapectl` and this account's `~/.tapectl`.
- The drive is not HUJ808A5L4, or the chip serial is not EW7VWMVKF6.
- The binary is older than 1.6.0.
- `TAPECTL_TEST_EARLY_WARNING_AT` is set.
- The drive-poll timer is active, or the tape lock is held.

**The steps, what each must show, and its drive time** (the write at ~158 MB/s, the reads at the
140 MiB/s production verify ran at):

| step | must show | time |
|---|---|---|
| 1 setup | no tape. The home, the config (`post_warning_bytes = "99008M"`, item 118's figure; `min_copies = 1`), four units staged, the filler's rows | ~10 min |
| 2 erase, `volume init SSA-0001` | whatever the cartridge held is erased by this home's `cartridge erase` (File 0's label confirmed), so init needs no `--force`; the medium line's capacity is the registered 2.55 TB | ~5 min |
| 3 `volume write` | exit 0, `"ending": "short"`, `ended_after` K, cut `ssa-m-filler` and `ssa-z-tail`; the volume `sealed` with `ended_after` K; `completed` and `cut` rows; the `write_early_warning` event; the confirm phase's bytes (`phase_timings`) cover the slice at file K and the seal, item 114's readback; the whole units' staging released (`min_copies = 1`) and the cut units' kept | ~4.5-5 h |
| 4 verify | `volume verify --full` exit 0; RESTORE.sh `--info` "Verdict: SEALED" and "ended early, after file K"; `--verify` "VERIFY: PASS" with the planned files after K marked NOT ON TAPE | ~9-10 h |
| 5 restore | `ssa-b-photos` (two slices) by `restore unit` and by RESTORE.sh, each identical to its source; the cut unit refused by both | ~15 min |
| 6 rebuild | `catalog rebuild --from-volume` into a second bare home; `short_seal_filler compare` agrees on the volume, its units, every write's status and every slice on the tape | ~15 min |
| 7 rewrite | the filler released by `staging clean --force`; `SSA-0001` retired; `cartridge erase`; `SSA-0002` initialised without `--force`; the cut unit written whole (full seal), verified and restored | ~20 min |

About 14-16 hours of drive time in all. Step 4's two whole-tape reads are most of it.

- [ ] Record: K, the `write_early_warning` event's `bytes_at` and `refusals` beside the profile's
      first refusal (2,502,136,168,448), the `write_feed_ratio` event, and each step's duration
      (`steps.tsv`).
- [ ] Record the readback past the warning (item 114): the step-3 log line "item 114: the confirm
      read back N bytes", from the confirm phase's own byte count. Steps 4 and 6 read the slice and
      the seal again.
- [ ] A FAIL stops the run and leaves the cartridge as that step left it. Read the failure first
      (`RESULT.txt`, the step's `.err` and `.out` files, `drive-at-fail/`), and keep the cartridge
      unwritten until it is understood (#420's lost readback). On a FAIL in step 3 or 4,
      `catalog-at-fail.db` and the tape are the evidence.
- [ ] Afterwards the cartridge holds `SSA-0002`, owned by the throwaway home. Retire it and run
      `cartridge erase` there before the cartridge is a test cartridge again. Then start the
      drive-poll timer.

The run directory (`/scratch/tapectl-short-seal/ssa-<UTC>-<serial>/`, or under
`~/.cache/tapectl/short-seal` where there is no `/scratch`) keeps every command's output. Per
step, it also keeps the drive's `/sys/class/scsi_tape/nstN/options` and `stats/`, MAM, and log
pages 0x02 0x03 0x0c 0x17 from before and after (one LOG SENSE each; never 0x2E).

## Spanning on the real drive (1.7.0 acceptance)

ADR-0012 item 137, as item 139 amends it (#478). Spanning stays off on home2 until this passes
(`docs/handoff.md` (h)). It uses the one test cartridge, EW7VWMVKF6, as two volumes in turn:
- **Tape 1** holds a unit that allows spanning, ended by the drive's real early warning. Its row
  is `range`, and what was written past the warning reads back (item 114).
- **RESTORE.sh's default mode** spools tape 1's range into scratch and is held at its prompt.
- **A rebuild of tape 1** records the range and names the slices it lacks.
- **Tape 2:** the same cartridge is written again as a whole copy, and the held restore finishes
  from scratch plus tape 2.

`--stream-tapes` and tapectl's continuation row (a range K1+1..N) need two cartridges and are
proven on mhvtl only (item 136; the gate's `span_*` checks).
`scripts/span-acceptance.sh` runs it in a throwaway home. `--print-plan` prints the operator's
part and the drive time without touching anything.

**How tape 1 is filled.** As in 1.6.0's run (item 115), with two units:
- **The filler** (`spn-filler`) is staged from a little more than one slice and given slice rows
  that all name its one staged file (`examples/short_seal_filler.rs`, `fill`). Here it is a whole
  unit that ends at the fill ceiling (97% of 2.5 TB, 2.425 TB).
- **The unit that may span** (`spn-span`, `allow_spanning = true` in its dotfile) comes after
  it. With such a unit queued, whole units stop at the ceiling, and that unit is planned last, past
  the plan bound to its end (items 83 and 123).

The drive's warning came near 2.502 TB on this cartridge (2026-10-21), so about 77 GB of a 100 GB
unit lands on tape 1. The cartridge keeps the generation table's 2.5 TB, as production does: unlike
1.6.0's run, nothing is registered at another capacity. `volume plan` must show the filler `committed`
and the unit the `tail` before any tape moves.

**The unit's source is copied.** `--span-source DIR` is the operator's real data, about 100 GB:
- the script refuses one too small to reach 8 GiB past the warning (about 93 GB);
- it copies the directory into `--work` and never writes the original. `unit init` writes a
  dotfile into the directory it registers;
- any `.tapectl-unit.toml` in the copy is removed (it belongs to another home);
- `defaults.global_excludes` is emptied, so every file is archived;
- the restore is compared with a sha256 manifest of the copy, taken once it was staged.

**Tape 2** (item 139). Tape 1 cannot be given up the 1.6.0 way:
- `volume retire` meets item 128's floor, because tape 1 is the only holder of slices 1..K1 of a
  version that is not yet current;
- `cartridge erase` refuses a volume the catalog still holds.

Step 4a shows both refusals and releases the filler from staging (tape 1 still gives it its copy).
It then writes one filemark at the start of the tape by hand (`mt weof 1`, the same short erase
`cartridge erase` does), so File 0 reads EMPTY, and runs `volume init SPN-0002 --force`. The chip
serial matches the cartridge's row, so binding marks `SPN-0001` `erased`. The unit then owes every
slice, and tape 2 holds it whole, written from the staging tape 1 kept (`--keep-staging`).

**The held restore.** RESTORE.sh's input is a FIFO the script holds open and answers in step 5.
It waits through steps 3 and 4 (an hour or so on home2) with no timeout and no tape device open;
the script checks its descriptors at the prompt and again before it answers. Its scratch keeps
only ciphertext.

**Before the run.**
- [ ] The whole chain is green on the 1.7.0 candidate, including the mhvtl rehearsal:
      `scripts/span-acceptance.sh --mhvtl --device /dev/tape/by-id/scsi-XYZZY_A3-nst` (an LTO-6
      emulation). It runs the same steps with a generated 48 MiB unit, 1 MiB slices, the drive's
      `capacity_override` at 200 MB and the store's test hook `TAPECTL_TEST_EARLY_WARNING_AT`
      inside the unit, for tape 1's write only.
- [ ] The candidate binary and its `short_seal_filler` example are built the same way and copied
      to home2. A debug build stages the 100 GB unit at about 3.9 MiB/s (1.6.0's run), about 7
      hours; a release build is faster.
- [ ] `--work` on a disk with about 4 x the unit free (the copy, its staging, the restore's
      scratch and the restored tree): on home2 a new directory on acache. `~/.cache` (/srv/home)
      has about 80 GB and is refused.
- [ ] The dar on `PATH` is 2.7.9 or later (home2's `/usr/local/bin/dar` is 2.7.21); RESTORE.sh
      restores a unit across tapes only with it.
- [ ] EW7VWMVKF6 loaded; `sudo systemctl stop tapectl-drive-poll.timer`; nothing else uses the
      drive; the host is quiet while the tape moves (`docs/operator-guide.md`, "A quiet host while
      the tape runs"). Start it inside tmux.

```bash
TAPECTL_HOME="$HOME/.cache/tapectl/spn-home-$(date +%Y%m%d)" \
TAPECTL_BIN=/path/to/tapectl-1.7.0 FIXTURE_BIN=/path/to/short_seal_filler \
  scripts/span-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
    --i-will-lose-the-cartridge EW7VWMVKF6 \
    --span-source /path/to/about-100-GB --work /srv/acache/spn-work-$(date +%Y%m%d)
```

It refuses (exit 2, nothing touched) when any of these fails:
- `TAPECTL_HOME` is unset, not empty, or one of production's homes (as for 1.6.0's run).
- The drive is not HUJ808A5L4, or the chip serial is not EW7VWMVKF6.
- The binary is older than 1.7.0.
- `TAPECTL_TEST_EARLY_WARNING_AT` is set.
- The drive-poll timer is active, or the tape lock is held.
- `--span-source` is missing, unreadable, or too small.
- `--work` is not empty, is inside production's home or staging, or lacks the room.
- The dar on `PATH` is older than 2.7.9.

**The steps, what each must show, and its time on home2** (the write at ~152 MiB/s and the reads
at ~108 MiB/s, as 1.6.0's run measured them):

| step | must show | time |
|---|---|---|
| 1a setup | no tape. The home and config (`post_warning_bytes = "99008M"`, `min_copies = 1`, no global excludes), the copy, both units staged, the manifest, the filler's rows; `volume plan` shows the filler `committed` and `spn-span` the `tail` | the copy and manifest ~30 min; staging (see above) |
| 1b init | whatever the cartridge held erased by this home's `cartridge erase`; `SPN-0001` initialised, the medium's capacity 2.5 TB | ~5 min |
| 1c write | exit 0, `"ending": "short"`, `ended_after` K; `range_units` names `spn-span` slices 1 to K1, nothing cut or deferred, the staging kept; rows `completed` (the filler) and `range` (`may_span` set); `spn-span`'s written positions consecutive, ending at K; the version `staged`; the confirm phase's bytes cover the slice at file K and the seal (item 114); File 0 and the short seal at K+1 state `requires = ["short-seal", "spanning"]` | ~4.5 h |
| 2 spool | RESTORE.sh off tape 1, `--restore --unit spn-span --scratch`: one prompt for slice K1+1; slices 1 to K1 in its scratch directory, each age ciphertext of its recorded size; alive, with its input on the FIFO and no tape device open | ~15 min |
| 3 rebuild | `catalog rebuild --from-volume` into a second home: `range_units` 1 to K1 of N; `spans_in_progress` names slices K1+1 to N missing, with no holder (a tape never names a later one); `spn-span` `staged`; `short_seal_filler compare` agrees | ~5 min |
| 4a re-init | `volume retire SPN-0001` refused ("Finish the span first"); `cartridge erase --dry-run` refused, naming `volume retire`; the filler released; one filemark at BOT; `volume init SPN-0002 --force` warns that `SPN-0001` is now marked erased, and it is | ~5 min |
| 4b write | `SPN-0002` seals full: `spn-span` `completed` with slices 1 to N, the version `current`; File 0 still states both features | ~15 min |
| 5 answer | ENTER on the FIFO: RESTORE.sh takes `SPN-0002` ("It gives slice(s) K1+1 to N", by the hashes its seal covers), "RESTORE COMPLETE", exit 0, its scratch removed; the restored tree equals the source's manifest | ~1 h |
| 6 close | `RESULT.txt` and the cleanup commands; nothing is erased | — |

About 5.5 hours of drive time; staging is most of the rest.

- [ ] Record: K and K1, N, the `write_early_warning` event's `bytes_at` and `refusals` beside
      1.6.0's 2,502,136,692,736, the `write_feed_ratio` event, how long the restore was held, and
      each step's duration (`steps.tsv`).
- [ ] Record the readback past the warning (item 114): step 1c's log line "item 114: the confirm
      read back N bytes".
- [ ] A FAIL stops the run and leaves the cartridge as that step left it: no later step writes it,
      and the exit trap only ends the held RESTORE.sh run. Read the failure first (`RESULT.txt`, the
      step's `.err` and `.out` files, `held.out`, `drive-at-fail/`), and keep the cartridge
      unwritten until it is understood. Then:
  - On a FAIL in 1c or 2, `catalog-at-fail.db` and tape 1 are the evidence.
  - On a FAIL in 2 to 5 the held run is ended, and its scratch directory under `<work>/scratch`
    stays, holding encrypted slices only. Remove it once it is read.
  - A FAIL in 4a after the filemark leaves the cartridge with File 0 EMPTY: tape 1's data is
    gone, and the run cannot be resumed. Start again from 1a in a fresh home.
- [ ] Afterwards the cartridge holds `SPN-0002` (`spn-span` whole), owned by the throwaway home.
      Return it to the test pool from that home, with the same binary:

```bash
tapectl --home "$TAPECTL_HOME" snapshot mark-reclaimable spn-span --version 1 --force
tapectl --home "$TAPECTL_HOME" volume retire SPN-0002
tapectl --home "$TAPECTL_HOME" cartridge erase --device /dev/tape/by-id/scsi-HUJ808A5L4-nst --confirm-label SPN-0002
```

  Then remove `--work` (the copy and the restored tree), and start the drive-poll timer.

The run directory (`/scratch/tapectl-span/spn-<UTC>-<serial>/`, or under `~/.cache/tapectl/span`
where there is no `/scratch`) keeps every command's output, the catalog after each tape, both
manifests, and the drive's state before and after every step, as for 1.6.0's run.

## After a pass

- [ ] Add the run to `docs/runs/` with the numbers above re-measured.
- [ ] If anything differs from the records here, that difference is the finding: capture
      `sg_logs --page=0x02,0x03,0x0c --maxlen=65532 <sg>` (never 0x2E by hand),
      `mt status`, and `dmesg` since the load, and file it with the command that produced
      it. Silent workarounds defeat the purpose.
