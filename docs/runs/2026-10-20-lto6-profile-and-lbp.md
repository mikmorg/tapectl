# 2026-10-20: the LTO-6 profile (#420) and the LBP prototype (#388) on home2

Two measurements on home2's HP LTO-6, run by an agent as the operator's login on the operator's
go-ahead:
- `scripts/lto-profile.sh`, the full run with the fill to the early warning and the walk past it.
  This is the measurement #420 asked for. #479 built the instrument.
- #388's Logical Block Protection prototype, `docs/research/lbp-prototype/lbp-prototype.sh`.

Afterwards the cartridge was short-erased in a throwaway home and ejected. The production catalog
was not touched.

| | |
|---|---|
| drive | HP Ultrium 6-SCSI, firmware 35GD, serial HUJ808A5L4, `/dev/tape/by-id/scsi-HUJ808A5L4-nst` (nst0, sg13) |
| HBA | LSI SAS2008 (`mpt3sas`), firmware 20.00.07.00 |
| cartridge | the test cartridge EW7VWMVKF6, FUJIFILM LTO-6, 846 m, made 2017-08-24 |
| profile | 03:14:53–07:57:20 UTC, `~/.cache/tapectl/lto6/profile-20261019-221453-EW7VWMVKF6` on home2 |
| LBP prototype | 12:12:57–12:13:29 UTC, `~/.cache/tapectl/lbp/lbp-20261020T121257Z-b524288` on home2 |
| erase, eject | 12:19 UTC: `cartridge erase --confirm-label EW7VWMVKF6` (short) in home d of `p444-20261020T024320Z`, read back "File 0 EMPTY; chip serial EW7VWMVKF6 (unchanged)"; then `mt offline` |
| host | drive-poll timer stopped; the other sessions on vm-desk1 held heavy work until 09:30 UTC |

## The profile

**The early warning.** The first `write(2)` refusal came after **2,502,136,168,448 bytes**: 4,772,446
blocks of 512 KiB, 1.0009 of the marketed 2.5 TB. It came as ENOSPC, and st showed EOD. The
2026-09-24 fill on the same cartridge stopped at 2,501,995,134,976 bytes. The two runs agree to
within 0.006%. The host fed the drive at 158.2 MB/s for 4.39 h, which is LTO-6's native rate.

**After the first refusal, st alternates.** Every accepted write was followed by exactly one
ENOSPC, then the next write was accepted. Over the whole walk the refusals came in runs of at
most 3. A refused `write(2)` returned in 0.02 ms, so it never reached the drive.
- st flags after each refusal: EOD. After each accepted write: EOT.
- **The slice:** 2048 blocks (1 GiB) were all accepted, retrying after each refusal. That took
  8.9 s (120 MB/s).
- **Then a filemark (MTWEOFI), a seal block and a filemark (MTWEOF):** each was accepted on its
  first attempt.
- **The walk past the seal** accepted 207,760 more blocks, 110.0 GB past the first refusal in all.
  It stopped at its cap, never at a hard error. The slowest accepted write took 4.5 s.
- **`close` returned EIO.**

**Room past the warning.**
- The drive's guaranteed minimum: page 0x0c "Minimum native capacity from EW to EOP" says
  **99,008 MB**. It read the same before the fill, at the warning and after.
- The measured room is at least 110.0 GB, because the walk stopped at its cap.
- After the walk, page 0x0c's "native BOP to EOD" read 2,620,446 MB. That equals page 0x17's
  "Total native capacity", so the walk ended at or near the physical end.
- MAM "remaining capacity" read 95,269 MiB at the warning.

**`--emit` proposes** `post_warning_bytes = "99008M"`, the drive's own figure. It keeps
`fill_ceiling = 0.97`. The run supports 0.98, but ADR-0012 item 98 caps the value at 0.97 until a
wear allowance is fitted. Nothing was pasted into home2's config: that is the CTO's call, on #420.

**What the run did NOT prove: the data past the warning reads back.** The readback step failed.
- `LOCATE to block address 4772446 failed: EIO`.
- READ POSITION (`MTIOCPOS`) never answered through st on this host, not even at BOT on the
  device's first open (`setup.lba0` is null), so every position in `result.json` is null.
