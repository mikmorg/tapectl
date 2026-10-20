# Logical Block Protection on the HP LTO-6: what it is, what Linux does with it, what it would check

*Research for #388, asked for by the CTO on 2026-10-16. Research only: no product code
changed, no tape or SCSI device opened, nothing run on a drive. The prototype (§8) and the
ruling (§9) come later. Author: research agent, 2026-10-16. §8.1 (mhvtl) and §8.2 (the HP
drive) were added on 2026-10-20, after the prototype ran; their facts are measured.*

Facts are tagged **DOC** (a vendor or standards document, cited by section or page),
**CODE** (source read in this repository, in Linux v6.8 / master, or in mhvtl at `6458223`),
**OBSERVED** (read today from this VM's files or sysfs, never a device node), **INFERRED**
(reasoned from the above) and **NEEDS DRIVE** (only the HP drive can settle it; §8 is the
test). Before §8.2, nothing here is MEASURED on a drive.

## 1. Summary

1. **The issue says CRC32C; this drive can only do Reed-Solomon.** CRC32C arrived with LTO-7
   (DOC: IBM-06 §0.1.3 "FCR 3317 - LTO7 LBP add support for CRC32C", §4.9.1.1). HP's LTO-6
   guide lists one method, `1` = Reed-Solomon (HP-V3 p.104). The host CRC is the ECMA-319
   GF(256) code, not an SSE4.2 instruction. Whether `02h` is refused is NEEDS DRIVE.
2. **The drive supports LBP and it is off.** The 2026-09-10 journal captured the page: current
   `4a f0 00 04 00 00 00 00`, changeable `4a f0 00 04 ff 3f c0 00`. HP's page is 8 bytes;
   IBM's (and mhvtl's) is about 32. The setting is not saved (HP: MODE SELECT `SP`
   unsupported): re-arm it every session; a crash leaves it armed.
3. **Linux `st` has no LBP code and needs none for the data.** The CRC rides in the block:
   `st` block size 524292, 524292-byte writes (INFERRED, NEEDS DRIVE). Setting the page
   needs SG, which `threat-model.md` §2 says tapectl never uses for MODE SELECT.
4. **The on-tape logical format should not change.** The drive stores the CRC on every block
   anyway and strips it when LBP_R is 0 (DOC: IBM-01 §4.5.2, IBM-06 §4.9.2). RESTORE.sh
   should see the same 524288-byte blocks. HP's guide is silent; NEEDS DRIVE (§4).
5. **mhvtl emulates LBP** (Reed-Solomon on TD5/TD6, CRC32C on TD7 up). TD6 is the faithful
   stand-in. One gap: it stores the bad block, then rejects it (§5). Mixed libraries are fine
   at the medium; tapes with and without LBP are indistinguishable (§6).
6. **LBP adds a write-time host-to-drive check and a read-time drive-to-host check.** It does
   not prove the tape reads cold, count corrected errors, find later rot, or catch a dropped
   or reordered block. It could make the full readback cheaper to defer, not unnecessary (§7).
7. **The issue's premise is stale.** A write's confirm has been quick by default since #387
   (ADR-0012, 2026-10-06 item 1). LBP would close the write-time hop that ruling gave up.

---

## 2. What LBP is (Q1)

LBP is set on the **Control Data Protection** mode page, MP `0Ah` subpage `F0h`, policy per
I_T nexus (DOC: IBM-06 Table 12, §6.6.9; HP-V3 p.104). The layout this drive speaks:

| Byte | Field | Notes |
|---|---|---|
| 0-3 | PS, SPF=1, PAGE CODE `0Ah`; SUBPAGE `F0h`; PAGE LENGTH | reads `4A F0`; length HP `0004h` (8 bytes in all), IBM `001Ch` |
| 4 | LBP METHOD | `00h` none, `01h` Reed-Solomon CRC (ECMA-319), `02h` CRC32C |
| 5 | LBP INFORMATION LENGTH (bits 5:0) | `04h` once a method is set |
| 6 | LBP_W (7), LBP_R (6), RBDP (5) | W: host sends the CRC on writes. R: drive returns it on reads. RBDP not changeable |

- HP lists method `1` only. IBM-06 Table 337 adds `02h` for LTO-7; FreeBSD `sa(4)` (2022)
  quotes all three "as of SSC-5r03" (DOC, secondary). The captured mask shows the method byte
  changeable, which does not mean `02h` is accepted. Other signs of support: INQUIRY
  `PROTECT=1` (HP-V3 p.39); READ BLOCK LIMITS maximum falls to `FFFFFBh` while LBP is on
  (p.149); a failed check is sense `10h/01h` "Logical block guard check failed" (error table).
