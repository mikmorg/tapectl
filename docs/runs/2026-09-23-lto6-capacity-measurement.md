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

## Run 3 (issue #323): tapectl's own `volume write` uses native capacity 1:1

Measured the same day, on the same drive and cartridge, with a one-off release build of
master `8007412` (the CTO approved it for this measurement only). The source was 20 x 1 GiB
of incompressible AES-CTR keystream, staged by `stage create` into 6 slices (5 x 4.296 GB +
7.9 KB). It ran in a temp home, never `~/.tapectl`. The tape was truncated at BOT, then
`volume init M323A1 --force` ran (see below), then `volume write M323A1 --device
/dev/tape/by-id/scsi-HUJ808A5L4-nst`. The command exited 0.

| | run 3 (`volume write`) |
|---|---|
| bytes sent to the drive (process `wchar`, all 14 volume files) | 21,488,398,724 |
| host feed rate while writing slices | **56 MB/s, steady** (each 4.296 GB slice took 76-77 s) |
| page 0x0c "Native capacity from BOP to EOD" after the write | 21,487 MB |
| page 0x17 "Total used native capacity [MB]" after the write | 21,487 |
| native capacity used / data written | **1.000** |
| page 0x17 write retries / unrecovered write errors | 2 / 0 |

Both counters come from tapectl's own post-write health sweep: `log_page_journal` rows for
pages 0x0c and 0x17 on the write's contact, decoded offline. No log page was read by hand.

**This contradicts the "slower feed uses more tape" reading of runs 1 and 2.** At 56 MB/s,
well below run 1's 94.8 MB/s, the drive used exactly one byte of native capacity per byte of
data. The better-supported hypothesis is that what costs tape is an *irregular* feed, not a
slow one. Run 1 came from an `openssl enc` pipe that delivers bursts. tapectl's write loop
delivers a steady stream that the drive's speed matching can follow. That is still a
hypothesis: only three operating points exist. For planning, what matters is that on this
drive, **tapectl's own write path fits the generation table's 2.5 TB with margin** (the
cartridge reports 2,620,446 MB native).

### Where the 36 minutes went

| phase | span (catalog timestamps) | duration |
|---|---|---|
| contact open to `writes.started_at` (pre-write checks, which read the staging) | 14:40:14 to 14:48:29 | 495 s |
| writing files 0-12 (envelopes, then slices) | 14:48:29 to 14:54:52 | 383 s, 56 MB/s |
| seal plus read-back confirm | 14:54:52 to 15:16:40 | 1,308 s |
| **total `volume write` wall clock** | | **2,186 s** |

The staging disk reads at 219-333 MB/s (`dd iflag=direct`), so it is not what limits the
write. The streaming loop itself (`tape::ioctl::write_stream`) does NO hashing: it reads
one 512 KiB block from the staged file, writes it to the drive, and repeats. Integrity
work sits only in the phases around it: the pre-write L1 check re-hashes every staged
slice in full, and confirm reads the tape back and hashes it. This VM's CPU has no SHA-NI.
Coreutils `sha256sum` runs at about 127 MB/s on one core, which makes the two hashing
phases CPU-bound candidates. The 56 MB/s streaming rate is not hashing. The unprofiled
hypothesis is that each staging read and tape write are serialized in one thread, with
no double buffering. Extrapolated linearly to a full 2.5 TB cartridge, the same
phases would take about 16 h (pre-write), 12.4 h (write) and 42 h (confirm). That is an
operability question, not a correctness one.

### Also observed

`volume init` on a tape truncated at BOT (a single filemark, then EOD) refused with "the
loaded cartridge's File 0 already identifies a DIFFERENT volume (a present but
unparseable/corrupt File 0)". Refusing without `--force` is correct: ADR-0003 fails closed
on anything that is not provably blank. The wording is wrong, though, because an empty
File 0 is not a different volume or corruption. `--force` was used under the CTO's
authorization for this cartridge.
