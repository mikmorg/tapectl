# LTO-6 Hardware Validation Checklist

Rewritten 2026-09-24 (ADR-0012's 2026-09-24 amendment, item 10). This is the procedure
for validating tapectl against a real drive and cartridge, re-runnable for any new drive,
cartridge batch, or host. Every item below has been run on real hardware at least once;
the record for each is named so a re-run can be compared with it. The pre-2026-09
version of this file (v1's "layered EOT recovery" and `/dev/nst0` addressing) is
superseded: Layout v2 has no end-of-tape salvage (ADR-0007), and no device is ever
addressed by number.

**The drive on this VM:** the HP Ultrium 6-SCSI, serial HUJ808A5L4, passed through from
`home2` (`docs/lto6-drive-passthrough.md`), is `/dev/tape/by-id/scsi-HUJ808A5L4-nst`.
mhvtl's drives are `scsi-XYZZY_A*-nst`. `/dev/nstN` numbering moves across reboots and
after module reloads; every command below takes the by-id path. The expendable test
cartridge is FUJIFILM LTO-6, medium serial `EW7VWMVKF6`; it is never a production tape.

## What mhvtl cannot tell you (why this session exists)

- **End of tape.** mhvtl accepts writes past its configured capacity without ENOSPC and
  silently corrupts the overflow (dry-run 2026-07-20). Only a real drive answers where
  host writes stop, and how.
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
- [ ] The mhvtl gate is GREEN on this binary (`TAPECTL_GATE_TAPE=/dev/nst1
      TAPECTL_MHVTL=1 bash scripts/mhvtl-verify-gate.sh`, 39 checks as of #301).
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
- [ ] The heir script alone (no tapectl): `mt rewind; mt fsf 2; dd bs=512k | tr -d '\0' > RESTORE.sh`,
      then `./RESTORE.sh --info`, `--find-envelope --key <key>`, `--restore --key <key> --to <dir>`.
      Every key the tenant holds must open its own leg (#288). This is the lifecycle
      suite's `restore_sh_*` checks, green on hardware.

## End of tape (measured once; re-run only for a new drive or media type)

There is no EOT salvage in Layout v2: a real EOT is a clean abort to an unsealed tape,
and the pre-flight gate (generation table × 0.92 usable) is the capacity defence. What
to measure is where the drive stops host writes, with `scripts/lto6-fill.sh`:

```bash
scripts/lto6-fill.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst --i-will-lose-the-cartridge EW7VWMVKF6
```

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

## After a pass

- [ ] Add the run to `docs/runs/` with the numbers above re-measured.
- [ ] If anything differs from the records here, that difference is the finding: capture
      `sg_logs --page=0x02,0x03,0x0c --maxlen=65532 <sg>` (never 0x2E by hand),
      `mt status`, and `dmesg` since the load, and file it with the command that produced
      it. Silent workarounds defeat the purpose.