- The writer's `mtpos()` drops the errno. Nothing records why.

The 1 GiB slice, the seal block and its filemarks were accepted by `write(2)` and never read back.
They cannot be now. The LBP prototype then rewrote the beginning of the tape, and the erase
followed. The alternating pattern, with an instant refusal and EOT changing to EOD, fits st
refusing a write without sending it, so no block was duplicated. But that is the inference the
readback existed to check. This should have been examined before the prototype overwrote the tape.
A readback that spaced by filemarks (`MTFSF`, then `MTBSF`/`MTBSR`) needs no READ POSITION and
could have run then. The fill writer's readback gap is filed against `scripts/lto6-fill-writer.py`
as #487.

**The other figures.**
- Block size: 512 KiB and 1 MiB both ran at 114.5 MiB/s in this test.
- READ BLOCK LIMITS: 1 to 16,777,215 bytes.
- A read past end of data returns no data (EOD semantics PASS).
- Hardware compression was off as found.
- The MAM's "remaining capacity" fell by 2050 MiB across a 2 GiB write. That is the +2 MiB
  over-report the 2026-09-10 session measured.
- The tool's "native capacity used per data byte" line (0.0395) is not a measurement. On this
  drive, page 0x17's "Total used native capacity" did not move during the 2 GiB write, and after
  the fill it read 103,077 MB against 2.6 TB written. The tool marks the unit as not derived.

**Wear.** Over the load, the MAM's life written went from 2,965,535 to 5,458,264 MiB. The drive's
counters across the whole run:
- write retries rose from 72 to 85;
- rewrites rose from 0 to 13;
- unrecovered errors stayed at 0.

## The LBP prototype

Run at the default `--block 524288`, so the protected block was 524,292 bytes. home2's HBA took
it. On mhvtl the same block fails with EBUSY, because mhvtl caps a request at 512 KiB.

| step | result |
|---|---|
| 1. the page | current, default and saved all read `4a f0 00 04 00 00 00 00`; changeable `4a f0 00 04 ff 3f c0 00`. Extended INQUIRY: SPT=1, GRD_CHK=0 |
| 2. method 02h (CRC32C) | **refused**: `sg_wr_mode` "Illegal request"; the page stayed at method 0 |
| 3. method 01h (Reed-Solomon) with LBP_W and LBP_R | accepted; the page read back `… 01 04 c0 00`; `setblk 524292` accepted |
| 4. 8 protected blocks and a 9th with one CRC bit flipped, one `dd` | `dd` **exit 0, "9+0 records out"**, 19.6 s; the next command (`mt status`) got **EIO**. A later read with LBP off found **8 blocks in File 0, then a filemark**: the bad block was refused and not written |
| 5. the heir's view (LBP off, 512 KiB blocks, plain `dd`) | the 8 blocks read back **byte-identical**; in variable mode one record read as **524,288 bytes**, so the CRC is not part of the medium's logical block |
| 6. LBP_R read | 8 blocks of 524,292 bytes; **every CRC the drive returned matches** the host's |
| 6b. LBP off, block size still 524,292 | **EIO** (mhvtl: 0 bytes and no error) |
| 7. close and reopen | the armed page persisted |
| 8. end state | method 0, LBP off, 512 KiB blocks, rewound; the page read back `4a f0 00 04 00 00 00 00` |

**The answer that matters most is in step 4. The drive refuses a bad CRC late, not on the
`write(2)` that carried it.** st buffers writes, so the refusal surfaced as a deferred EIO on the
next operation. The 19.6 s, against 0.02 s to read the same bytes, is most likely the drive
retrying the bad block before refusing it. That cost is paid only when bad data is sent.

READ BLOCK LIMITS with LBP on was not measured. The script's `sg_raw` parsing printed nothing on
home2's sg3-utils 1.44, so the expected maximum of FFFFFB is unconfirmed.

The research note's §8.2 carries these results into the design, with a new question for the CTO
(§9, question 7).