- **Reed-Solomon (1):** (N, N-4) over GF(256), P(x) = x^8+x^4+x^3+x^2+1,
  G(x) = x^4+201x^3+246x^2+201x+1. Seed 0, no inversion, table-driven; IBM's sample appends it
  **big-endian** (IBM-06 Annex D.1). HP gives no byte order: NEEDS DRIVE. mhvtl has a runtime
  switch `lbp_rscrc_be`, default big-endian (`usr/vtllib.c:70`).
- **CRC32C (2, LTO-7+):** Castagnoli, seed `0xFFFFFFFF`, final inversion, no 32-bit padding,
  appended **little-endian** in IBM's sample (Annex D.2). The drive converts it to the
  Reed-Solomon form for the medium (IBM-06 §6.6.9).
- **Generations:** first documented for LTO-5 (IBM-01 §0.1.1, "FCR 3208"). IBM Storage Protect:
  "At the LTO-5 level, LBP is supported only on IBM LTO-5. Starting with LTO-6, LBP is
  supported on all LTO drives" (IBM-SP, secondary). HP-V3 is the first edition (October 2012)
  and the drive runs `35GD`, so the captured mask is the better evidence. Volume 2 has no LBP text.

## 3. Linux `st`, the HBA, and tapectl's tape layer (Q2)

### 3.1 What `st` does

- **No LBP code.** The only `protect` match in `drivers/scsi/st.c` (v6.8 and master) is the
  write-protect message; `include/uapi/linux/mtio.h` has no LBP ioctl (CODE: grep).
- **The CRC is part of the block.** In fixed mode, n is the block descriptor's BLOCK LENGTH and
  the data is n - x bytes, x the information length (DOC: IBM-06 Table 15). The wire block is
  524288 + 4 = **524292**; the medium block is 524288.
- **`MTSETBLK` sends a block descriptor and no pages** (`st.c:2894-2945`), so it does not
  disturb page `0Ah/F0h`; sizes outside the READ BLOCK LIMITS read at open are refused
  (`st.c:1108-1131`, `2901-2907`). A fixed-mode `write()` must be whole blocks (`st.c:1645`).
- INFERRED, NEEDS DRIVE: `MTSETBLK 524292` plus 524292-byte `write()`s with the CRC last is
  enough; `st` forwards the bytes. This corrects the 2026-07-21 research note (§C), which
  concluded a custom SG_IO engine was needed; its "half-enabling breaks every write" holds
  only when the payload lacks the CRC (IBM-06 §4.9.4 NOTE 2).

### 3.2 HBA and buffer limits

- `st` sizes its buffer from `queue_max_segments` (capped at 256) and a page order up to 6
  (`st.c:3898-3940`, `4265-4267`). A block cannot be split, so the request limit must reach
  1025 sectors: **a layer capped at exactly 512 KiB per command would refuse 524292.**
- OBSERVED on vm-desk1: `virtio_scsi` `sg_tablesize=126`, `mhvtl` host `128`; the mhvtl module
  sets `.max_sectors = 4096` (`kernel/mhvtl.c:375`). The real drive took 4 MiB writes at
  ~114 MiB/s through virtio-scsi on 2026-09-10 (journal Phase A).
- **home2 is unmeasured** (LSI SAS2008 `mpt3sas`, kernel 4.19). `docs/lto6-drive-passthrough.md`
  records `sg_tablesize=128`, `max_sectors` not exposed, and a ~4 MB headroom that is its own
  inference. I found no write above 512 KiB recorded on home2. NEEDS DRIVE, on home2.
- Direct I/O is on (`try direct i/o: yes (alignment 4 B)`), but `sgl_map_user_pages` returns
  `ENOMEM` when the buffer spans more pages than the table (`st.c:4879`), and `st` then buffers
  (`st.c:1560-1566`). 512 KiB already spans 128-129 pages against a table of 126-128, so the
  fallback may be in force today; +4 only removes the page-aligned case on a 128-entry table.
  A copy per block, not a failure. INFERRED.

### 3.3 What tapectl does today, and what would change

