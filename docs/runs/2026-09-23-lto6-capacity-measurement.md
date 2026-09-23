# 2026-09-23: MAM capacity unit and feed-rate-dependent capacity on the real HP LTO-6

Drive: HP Ultrium 6-SCSI, serial HUJ808A5L4, fw 35GD, via `/dev/tape/by-id/scsi-HUJ808A5L4-nst`.
Cartridge: the CTO's expendable test cartridge, FUJIFILM LTO-6, MAM medium serial
EW7VWMVKF6. The CTO authorized overwriting it. Raw captures are in `/scratch/tapectl-lto6/unit182-20260923-021914/`, with `fast/`
for run 2. Hardware compression was OFF (mode page 0x0f `DCE=0`). Block size 512 KiB.
Before each run the tape was truncated at BOT (`rewind; weof 1; rewind`), so each run
writes from BOT.

Each run wrote exactly 21,474,836,480 bytes (20 GiB) of incompressible data
(AES-128-CTR keystream).

| | run 1 | run 2 |
|---|---|---|
| source | `openssl enc` pipe | 2 GiB file in `/dev/shm`, written 10 times |
| host feed rate (dd) | **94.8 MB/s** | **151 MB/s** |
| page 0x17 "Total used native capacity [MB]": before, after, change | 23, 31,874, **+31,851** | 0, 21,607, **+21,607** |
| MAM "Remaining capacity in partition [MiB]": before, after, change | 2,499,053, 2,468,655, **-30,398** | 2,499,053, 2,478,446, **-20,607** |
| MAM "Total MiB written in current/last load": change | +20,494 | +20,499 |
| native capacity used / data written | **1.48** | **1.006** |

## #182: the MAM capacity attribute IS in MiB (2^20)

The drive keeps two counters of the same physical quantity in two units. In run 1,
31,874 MB decimal is 30,397.5 MiB, and MAM remaining fell by 30,398. In run 2, 21,607
MB is 20,606.1 MiB, and MAM remaining fell by 20,607. Both runs agree exactly, in
opposite directions from the rounding. Had the MAM attribute been in MB, it would have
fallen by 31,851 and 21,607. `sg_read_attr`'s `[MiB]` label is correct, and so is
the `1024 * 1024` multiplier in `src/tape/mam.rs`.

So `Maximum capacity in partition` = 2,499,053 MiB = 2.620 x 10^12 bytes of NATIVE
capacity. That exceeds the marketed 2.5 TB; the arithmetic that suggested MB in #182
was a coincidence.

The "MiB written in current load" counter is also true MiB: +20,494 and +20,499 against
20,480 MiB written, where +21,475 would have meant MB.

## New: how much a cartridge holds depends on the host feed rate

The MAM capacity figures count tape consumed, not data stored. At 94.8 MB/s the drive
used 1.48 bytes of native capacity per byte of data; at 151 MB/s, 1.006. Page 0x17's
data-sets-written count is about the same for both runs, so the extra tape in run 1
was not extra data sets.

Extrapolated, a cartridge fed at about 95 MB/s would reach end of tape at roughly
**1.77 TB** of data, not 2.5 TB. The pre-flight capacity gate plans against the
generation table's 2.5 TB (ADR-0010), so a slow write would hit a real EOT about 30%
early. That is a clean abort to an unsealed tape (v2), so no data is lost, but hours of
writing and the cartridge's session are.

Only the two operating points above are measured. The mechanism is not known; possibly
the gaps the drive leaves when it stops and restarts because the host cannot keep up.
tapectl's own `volume write` feed rate on this drive has not been measured.

## Also corrected

- The 2026-09-10 journal's hypothesis that MAM remaining capacity "refreshes lazily" is
  contradicted: in run 1 it had already moved when read at EOD, before any rewind.
- TapeAlert (0x2E) read-to-clear is still unanswered: every flag was 0 throughout.