Today (CODE): fixed 512 KiB (`DEFAULT_BLOCK_SIZE`, `cli/volume.rs:13` and copies; a
`block_size` config key is refused, `config.rs:1778`). `TapeDevice::open` sends `MTSETBLK` every
time (`tape/ioctl.rs:195-231`); `TapeStore::open` then rewinds and sends `MTCOMPRESSION 0`
(`store.rs:1636-1646`). `write_stream` (`ioctl.rs:325`) zero-pads the last block and does one
`write_all` of `bs` bytes per block, then a filemark; `read_file_streaming` (`:389`) and
`read_file_head` (`:431`) `read(2)` a `bs` buffer to the filemark. All sits behind the
`TapeOps` trait (`store.rs:1392`).

The change stays inside `TapeDevice`; Layout, `pad_to_blocks`, the front index and RESTORE.sh
keep seeing 512 KiB blocks:

1. **Open:** set the page, then `set_block_size(bs + 4)`. In that order: with the page on and
   `st` still at 524288 the drive would read 524284 data + 4 CRC.
2. **Write:** buffer `bs + 4`, CRC over the padded 524288 bytes, append, write. Compute it in
   the hasher stage so one pass over one buffer serves sha256 and CRC.
3. **Read:** read `bs + 4`, check, strip, hand on `bs`. `FakeTape` and `MemStore` need a CRC
   model for an injected-bad-CRC test; `mtget_journal` and `drive status` expect 524288.
4. **Failures:** `st` probably returns plain `EIO` for a guard-check failure (INFERRED; I did not
   trace `st_chk_result`); the sense is not visible to `write(2)`. Telling a corrupted path
   from a bad medium needs the kernel log or SG.
   Today any non-`ENOSPC` write error leaves a resumable `interrupted` session (#408).

### 3.4 The SG boundary and the volatile page

- `threat-model.md` §2: SG answers three read-only questions through `sg_inq`, `sg_logs`,
  `sg_read_attr`; tapectl "never sends ... a MODE SELECT ... or a raw CDB".
  `tests/sg_boundary.rs` pins that list, so even a MODE SENSE via `sg_modes` is a change.
- The design review's shape (`docs/audits/2026-10-15-design-review.md`: operator sets it once,
  tapectl checks) fails: the page is not saved (HP-V3 `PS (0)`; IBM-06 Table 337 `PS 0b`) and
  is lost on reset or power cycle, so the check would run before every write.
- Shapes for the CTO: **(A)** the operator runs `sg_wr_mode` before each session and tapectl
  verifies with MODE SENSE (one new read-only tool, plus a chore that can be forgotten); or
  **(B)** tapectl sets the page (a MODE SELECT over SG, a deliberate widening of §2). Both
  need the page cleared on every exit (§4).

## 4. Does the on-tape logical format change? (Q3)

**No, by the specs. Confirm on the drive before relying on it.**

- IBM-01 §4.5.2 / IBM-06 §4.9.2: the 4-byte Reed-Solomon CRC "is written to the medium with
  each logical block" as a **format specific symbol**. The drive generates it when the method
  is 0 or LBP_W is 0. It reads, validates and removes it before transfer when the method is 0
  or LBP_R is 0. IBM-06 §4.9.5: the CRC is "validated by the device server as logical blocks
  are processed regardless of the logical block protection settings". (HP-V3 documents the
  page but not this mechanism.)
- So the medium holds the same bytes with LBP on or off, and capacity use is the same. A
  non-LBP reader gets the 524288 data bytes. INFERRED for RESTORE.sh: it sends
  `mt setblk 524288` and `dd bs=512k` (`volume-format-v2.md` §6), never touches the mode page,
  and on a drive at power-on state (LBP off) reads an LBP-written tape as any other.
  `pad_to_blocks` and the trim-to-`size_bytes` contract are untouched.
- **What would make it a format change:** if the HP drive recorded 524292-byte records,
  `dd bs=512k` at block size 524288 would fail on every block. That breaks the heir path and
  means a `requires` entry, a minor version (the 1.x on-tape-bytes rule) and a RESTORE.sh re-pin
  (`tests/on_tape_golden.rs`): a CTO decision, never an agent's. Prototype step 5 is the test.
  Shrinking the data block to 524284 to keep the wire block at 512 KiB is a format change too.
- **A risk the format test does not cover.** The page lasts until the drive resets. If tapectl
  dies with LBP on and an operator then runs RESTORE.sh on that drive, the drive reads
  `setblk 524288` as 524284 + 4 and every read mismatches. tapectl would have to clear the page
  on every exit; making RESTORE.sh check it would add `sg3-utils` to the heir path (a re-pin).

## 5. Does mhvtl emulate LBP? (Q4)

**Yes** (CODE: `markh794/mhvtl` at `6458223`, 2026-10-16). Its ChangeLog: "Initial support for
Data Integrity Validation (via Logical Block Protection) - IBM LTO6, LTO7 & LTO8" (v1.7, 2022).

- **Page:** `usr/mode.c` `add_mode_control_data_protection` adds MP `0Ah/F0h`, IBM-sized (not
  HP's 8 bytes), default method 0, changeable bitmap `03/07/c0`. The MODE SELECT arm in
  `usr/ssc.c` (~1222) calls `set_lbp` and `update_logical_block_protection`, which latch
  method, LBP_W and LBP_R from bytes 4-6 (ILLEGAL REQUEST if the personality has no LBP).
  `usr/pm/ult3580_pm.c`: TD5 and TD6 `LBP_RSCRC`; TD7 up (`init_ult3580_lbp`) `LBP_CRC32C`;
  IBM personalities only.
- **Write/read:** `usr/mhvtl_io.c` `writeBlock` and `writeBlock_*` strip 4 bytes and call
  `verify_lbp_crc`; a mismatch answers HARDWARE ERROR `E_LOGICAL_BLOCK_GUARD_FAILED`
  (`0x1001` = 10h/01h, `include/common/mhvtl_scsi.h:190`). `complete_read_6` (`usr/ssc.c`)
  passes the method to `readBlock` only when LBP_R is set; `readBlock` recomputes the CRC and
  appends it (the stored block has none). Tests: `scripts/test_lbp.sh`, `tests/test_crc.c`
  (Reed-Solomon vectors per ECMA-319, CRC32C per RFC 3720 B.4).
- **OBSERVED:** `/usr/bin/vtltape` here is a source build of 2026-04-10 (`dpkg -s mhvtl` finds
  no package; module 0.18.42 via DKMS). Its strings include `lbp_rscrc_be` and "LBP mis-compare
  on write", so the installed build has this code; the commit is unknown. The gate's TD8
  emulation speaks CRC32C, which this drive cannot: **a TD6 emulation is the faithful
  stand-in** (`nst1`/`nst2` per CLAUDE.md; re-check by-id). None was opened here.

**Where mhvtl differs from the drive.**
- **Gap 1:** `writeBlock_nocomp` calls `write_tape_block` (`mhvtl_io.c:717`) before
  `verify_lbp_crc` (`:719-727`). The bad block lands on the virtual tape, then the command
  fails. IBM-06 §4.9.4: the drive stops "prior to writing the failed logical block". A mhvtl
  test shows the error path, not the head position (CODE, not run).
- **Gap 2:** `scripts/test_lbp.sh` sends a `1Ch`-length page; its `sg_wr_mode --contents`
  string would fail on the HP drive.
- **Possible gap 3 (CRC32C only; read, not run):** the write check accepts the CRC
  little-endian on a little-endian host, like IBM's sample, but `readBlock` appends it
  big-endian (`put_unaligned_be32`). Irrelevant to Reed-Solomon, big-endian both ways.
- mhvtl cannot say how the real drive orders the CRC or whether HP accepts `02h`.

## 6. Mixed libraries and drives without LBP (Q5)

- **Medium:** tapes with and without LBP are indistinguishable (§4); the write method "does not
  need to be the same as the method ... used to read that same block" (IBM-06 §4.9.1). A tape
  written by tapectl 1.1.x can be read with LBP_R on; that buys the read hop only.
- **Record:** nothing on the medium says the host checked at write time. tapectl would need
  its own flag (a catalog column; a note in File 0 or the seal marker would move golden bytes:
  a CTO call). Without it `audit` cannot tell which volumes had the check.
- **A drive without LBP** (page absent or zero mask, `PROTECT=0`, mhvtl's non-IBM
  personalities): tapectl must probe and fall back to today's checks; warn or refuse is policy
  (§9). A cartridge started on one drive and filled on another has mixed assurance: a
  per-session fact, not a per-volume one (INFERRED). IBM Storage Protect keeps LBP on for a
  filling volume that already holds protected data and wants every drive in a library to
  support it for writes (IBM-SP).

## 7. What LBP covers and what it does not (Q6)

### 7.1 The checks tapectl has now (CODE, traced through the callers)

- **L1, `validate`:** size-only by default, a full hash under `--prewrite-hash`
  (`session.rs:13-14`). **L2, execute's inline hash.** `session.rs:2752-2790` calls `pipeline::write_verified`
  (`pipeline.rs:284`): a reader thread feeds a hasher thread, which feeds `store.execute` on
  the calling thread (`TapeStore::execute`, `store.rs:1781`, then `write_stream`). The hasher
  holds the last block until the hash equals the front-index value; a mismatch aborts to an
  unsealed tape. So **every content entry is hashed before `write(2)` sees it.** Limits: nothing
  after `write(2)`; the zero padding is not hashed; the seal marker is written later by
  `ReadyToSeal::seal` (`session.rs:2195`) with no inline hash; confirm checks it.
- **L3, confirm.** `SealedPending::confirm` (`session.rs:2246`) calls `confirm_with`
  (`store.rs:502`) then `chain_walk` (`store.rs:827`). The default tier (`Tier::write_confirm`,
  `store.rs:68`) is navigable: File 3 and the seal marker only. `--full-confirm` selects
  `Tier::Integrity`: every content file is read back (`TapeStore::read_file`, `store.rs:1798`),
  trimmed to its claimed size and sha256'd against the front index. `volume verify`
  (`volume/write.rs:4484`) runs the same walk at `Integrity` unless `--quick`; `restore`
  hashes each slice's ciphertext as it streams off the tape (`volume/restore.rs:1899-1915`);
  `audit` (`no_full_verify`) names sealed volumes with no passed full readback.
  The drive's own read-after-write, ECC and rewrite sit between its buffer and the medium;
  tapectl trusts them.

### 7.2 What each check covers

| Hop | L1/L2 | LBP_W | LBP_R | Full readback + sha256 |
|---|---|---|---|---|
| staged file on disk to tapectl's buffer | yes | no | no | yes |
| buffer to `st`, HBA, cable, drive buffer | no | **yes** (from where the CRC is computed) | no | yes |
| drive buffer to medium | no | no | no | yes (reads what the medium holds) |
| the medium later (shelf rot) | no | no | no | only when run again |
| medium to drive to host, on a read | n/a | no | **yes** | yes |
| reads cold in this drive; corrected-error counts (#421) | no | no | no | yes |
| a block dropped, repeated or reordered | no | no | no | yes (the file sha256 changes) |

### 7.3 Why LBP is not the readback

- With LBP_W off the drive stamps a valid CRC on whatever it received. Host-side corruption
  between buffer and drive is invisible to the drive and to every later read. Only the host's
  own pre-corruption CRC, or a hash after a readback, catches it. That is the one hop LBP adds.
- Per-block CRCs prove each block intact, not that the stream has the right blocks in order.
  A CRC computed after the hash would seal in corruption between the two (§3.3 item 2).
- ADR-0007 rejected a confirm that depends on LBP ("unreachable through the Linux data path");
  §3 weakens "unreachable". `volume-format-v2.md` §5 keeps LBP evidence supplementary, and
  ADR-0001 requires recorded strength to match what ran, so LBP needs its own evidence class.
  Cost: IBM-SP warns of added processor use; a table-driven CRC against 114 MiB/s is INFERRED
  small.

### 7.4 The tradeoff, for the CTO (no ruling here)

| Option | Gains | Leaves open |
|---|---|---|
| **a. Nothing** (today) | no new SG surface | path errors found only by a full verify |
| **b. LBP extra, quick confirm as now** | a path error found at the first bad block, at write time | tape not proven to read cold; `no_full_verify` still applies; SG widening |
| **c. LBP plus `--full-confirm`** | belt and braces; per-block localisation | no time saved; same SG widening |
| **d. LBP, full readback only as scheduled `volume verify`** | readback becomes a scheduled medium test | as b, and the staged slices are gone before the first cold read |

LBP can make the full readback cheaper to defer. It cannot make it optional while the
readback is the only thing that proves the cartridge reads. The design review said the same.

## 8. Prototype plan for the real drive (Q7) -- dry-run on mhvtl, then run on the drive (§8.2)

Test cartridge `EW7VWMVKF6` only; resolve nodes by serial (`HUJ808A5L4`) via
`/dev/tape/by-id/`, never by number. This is the shape of the plan, to be checked against the
template read in step 1. **Nothing below was run, or could be run, for this note.**

```bash
SG=/dev/sgX     # the drive's sg node, found by serial
NST=/dev/nstY   # the non-rewinding node of the same drive

# 1. Read only: current / changeable / default / saved, and the support bits.
for pc in 0 1 2 3; do sg_modes --page=0x0a,0xf0 --control=$pc --hex "$SG"; done
sg_vpd --page=ei "$SG"                      # Extended INQUIRY, SPT field
sg_raw -r 6 "$SG" 05 00 00 00 00 00         # READ BLOCK LIMITS with LBP off

# 2. Probe method 02h. Expect ILLEGAL REQUEST on an LTO-6. HP applies parameters up to the
#    first error (HP-V3 p.97), so re-read the page after EVERY MODE SELECT, refused or not.
sg_wr_mode --page=0x0a,0xf0 --dbd --contents=0a,f0,00,04,02,04,00 \
           --mask=00,00,00,00,ff,3f,c0 "$SG"
sg_modes --page=0x0a,0xf0 --hex "$SG"

# 3. Enable method 01h with LBP_W and LBP_R, touching only bytes 4-6; then block size.
sg_wr_mode --page=0x0a,0xf0 --dbd --contents=0a,f0,00,04,01,04,c0 \
           --mask=00,00,00,00,ff,3f,c0 "$SG"
sg_modes --page=0x0a,0xf0 --hex "$SG"       # read back
sg_raw -r 6 "$SG" 05 00 00 00 00 00         # expect a maximum of FFFFFB
mt -f "$NST" setblk 524292

# 4. Host tool (throwaway C or Python, not tapectl): turn an N*524288-byte payload into
#    N*524292 bytes, each block followed by its Reed-Solomon CRC (IBM Annex D.1, seed 0,
#    big-endian); check it against mhvtl's tests/test_crc.c vectors. nine.bin = 8 good blocks
#    and a 9th whose CRC has one byte flipped (one dd: close would write a filemark).
mt -f "$NST" rewind
dd if=nine.bin of="$NST" bs=524292 count=9           # expect EIO at the 9th block, or later:
mt -f "$NST" status                                  # the report may be deferred (IBM-06 4.9.4);
                                                     # dd exiting 0 proves nothing. Block 8, not 9
dmesg | tail                                         # expect sense 4 / 10h 01h

# 5. Heir proof: LBP off, original block size, plain dd.
sg_wr_mode --page=0x0a,0xf0 --dbd --contents=0a,f0,00,04,00,00,00 \
           --mask=00,00,00,00,ff,3f,c0 "$SG"
mt -f "$NST" setblk 524288 && mt -f "$NST" rewind
dd if="$NST" of=back.bin bs=512k count=8 && cmp back.bin payload8.bin
mt -f "$NST" setblk 0 && mt -f "$NST" rewind         # variable mode: record length?
dd if="$NST" of=/dev/null bs=1M count=1              # expect 524288 bytes, not 524292

# 6. LBP_R read: re-enable method 1 W+R, setblk 524292, read 8 blocks, verify each CRC on the
#    host; then LBP_R off with setblk 524292, to see how st reports the mismatch.
# 7. Persistence: reopen NST, offline and reload, a power cycle if the lab allows: re-read page.
# 8. End state, always: method 0, LBP_W and LBP_R 0, setblk 524288, rewind.
```

Whether `--contents` starts `0a` or `4a`, and how `--mask` treats the page length, need
`--verbose` on the drive. Do not reuse mhvtl's `1Ch`-length string.

**What to measure.**
1. Is method `02h` refused? (Can CRC32C ever apply to this drive?)
2. The Reed-Solomon byte order the drive accepts (big-endian per IBM D.1).
3. Does a bad CRC fail with sense `4/10h/01h`, and does the head stay before the block?
4. Does an LBP-written block read back as 524288 bytes with LBP off, byte-identical? Variable
   record length 524288 or 524292?
5. READ BLOCK LIMITS on and off; page persistence across reopen, reload, power cycle.
6. Does `setblk 524292` plus a 524292-byte `write()` work at all, on vm-desk1 **and on home2's
   `mpt3sas`** (§3.2)? Does direct I/O survive? Throughput and host CPU for 2 GiB, on and off.
7. A **mhvtl TD6** dry run of steps 3-6 first, compared with the drive.

### 8.1 The prototype as a script, and its mhvtl dry run (2026-10-20)

The plan above is now a script, `docs/research/lbp-prototype/lbp-prototype.sh`, with its host tool,
`lbp.py`. The tool's Reed-Solomon CRC uses the table from mhvtl's `usr/utils/reed-solomon.c` (IBM
Annex D, seed 0, appended big-endian), and its `selftest` reproduces the two answers mhvtl's
`usr/utils/validate_crc.c` asserts: `0x733D4DCA`, then `0x754ED37E` chained. How the script works:
- It never hardcodes the page. It reads the drive's own Control Data Protection page, changes only
  bytes 4-6, clears the PS bit, and writes the page back. This settles the `0a`/`4a` question
  above: the HP LTO-6 reports `4a f0 00 04 …` (length 04h, SPF set), mhvtl `0a f0 00 1c …`.
- It refuses unless the chip's medium serial is the one named, the drive-poll timer is stopped,
  and the harness tape lock is free.
- On every exit it restores method 0, LBP off, the 512 KiB block size and a rewind.
- `--block N` sets the data bytes per block (default 524288).

**The HP LTO-6, read only** (from the #420 profile run on home2, 2026-10-20, firmware 35GD):
`sg_modes --page=0x0a,0xf0` answers with method 00h, info length 00h, flags 00h. The page exists
and protection is off.

**The mhvtl TD6 dry run (vm-desk1, drive A4):**
- **At 524292 bytes per block nothing moves.** Every write and read fails at once with EBUSY, which
  st returns when "the request was too large". mhvtl's adapter reports `sg_tablesize` 128, and
  128 pages of 4 KiB is exactly 512 KiB.
- **home2's path is different.** Its LSI SAS2008 (the `mpt3sas` module, which names SAS2 hosts
  `mpt2sas`; firmware 20.00.07.00, `sg_tablesize` also 128)
  accepted 1 MiB blocks in the same profile run, so 524292 is expected to pass there. The real
  drive's run answers measurement 6.
- **At 262144 + 4 bytes per block, all of it worked on mhvtl:**
  - Method 01h with LBP_W and LBP_R armed. mhvtl also accepted method 02h on a TD6, so whether an
    LTO-6 refuses CRC32C (measurement 1) is the real drive's to answer.
  - The 8 protected blocks were written, and the 9th, with one CRC bit flipped, was refused with
    EIO ("8+0 records out"): measurement 3, emulated.
  - With LBP off, plain `dd` read the 8 blocks back byte-identical. In variable mode one record
    read as 262144 bytes, so the CRC is not part of the medium's logical block: measurement 4 and
    the heir's view, emulated.
  - With LBP_R on, the drive returned a CRC with every block, and each matched the host's:
    measurement 2, emulated.
  - With LBP off and the block size still N+4, a read returned 0 bytes and no error. That is st's
    fixed-mode length mismatch: a reader that forgets to reset the block size gets nothing, not
    wrong data.
  - The armed page survived closing and reopening the device. It is volatile only across a reset
    or a reload, as §3.4 says.

### 8.2 On the HP LTO-6 (home2, 2026-10-20)

The same script ran on home2's HP drive, on EW7VWMVKF6, at the default 524288. The run record is
`docs/runs/2026-10-20-lto6-profile-and-lbp.md`.
- **Measurement 1: the drive refuses method 02h (CRC32C)** with ILLEGAL REQUEST, and the page
  stays at method 0. Method 01h (Reed-Solomon) with LBP_W and LBP_R was accepted. On an LTO-6 the
  design is Reed-Solomon only, which is question 1.
- **Measurement 6: the drive takes 524,292-byte blocks.** `setblk 524292`, writes and reads all
  worked through home2's SAS2008. mhvtl's 512 KiB cap does not apply there.
- **Measurement 3: a bad CRC is refused, late.** One `dd` wrote 8 good blocks and a 9th with one
  CRC bit flipped. It exited 0 with "9+0 records out" after 19.6 s. The next command
  (`mt status`) got EIO. A later read found 8 blocks in File 0, then a filemark, so the bad block
  never reached the medium.
  - st writes behind, so the refusal arrives on a later operation, not on the `write(2)` that
    carried the block.
  - The 19.6 s, against 0.02 s to read the same bytes back, is most likely the drive retrying the
    bad block. That cost is paid only when bad data is sent.
- **Measurement 4 and the heir's view: settled.** With LBP off, plain `dd` at 512 KiB read the
  8 blocks back byte-identical. In variable mode one record read as 524,288 bytes, so the CRC is
  not part of the medium's logical block. A tape written with LBP reads like any other: no
  RESTORE.sh change, and question 4's first half does not arise.
- **Measurement 2: LBP_R works.** Every CRC the drive returned with the 8 blocks matched the
  host's.
- **A reader that forgets the block size gets EIO here.** With LBP off and the block size still
  524,292, the read failed with EIO. On mhvtl it returned 0 bytes. Either way it is a refusal,
  never wrong data.
- **The armed page survived a close and reopen,** as on mhvtl. The script's exit trap put it back
  to method 0, and the page read back so.
- **Not measured: READ BLOCK LIMITS with LBP on.** The script parses `sg_raw`'s output, and on
  home2's sg3-utils 1.44 it printed nothing, so the expected maximum of FFFFFB is unconfirmed.
  With LBP off, `sg_read_block_limits` read 1 to 16,777,215 bytes.

## 9. Open questions for the CTO

1. **Reed-Solomon only?** Is an LTO-6-only design (method 1) acceptable, with CRC32C left for
   the day an LTO-7+ drive is bought?
2. **The SG boundary:** (A) operator sets the page, tapectl checks with `sg_modes`; or (B)
   tapectl sets it. Either reverses `threat-model.md` §2 and changes `tests/sg_boundary.rs`.
3. **The heir path:** is "tapectl clears the page on every exit" enough against a crash that
   leaves LBP on, or must RESTORE.sh become LBP-aware (a re-pin and a format decision)?
4. **If the drive records 524292-byte records:** drop LBP, or a minor version with a
   RESTORE.sh change? **A drive without LBP, or a page that cannot be set:** refuse, warn, or
   fall back? Does `audit` count a volume with no write-time check as a finding?
5. **What should LBP buy?** §7.4: a, b, c or d. May an LBP write with a quick confirm count as
   a write-time end-to-end check for `no_full_verify`, and where is that recorded?
6. **Where does the prototype run?** home2 holds the HBA limit that matters; vm-desk1 has the
   measured headroom. (Answered: it ran on home2, §8.2.)
7. **Where does tapectl see a CRC refusal?** On the HP drive it arrives late, on a later
   operation (§8.2). tapectl ends each file with an immediate filemark (MTWEOFI), which does not
   flush, so a refusal of a slice's last block would surface on the next file's first write, or
   on the seal's synchronous filemark, the only one a session writes. Must an LBP write flush after each slice (MTWEOF), so
   the error lands on the slice that caused it? And must a write session that saw an LBP
   refusal drop the whole session, or retry the slice?

## 10. What I could not find, and sources

**Not found.** SSC-4 / SSC-5 text: `t10.org/cgi-bin/ac.pl` returned a login page for the
drafts, so the fields above come from the IBM and HP manuals and `sa(4)`; T10/07-374r1 is the
originating 2007 proposal and not normative (it uses subpage `80h` and other field names). An
HPE statement that LTO-6 keeps the CRC on the medium regardless of LBP (that is IBM's text;
HP-V3 is silent). HPE firmware notes for `35GD`. The home2 HBA's request limit, and whether
the drive accepts `02h` (§3.2, §8). Any measurement of Reed-Solomon CRC speed in Rust.

**Sources.**
- IBM-01: IBM LTO Ultrium Tape Drive SCSI Reference, GA32-0928-01, 13 Feb 2013 (§0.1.1, §4.5):
  https://public.dhe.ibm.com/systems/support/system_x_pdf/tape_scsi_reference.pdf
- IBM-06: GA32-0928-06, 30 Apr 2024 (§0.1.3, §4.7.1.2, §4.9, §6.6.9, Annex D):
  https://ibm.com/support/pages/system/files/inline-files/%5BAPPROVED%5D%20LTO%20SCSI%20Reference%20GA32-0928-06.pdf
- HP-V3: HP LTO Ultrium 6 Tape Drives Technical Reference Manual, Volume 3: Host Interface
  Guide, Oct 2012 (pp.39, 104, 149; an Oracle-hosted copy, not an HPE publication):
  https://docs.oracle.com/cd/E38452_01/en/LTO6_Vol3_E1_D20/LTO6_Vol3_E1_D20.pdf
  (Volume 2, `.../LTO6_Vol2_E1_D8/LTO6_Vol2_E1_D8.pdf`, has no LBP text.)
- T10/07-374r1, "SSC-3: End-to-end Logical Block Protection", Aug 2007:
  https://www.t10.org/ftp/t10/document.07/07-374r1.pdf
- FreeBSD `sa(4)`: https://cgit.freebsd.org/src/plain/share/man/man4/sa.4
- IBM-SP: IBM Storage Protect, DEFINE DEVCLASS (LTO):
  https://www.ibm.com/docs/en/SSEQVQ_8.1.10/srv.reference/r_cmd_devclass_lto_define.html ;
  https://www.ibm.com/support/knowledgecenter/SSGSG7_7.1.1/com.ibm.itsm.srv.doc/r_lbp_drives.html
- Linux `st`: https://github.com/torvalds/linux/blob/v6.8/drivers/scsi/st.c (and `master`),
  `st_options.h`, `include/uapi/linux/mtio.h`. mhvtl: https://github.com/markh794/mhvtl
  (commit `6458223`). Repository documents are cited inline.
