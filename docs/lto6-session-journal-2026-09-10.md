# LTO-6 Hardware Validation Session — Journal

**Date:** 2026-09-10
**Operator:** Mike (cartridge handling) + Claude (commands)
**Drive:** HP Ultrium 6-SCSI, fw `35GD`, serial `HUJ808A5L4`
**Media:** FUJIFILM LTO-6, medium serial `EW7VWMVKF6`, 846 m — brand new,
load count 1, 0 MiB written at session start
**Authorization:** "i've inserted a brand new tape. it is yours to do validation
& iterate as much as you need on."

A running log of everything done to the drive and the media, in order. Raw
command output that is too long to inline is kept under
`/scratch/tapectl-lto6-session/recordings/` and named at the point of use.

Passthrough setup (how the drive reaches this VM) is a separate document:
`docs/lto6-drive-passthrough.md`.

---

## Phase 0 — Environment

### Working directories

Everything transient lives on `/scratch` (per the machine's convention that
`/` is reserved for system + backed-up files):

```
/scratch/tapectl-lto6-session/
├── bin/          # session helper scripts
├── src/          # source trees used as archive input
├── restore/      # tapectl restore targets
├── recovery/     # raw-recovery drill (no tapectl) targets
└── recordings/   # raw command output
```

Nothing in this session touches `~/.tapectl` — the real archive DB. The test
archive is built under an explicit `--home` (Phase 3).

### The device guard

mhvtl is loaded in this guest and owns `/dev/st0`–`/dev/st2`; the real drive
landed at `/dev/st3` only because mhvtl enumerated first. That ordering is not
a guarantee — if mhvtl ever fails to load, the real LTO-6 becomes `/dev/nst0`,
which is the hardcoded default in `tests/mhvtl_e2e.rs`,
`scripts/mhvtl-verify-gate.sh` and `scripts/mhvtl-device.sh`.

So no drive command in this session names a node directly. They all resolve it
through `/scratch/tapectl-lto6-session/bin/tape-dev.sh`, which starts from the
serial-based `by-id` symlink and then **independently confirms via INQUIRY**
that the device really is vendor `HP` with serial `HUJ808A5L4`, exiting nonzero
if not.

```bash
$ /scratch/tapectl-lto6-session/bin/tape-dev.sh all
nst=/dev/nst3 st=/dev/st3 sg=/dev/sg5 serial=HUJ808A5L4 vendor=HP
```

Resolved for this session: `nst=/dev/nst3`, `st=/dev/st3`, `sg=/dev/sg5`.
Re-checked before each phase; if mhvtl load order ever shifts, the numbers
change and the guard follows them.

---

## Phase 1 — Pre-flight

Working the Pre-flight section of `docs/lto6-validation-checklist.md`.
Raw output: `recordings/preflight-*.txt`.

| # | Check | Result |
|---|---|---|
| 1 | Drive visible (`lsscsi -g`) | ✅ `[4:0:0:0] HP Ultrium 6-SCSI 35GD /dev/st3 /dev/sg5` |
| 2 | Drive responds (`mt status`) | ✅ `BOT ONLINE IM_REP_EN`, density `0x5a (LTO-6)` |
| 3 | MAM query (`sg_read_attr`) | ✅ capacity 2,499,053 MiB, load count 1, TapeAlert 0 |
| 4 | sg_logs counters (page 0x02) | ✅ readable — all counters 0 (fresh drive + media) |
| 5 | `dar` ≥ 2.6 | ✅ 2.7.13 |
| 6 | 512 K block size accepted | ✅ `Tape block size 524288 bytes` |
| 7 | Compression state recorded | ⚠️ **ON as found** — see below |
| 8 | mhvtl gate green | ⏸️ not re-run this session; see note |

### Check 5 note — `dar --version` needs stdin closed

`dar --version` writes `No terminal found for user interaction...` to stderr and
the version banner gets lost in normal capture. `dar --version </dev/null` gives
a clean read. Worth knowing for any scripted version check.

### Check 7 finding — hardware compression is ENABLED as found

`sg_modes --page=0x0f` returned:

```
>> Data Compression, page_control: current
 00     0f 0e c0 80 00 00 00 01  00 00 00 01 00 00 00 00
```

Decoding byte 2 and byte 3 of the SSC Data Compression page:

| Bit | Value | Meaning |
|---|---|---|
| byte2 bit7 `DCE` | **1** | Data Compression **Enabled** |
| byte2 bit6 `DCC` | 1 | Data Compression Capable |
| byte3 bit7 `DDE` | 1 | Data Decompression Enabled |
| byte3 bits6-5 `RED` | 0 | Report Exception on Decompression = 0 |

So the drive's as-found state is **compression ON**, and the design requires it
OFF for encrypted data (encrypted bytes are incompressible; leaving compression
on costs throughput and makes capacity accounting lie). Issue #28 has tapectl
issue `MTCOMPRESSION 0` at write time — this session is the first chance to
confirm that actually lands on real hardware. Checked again after the first
`volume write` in Phase 4.

The harness deliberately reports this rather than parsing it to a verdict
(vendor rendering differs); the decode above is mine, from the raw bytes.

### Check 8 note — mhvtl gate not re-run

The checklist's precondition is that `scripts/mhvtl-verify-gate.sh` is green
with an empty EXPECTED_FAIL. Per CLAUDE.md it is currently green with exactly
two ticketed EXPECTED_FAIL entries (H7 #33, H8 #34, both phase-2). I did not
re-run it here: it drives the mhvtl virtual devices, and running it would take
the same `/tmp/tapectl-tape.lock` this session needs. Recorded as a known
deviation from the checklist's stated precondition rather than silently skipped.
---

## Phase 2 — Measurement harness (`scripts/lto6-measure.sh`)

Settles the measurable subset of `docs/design/v2-open-questions.md` §5.
**This phase destroys the cartridge's contents** (it overwrites from BOT; there
is no long `mt erase` in the script, so it costs minutes, not hours).

```bash
W=/scratch/tapectl-lto6-session
./scripts/lto6-measure.sh \
    --erase-cartridge EW7VWMVKF6 \
    --device "$($W/bin/tape-dev.sh nst)" \
    --out "$W/recordings/measure"
```

### Consent check passed on the medium serial

The script cross-checks the named cartridge against MAM before erasing
anything, and it accepted `EW7VWMVKF6` — matching on the `Medium serial number`
attribute:

```
Barcode verified against MAM: `EW7VWMVKF6`.
```

Worth recording because this cartridge is **blank and unlabelled** — MAM's
`Volume identifier` is empty. I expected to need `--allow-unverified-barcode`
and did not: the script's grep accepts `medium serial number` as well as
`barcode`, so an unlabelled tape still gets a real consent check rather than a
bypass. That is the anchored-grep design in the script's comments doing its job.

### A one-line trap for anyone re-running this

The harness discovers the sg node by `basename`-ing the device path and
stripping a leading `n`, then grepping `lsscsi`. That means **a `by-id` path
breaks discovery** — `/dev/tape/by-id/scsi-HUJ808A5L4-nst` does not basename to
`st3`. It needs a real `/dev/nstN`. So the guard resolves the stable name to a
node *and verifies the serial* immediately before the call, and the node is what
gets passed in. Safety and the script's expectations both satisfied.

### Harness results

Report: `recordings/measure/run-20260910-012953/REPORT.md`. Raw output for every
step is in that directory.

| § | Question | Answer |
|---|---|---|
| A | Block size 512 K vs 1 M | **No difference.** See below — the harness's own number is an artifact |
| B | Hardware compression as-found | **ON** (`DCE=1`) — design requires OFF |
| C | LBP | **Drive supports it**, currently disabled |
| D | MAM remaining-capacity over-report | **MAM remaining capacity does not move.** See below |
| E | EOD semantics (§3.2's assumption) | ✅ **PASS** — read past EOD returned no data |
| F | Drive inventory / error counters | All zero; captured for the record |

READ BLOCK LIMITS: min 1 B, **max 16,777,215 B (16 MB)**. Note this contradicts
the mhvtl dry-run's "drive advertised a 2 MiB maximum" — that was mhvtl's
virtual drive, not this hardware.

### ⚠ A — the harness reports a block-size effect that does not exist

The harness measured:

```
| 512K | accepted | 36.1 MiB/s (2048 MiB in 56.8s) |
| 1M   | accepted | 113.0 MiB/s (2048 MiB in 18.1s) |
```

Read literally that is a 3.1× win for 1 M, and the harness's own text says a
large enough advantage "is the input to reopening that choice". **It is an
artifact.** The harness generates one payload file on `/scratch` (a virtio-blk
disk) and reads it once per block size: the first run reads it cold, later runs
read it warm from page cache. What was measured was the source disk, not the
tape.

Re-ran it controlled — payload in `/dev/shm` so both runs read from RAM, and
the order alternated so residual warm-up would show as disorder rather than a
trend (`bin/blocksize-ab.sh`, log at `recordings/blocksize-ab.log`):

```
source read (RAM -> /dev/null): 0.58s  == 3531 MiB/s     <- not the bottleneck

=== pass 1: 512K then 1M ===
512K      114.0 MiB/s  (2048 MiB in 18.0s)
1M        114.4 MiB/s  (2048 MiB in 17.9s)
=== pass 2: reversed order (1M then 512K) ===
1M        114.7 MiB/s  (2048 MiB in 17.9s)
512K      113.9 MiB/s  (2048 MiB in 18.0s)
=== larger sizes, single pass ===
2M        114.5 MiB/s  (2048 MiB in 17.9s)
4M        114.6 MiB/s  (2048 MiB in 17.9s)
```

**Every block size from 512 K to 4 M writes at ~114 MiB/s.** The spread across
six runs is 0.8 MiB/s — under 1%.

Conclusions:

1. **§5's block-size question resolves to "leave it alone".** 512 K is not
   costing anything. This is the "a wash" branch the harness anticipated.
2. **`scripts/lto6-measure.sh` has a methodology bug** and should be fixed
   before anyone runs it again, because as written it argues for re-engineering
   the write path on the strength of a page-cache artifact. Fix: generate the
   payload in `tmpfs`, or drop caches between runs, or read cold every time.
3. **1 M blocks are accepted on this hardware.** The 2026-08-02 dry-run's 1 MiB
   `EBUSY` was an mhvtl/host-config artifact, not a real `st` ceiling here.
4. ~114 MiB/s is below LTO-6's ~160 MB/s native rate. Not yet attributed —
   candidates are the virtio-scsi passthrough, compression being ON while the
   payload is incompressible, or the drive's real streaming rate. Worth one
   experiment (§B below changes one of those variables).

### ⚠ D — MAM remaining-capacity is not a live gauge on this drive

> **SUPERSEDED — read Phase 6.** The conclusion in this section is WRONG. It
> rests on the harness's `mam-before`/`mam-after` pair, and that pair was
> sampled around a write that silently failed (a block-size mismatch inside
> the harness). Remaining capacity *does* update promptly. Kept here
> unedited because the reasoning-from-bad-data is the point.

The harness leaves this as a manual diff. Doing it:

| | Remaining capacity [MiB] |
|---|---|
| session start (pre-flight) | 2,499,053 |
| `mam-before` (harness) | 2,499,053 |
| `mam-after` (harness, +2048 MiB written) | 2,499,053 |
| after ~4 GiB written this session | 2,499,053 |

**It never moved.** Meanwhile the drive *does* track writes live:

```
Total MiB written in medium life:      0  ->  4132
Total MiB written in current/last load: 0  ->  4132
```

So the drive updates its write counters in real time but does not decrement
`Remaining capacity in partition` at the multi-GiB scale — the over-report after
4 GiB of writing is the full 4 GiB, i.e. 100% of what was written.

This matters because §5 frames question D as *sizing the ENOSPC buffer* from the
over-report bound. If remaining-capacity is static over the range we can
practically test, then **there is no finite bound to measure this way** and MAM
remaining-capacity cannot serve as a live capacity gauge. Two honest readings:

- the drive only recomputes at coarse granularity (per-wrap, at EOD, or at
  unload), and 4 GiB of a 2.44 TiB tape (0.16%) is simply below its resolution; or
- it only refreshes the attribute on unload/reload.

Either way the practical conclusion for the pre-flight capacity gate is the
same: **do not trust `Remaining capacity` to reflect what this session has
already written.** `Total MiB written in current/last load` *is* live and is the
attribute to lean on. This wants confirming with a much larger write before it
is treated as settled — see "Open" at the end.

### C — LBP is available but off

```
current  : 4a f0 00 04 00 00 00 00     LBP_METHOD = 0  (off)
default  : 4a f0 00 04 00 00 00 00
saved    : 4a f0 00 04 00 00 00 00
changeable: 4a f0 00 04 ff 3f c0 00    <- non-zero = the drive accepts changes
```

The changeable mask is what answers the question: `LBP_METHOD` is fully
changeable (`ff`), the LBP information length is changeable (`3f`), and the
`LBP_W`/`LBP_R` bits are changeable (`c0`). **This drive supports logical block
protection**; it is simply disabled. Enabling it stays a considered change with
its own ticket, as the harness argues — MODE SELECT changes the block format for
every subsequent command.
---

## Phase 3 — Isolated test archive

Built under an explicit `--home` (via `TAPECTL_HOME`) so nothing touches the
real `~/.tapectl`:

```bash
export TAPECTL_HOME=/scratch/tapectl-lto6-session/tapectl-home
tapectl init --operator lto6-validation -y
```

### Source data — two tenants, deliberately awkward

| Unit | Tenant | Content | Size |
|---|---|---|---|
| `acme-photos` | `acme` | 36 incompressible blobs across 2019/2020/2021 | 178 MB |
| `globex-docs` | `globex` | real text (`/usr/share/doc`) + 3× 20 MB random | 61 MB |

Two tenants on one volume is the point: it exercises the multi-tenant envelope
path and the trial-decryption isolation the design claims. `globex-docs` also
carries an empty file, a 5-deep path, a UTF-8 filename (`ünïcödé.txt`) and a
relative symlink, so the restore `diff -r` has something to actually test.

```
tenant "acme" created (id=2) with primary and backup keys
tenant "globex" created (id=3) with primary and backup keys
unit "acme-photos" initialized (id=1)
unit "globex-docs" initialized (id=2)
snapshot created: acme-photos v1 (37 files, 177 MB)
snapshot created: globex-docs v1 (28 files, 60 MB)
staged: acme-photos (1 slices, 177 MB dar, 177 MB encrypted)
staged: globex-docs (1 slices, 60 MB dar, 60 MB encrypted)
```

Note "177 MB dar, 177 MB encrypted" — no compression gain, exactly as expected
for age ciphertext over already-incompressible input.

### Two config defects found while setting this up

**1. The shipped default `dar` path does not exist.** A fresh `init` writes
`binary = "/opt/dar/bin/dar"`, and `init` itself says so:

```
dar:      /opt/dar/bin/dar (NOT FOUND — install before staging)
```

Good that it warns. Worth noting the default is wrong on Debian/Ubuntu
(`/usr/bin/dar`) and on any distro using `/usr/local/bin`.

**2. `config check` rejects a `PATH`-resolved `dar`, but the runtime accepts it.**

Setting `binary = "dar"` — which is what CLAUDE.md tells us to do, precisely so
the path is not hardcoded — produces:

```
warning: dar binary not found at 'dar' — config.dar.binary points nowhere;
archiving will fail until this is corrected
```

That prediction is **false**. With `binary = "dar"` left in place, staging ran
fine:

```
staged: acme-photos (1 slices, 177 MB dar, 177 MB encrypted)
```

The cause is a mismatch between the checker and the runtime:

- `src/policy/depth_check.rs:39` — `check_dar` does `Path::new(binary).exists()`,
  which is false for a bare name.
- `src/dar/version.rs:18` and `src/dar/restore.rs:83` — the runtime uses
  `Command::new(dar_binary)`, which **does** resolve via `PATH`.

So `config check` emits a false negative that pushes the user toward hardcoding
an absolute path — the exact thing CLAUDE.md warns against ("never hardcode
`/usr/bin/dar`, which is wrong on any distro installing to `/usr/local/bin`").
Fix: when `binary` contains no `/`, resolve it via `PATH` before the existence
test, so the checker asks the same question the runtime will.

---

## Phase 4 — Round-trip on real media

### Backend configuration

`volume init` first failed with `configuration error: no LTO backend configured` —
a fresh `init` writes `[backends] lto = []` and there is no command to populate
it, so it is a hand-edit. Added:

```toml
[[backends.lto]]
name = "hp-lto6"
device_tape = "/dev/nst3"
device_sg = "/dev/sg5"
media_type = "LTO6"
nominal_capacity = "2.5T"
usable_capacity_factor = 0.92
enospc_buffer = "50M"
block_size = "512K"
hardware_compression = false
```

### `config check` is candid about which knobs are inert — and that matters here

```
note: backends.lto["hp-lto6"].block_size is parsed but not consumed — the write
      path uses a fixed 512 KiB block unconditionally.
note: backends.lto["hp-lto6"].hardware_compression is parsed but not consumed —
      nothing issues MTCOMPRESSION today. (#28)
```

Two consequences for this session:

1. **The `block_size` default is misleading but currently harmless.**
   `default_block_size()` in `src/config.rs:147` returns `"1M"`, which
   contradicts the normative format constant — `v2-open-questions.md:442` calls
   512 K a "format constant — never scales" and `volume-format-v2.md` §1/D7 fixes
   it. Because the knob is inert, the write path uses 512 KiB regardless, so no
   tape is at risk today. It is a **latent** trap: `src/volume/layout.rs`
   hardcodes `setblk 524288` into the GUIDE and RESTORE.sh text it writes *onto
   the tape*, so if epic #20 ever wires `block_size` without also templating
   those strings, a 1 M tape would ship with recovery instructions telling the
   heir to use 512 K. Wire the two together or not at all.
2. **We are writing with hardware compression ON.** Phase 1 measured `DCE=1`, the
   design wants it off, and nothing issues `MTCOMPRESSION 0` yet (#28). So this
   write is the real-hardware demonstration of that gap. Re-checked after the
   write below.

### ADR-0005 blocks the write until an escrow recipient exists

```
error: volume "LTO6-0001" failed pre-write validation: escrow recipient missing (ADR-0005)
```

Working as designed, and worth recording that the gate fires on a real write
path rather than only in tests. Resolved with `key generate --escrow -y`.

**Operational note:** the escrow secret is printed exactly once and deliberately
stored nowhere — not in the DB, not on disk. For this session it is a throwaway
identity for a disposable archive, saved to
`/scratch/tapectl-lto6-session/keys/escrow-THROWAWAY.key` (mode 600, outside the
repo, deliberately NOT reproduced in this journal). Anyone scripting a first
write needs to capture that stdout at generation time or the recipient is lost —
that is the intended ceremony, but it does mean the flow cannot be fully
automated without a human holding the paper.

**Ordering question raised:** the two units were staged (and therefore encrypted)
*before* the escrow identity existed. ADR-0005 says the escrow public key is
appended to every *future* encryption's recipient list, so those already-sealed
slices cannot name a recipient that did not exist when they were written.
Whether pre-write validation should catch that — a volume passing the escrow
gate while carrying slices the escrow key cannot open — is examined against the
actual tape in the raw-recovery drill (Phase 5).
### The write

```
volume "LTO6-0001" write completed        real 2m6s  (user 1m39s)
```

The 2m6s is **not** a tape throughput figure — `user` time is 1m39s of that, so
it is CPU-bound in the unoptimized debug build (sha256 + age over 237 MB).
Phase 2 already established the tape path itself runs at ~114 MiB/s. Any number
that goes into `docs/perf-baselines.md` must come from a release build.

### ✅ `volume identify` reads back correctly off real tape

```
Label:   LTO6-0001
[layout] front_index = 3   seal_marker = 10   total_files = 11
[media]  cartridge_serial = "EW7VWMVKF6"
         mam_capacity_bytes = 2620446998528
```

The v2 layout is on real media and File 0 is readable as plain text.

### ⚠ `volume identify` leaves two MAM fields empty that MAM actually has

```
cartridge_manufacturer = ""
tape_length_meters = 0
```

The drive reports both:

```
Medium manufacturer: FUJIFILM
Medium length [m]: 846
```

`cartridge_serial` is populated correctly from MAM, so the plumbing exists and
these two attributes are simply not being read or not being mapped. Cosmetic
today, but this block is the self-describing header an heir reads, so an empty
manufacturer field is a small loss of provenance for free.

### ✅ Hardware compression IS disabled on write — and `config check` says otherwise

Measured across the write:

| | `DCE` bit |
|---|---|
| before (Phase 1, as found) | **1** — compression on |
| after `volume write` | **0** — compression off |

Something issued `MTCOMPRESSION 0`. Tracing it: `src/store.rs:498`,
`TapeStore::open` calls `dev.disable_compression()` (defined
`src/tape/ioctl.rs:114`) **unconditionally**.

So this advisory from `config check` is wrong:

```
note: backends.lto[...].hardware_compression is parsed but not consumed —
      nothing issues MTCOMPRESSION today. docs/design-errata.md §2.29 tracks
      `MTCOMPRESSION 0` landing with issue #28.
```
`src/policy/decorative.rs:73`

Precisely:

- **"the knob is not consumed" — true.** `disable_compression()` ignores the
  config value entirely, so setting `hardware_compression = true` does *not*
  turn compression on. The knob is inert in both directions.
- **"nothing issues MTCOMPRESSION today" — false.** The write path issues it on
  every `TapeStore::open`, and this session proves it lands on real hardware.

The design intent of #28 is therefore already satisfied in practice; it is the
advisory (and whatever §2.29 says) that is stale. Worth fixing, because an
operator reading that note would reasonably conclude they must disable
compression by hand before every write.

### ⚠ MAM remaining-capacity: revising the Phase 2 finding

Phase 2 concluded remaining capacity "never moved". With more data that is too
strong. After the real write:

| Point | Remaining capacity [MiB] | Total written medium life [MiB] |
|---|---|---|
| session start | 2,499,053 | 0 |
| harness `mam-after` (+2 GiB, read while positioned at ~2 GiB) | 2,499,053 | 4,132 |
| after `volume write` + `volume identify` (which rewinds) | **2,498,802** | 16,721 |

So it *does* move — it fell by 251 MiB, which matches the ~237 MB volume just
written from BOT rather than the 16.7 GiB cumulative total. That tells us two
things:

1. **Remaining capacity tracks the current end-of-data position, not cumulative
   writes** — correct behaviour for an overwrite-from-BOT workload.
2. **The attribute appears to refresh lazily.** The harness read it immediately
   after a write while still positioned at ~2 GiB and got a stale full-capacity
   value; the reading that moved was taken after a rewind/reposition.

If (2) holds, it is the operationally important half: **sampling MAM remaining
capacity immediately after writing returns a stale number**, which is exactly
what a naive ENOSPC guard would do. `Total MiB written in current/last load` was
live at every sampling point and is the more trustworthy attribute.

This is a hypothesis with two supporting data points, not a settled result. It
needs a controlled test — sample, write, sample, rewind, sample — which would
overwrite this cartridge, so it is deferred until after the raw-recovery drill
(Phase 5) has finished with the tape.
### ✅ Verify

```
verify LTO6-0001 (full tier): 11 checked, 11 passed, 0 failed
```

All 11 files of the v2 layout read back off real media with matching sha256.

### ✅ Restore + `diff -r` — full pass

```
restored "acme-photos" from LTO6-0001 (1 slices) to .../restore/acme
restored "globex-docs" from LTO6-0001 (1 slices) to .../restore/globex
```

Both units restore clean. Verified two independent ways:

```
diff -r --no-dereference src/acme-photos  restore/acme    -> IDENTICAL
diff -r --no-dereference src/globex-docs  restore/globex  -> IDENTICAL

content+symlink-target tree checksums:
  acme-photos  == acme    0de633ebd565df643a1d3afd099ad80bb6be360ca99df61428c8c72db292f7da
  globex-docs  == globex  ff452ad12904cad1086bffc5d0906346947264b6e299d42301d88b5a3ebbb106
```

The awkward cases all survived the tape round-trip:

| Case | Result |
|---|---|
| UTF-8 filename `ünïcödé.txt` + CJK content | ✅ name and content intact |
| Zero-byte file | ✅ restored at 0 bytes |
| 7-deep path | ✅ full depth preserved |
| Relative symlink | ✅ target `../../../../dataset_1.bin` preserved |
| **Dangling** symlink (from `/usr/share/doc/bash`) | ✅ preserved *as a link*, not dereferenced |

**Methodology note for whoever repeats this:** a plain `diff -r` **fails** on this
data, and it is not tapectl's fault. `/usr/share/doc/bash` contains a symlink
whose target does not exist; `diff -r` dereferences symlinks and errors on both
sides. Use `diff -r --no-dereference`. The checklist's bare
`diff -r <source> /tmp/recovered` will produce a false failure on any real
source tree containing a dangling link — worth amending there.

An informational warning appears on every restore, correctly:

```
WARN restoring as a non-root user: restored files will be owned by the
     invoking user, not their archived owners
```

### Drive health after the round-trip

```
tapectl report health:
  LTO6-0001 verify: corrected=0 uncorrected=0 alerts=0
  LTO6-0001 write:  corrected=0 uncorrected=0 alerts=0
```

**Uncorrected errors are 0 on both the read and write counters** — the number
that matters. Media and drive are healthy.

### ⚠ `report health` reports `corrected=0` while the drive reports 875

The raw LOG SENSE pages tell a busier story than tapectl's summary:

```
Write error counter page [0x2]
  Errors corrected without substantial delay   = 875
  Total errors corrected                       = 0
  Total times correction algorithm processed   = 305674
  Total uncorrected errors                     = 0
```

tapectl's `corrected=0` is consistent with the **`Total errors corrected`**
parameter, which really is 0 — so this is a field-selection question, not a
miscount. But `Errors corrected without substantial delay` (875) and
`Total times correction algorithm processed` (305,674) are the parameters that
normally trend upward as a drive or medium degrades, and an operator reading
`report health` sees a cleaner picture than the drive is actually reporting.
Worth deciding deliberately which parameter the health summary should surface,
and saying so in the output. Not urgent — uncorrected is 0 either way, and on a
brand-new tape 875 ECC corrections across ~17 GiB is unremarkable.
---

## Phase 5 — Raw-recovery drill (the heir leg), no tapectl

Tools used: `mt`, `dd`, `age`, `dar`, `sha256sum`, `tr`. The `tapectl` binary was
not invoked once in this phase. All present on this host, `age` 1.1.1.

### ✅ RESTORE.sh comes off the tape and runs

```bash
mt -f "$DEV" rewind && mt -f "$DEV" setblk 524288 && mt -f "$DEV" fsf 2
dd if="$DEV" bs=512k | tr -d '\0' > RESTORE.sh && chmod +x RESTORE.sh
```

25,278 bytes, executable, self-describing — including a documented degradation
ladder (front index → seal marker's embedded copy → manual zero-strip).

### ✅ `--info`: the front index and seal binding are intact on real media

```
Verdict: SEALED
  Front index (file 3) hash matches the seal marker's binding.

    0  id_thunk                      1700 bytes
    1  system_guide                  6768 bytes
    2  restore_sh                   25278 bytes
    3  front_index                      -
    4  tenant_envelope              10346 bytes
    5  tenant_envelope              11486 bytes
    6  operator_envelope            56335 bytes
    7  operator_envelope_backup     56335 bytes
    8  data_slice               185656602 bytes
    9  data_slice                63135386 bytes
   10  seal_marker                      -
Sealed at: 2026-09-10T01:43:17Z
```

Envelopes before slices, front index at file 3, seal marker last — the v2 layout
exactly as `volume-format-v2.md` specifies, confirmed from recorded bytes.

### ✅ `--verify`: keyless integrity walk passes on all 10 files

```
PASS file 3 front_index (matches seal binding)
PASS files 0,1,2,4,5,6,7,8,9
VERIFY: PASS — every file matches the front index.      (6.9s)
```

### ✅ `--restore` with a tenant key reproduces the source byte-for-byte

Trial-decryption picked the right envelope on its own (tried file 4 → rejected,
file 5 → accepted), which is the tenant-isolation claim working on real media.

```
40 inode(s) restored ... 0 inode(s) failed to restore
>>> RESTORE COMPLETE                                     (5.0s)

diff -r --no-dereference src/acme-photos recovery/recovered  -> IDENTICAL
sha256 of tree: e50bcf6644f29c164020f7588acf0eb3ebdeb10f04821b1fb0a74517b10b2323 (both)
```

**The design's strongest claim holds on real hardware.**

---

## 🔴 FINDING — a SEALED tape can pass escrow validation and still be
## unrecoverable by the escrow key

This is the most serious thing found in the session.

### What happened

The escrow identity was generated *after* the two units were staged, because
`volume write` is what refused to proceed without one:

```
01:37:05  stage create acme-photos    -> slices encrypted
01:37:50  stage create globex-docs    -> slices encrypted
01:41:30  key generate --escrow       -> escrow identity exists from here on
01:41:55  volume write                -> envelopes encrypted, tape sealed
```

The write then succeeded, the tape sealed, `volume verify` passed 11/11, and
`RESTORE.sh --verify` passed keylessly. Everything reports healthy.

But the escrow key cannot recover the data:

```
$ ./RESTORE.sh --find-envelope --key escrow.age.key
>>> Decrypted envelope at file 4          <- envelope opens fine

$ ./RESTORE.sh --restore --key escrow.age.key --to ./escrow-recovered
>>>   checksum verified against front index
age: error: no identity matched any of the recipients
FATAL: cannot decrypt slice 1 — wrong key?
```

### Why — confirmed from the bytes

age recipient counts, read directly:

| Object | Encrypted at | X25519 recipient stanzas |
|---|---|---|
| tenant envelope (tape file 4) | `volume write`, 01:41:55 | **5** |
| data slice (staging) | `stage create`, 01:37:50 | **4** |

The envelope was encrypted after the escrow recipient existed and includes it.
The slices were encrypted before it existed and do not. Slice mtime (01:37:50)
predates the escrow public key (01:41:30) by 3m40s.

(The stanza *values* cannot be compared across the two files — an X25519 stanza
carries a per-encryption ephemeral share, not the recipient's public key. The
count, plus the observed decrypt/refuse behaviour, is the evidence.)

### Why it matters

ADR-0005 exists for exactly one scenario: the site is lost, the tenant and
operator keys are gone with it, and an heir has the printed escrow kit. On this
tape that heir gets **the envelope** — a manifest describing, in detail, the unit
they cannot decrypt — and nothing else. The failure is silent at write time and
only discoverable by attempting an escrow-key restore, which nobody does until it
is the only option left.

### The gap in the check

Pre-write validation asks *"is an escrow recipient registered?"* It does not ask
*"are the slices this volume is about to seal actually encrypted to it?"* Those
come apart whenever staging precedes escrow registration — which is the natural
order for anyone setting up a new archive, since `volume write` is the first
command that mentions escrow at all.

### Suggested fixes (not applied — this is a validation session)

1. **Make pre-write validation check the slices, not the registry.** The escrow
   recipient's stanza either is or is not in each staged slice; verify it and
   refuse the write if it is missing.
2. **Fail earlier.** `stage create` should require the escrow recipient too, so
   the error arrives before the expensive encryption rather than after.
3. **Have `init` create the escrow identity** as part of first-run setup, so the
   window in which staging can precede escrow never opens.
4. **Give `RESTORE.sh --verify` an escrow-reachability check** — it already walks
   every file; confirming a named recipient is present in each slice header needs
   no key material.

Fix (1) is the load-bearing one; (3) closes the window; (2) and (4) make the
failure loud early and auditable later.
---

## Phase 6 — MAM capacity, measured properly (correcting Phase 2)

Controlled test (`bin/mam-refresh-test.sh`), run after the recovery drill so the
cartridge was free:

```
A. at BOT, before writing              remaining=2498802 MiB   written_this_load=16721 MiB
B. immediately after write             remaining=2497003 MiB   written_this_load=18775 MiB
C. after weof (still at EOD)           remaining=2497003 MiB   written_this_load=18777 MiB
D. after rewind to BOT                 remaining=2497003 MiB   written_this_load=18780 MiB
E. after fsf 1                         remaining=2497003 MiB   written_this_load=18780 MiB
```

`B < A`, so **remaining capacity updates immediately after the write**. The
lazy-refresh hypothesis from Phase 4 is **disproved** — repositioning changes
nothing (C, D, E are all equal to B).

### 🔴 Why Phase 2 saw it "never move": a second bug in `lto6-measure.sh`

The harness's section D never wrote anything. Its sequence is:

```bash
measure_blocksize 1048576 "1M"     # section A ends here, leaving the driver in 1 M mode
...
# ---------- D. MAM over-report bound ----------
mt -f "$TAPE_DEV" rewind
mt -f "$TAPE_DEV" weof 1
raw mam-before sg_read_attr "$DRIVE_SG" || true
dd if="$PAYLOAD" of="$TAPE_DEV" bs=524288 status=none 2>/dev/null || true   # <-- 512 K
```

Section A leaves the `st` driver in **1 M** fixed-block mode. Section D then
writes with **`bs=524288`** and never issues its own `setblk`. Writing a 512 K
block to a tape in 1 M fixed-block mode is `EINVAL`. Reproduced deliberately:

```
$ mt -f /dev/nst3 setblk 1048576
$ dd if=payload of=/dev/nst3 bs=524288 count=10
dd: error writing '/dev/nst3': Invalid argument
0+0 records out, 0 bytes copied
```

`2>/dev/null || true` swallows both the message and the exit status, so
`mam-after` is sampled after a **zero-byte** write and is necessarily identical
to `mam-before`. The harness then reports the difference as the over-report —
producing a confident, wrong answer of "no change".

Note section E *does* issue `mt setblk 524288` before its writes, so the author
knew the driver state carries over; section D just missed it. Fix: set the block
size explicitly in section D, and stop discarding the `dd` status — a silent
failure here is worse than a loud one, because it corrupts a safety parameter.

### ✅ The actual answer to question D — MAM is accurate

My first pass at this arithmetic was wrong and is corrected here.

Remaining capacity tracks the **end-of-data position**, not cumulative writes
(established in Phase 4). My test rewound and overwrote from BOT, but I took the
baseline from reading `A` — which was taken while the tape still had the 251 MiB
volume on it. Differencing against a dirty baseline invented a shortfall.

Against the true empty-tape capacity (2,499,053 MiB, measured on the blank
cartridge in Phase 1):

| Event | Remaining | Implied consumed | Actually written from BOT | Discrepancy |
|---|---|---|---|---|
| `volume write` (~243 MiB of blocks) | 2,498,802 | 251 MiB | ~243 MiB | ~8 MiB conservative |
| 2,048 MiB overwrite from BOT | 2,497,003 | 2,050 MiB | 2,048 MiB | **+2 MiB** |

**MAM remaining-capacity is accurate to within a few MiB at every scale
measured, and errs slightly conservative** (it reports marginally *less*
remaining than a naive byte count implies — the safe direction).

So there is **no measurable over-report**, and nothing here contradicts the
configured default:

```toml
enospc_buffer = "50M"
```

An irony worth recording: the harness's section-D *methodology* was right all
along — `rewind` → `weof` → sample → write → sample gives a clean baseline,
because `weof` truncates EOD back to BOT. It just never wrote anything (the
block-size bug above). My replacement test wrote successfully but skipped the
`weof`, so it had the opposite flaw. Neither run was trustworthy on its own; the
answer came from differencing against the independently-measured blank-tape
capacity instead.
---

# Summary

## What passed — the happy path is real

| Check | Result |
|---|---|
| v2 layout written to real LTO-6 | ✅ 11 files, envelopes before slices, sealed |
| `volume verify` | ✅ 11 checked, 11 passed, 0 failed |
| `volume identify` | ✅ File 0 readable as plain text |
| `restore unit` × 2 tenants + `diff -r` | ✅ byte-identical, incl. unicode / empty / deep / symlink / **dangling** symlink |
| **Raw-recovery drill (no tapectl)** | ✅ RESTORE.sh off tape → keyless verify PASS → keyed restore → **byte-identical** |
| Tenant isolation (envelope trial-decrypt) | ✅ wrong envelope rejected, right one found |
| EOD semantics (§3.2's assumption) | ✅ read past EOD returns no data |
| Uncorrected read/write errors | ✅ 0 / 0 |

The design's strongest claim — *full restore with no database and no tapectl* —
works on real hardware, end to end, from bytes recorded on tape.

## Findings, by severity

| # | Finding | Where |
|---|---|---|
| 🔴 1 | [#115](https://github.com/mikmorg/tapectl/issues/115) — **A SEALED tape can pass escrow validation while the escrow key cannot decrypt its slices.** Staging before escrow registration produces slices with no escrow recipient; pre-write validation only checks the *registry*. Silent until it is the only key left. | Phase 5 |
| 🔴 2 | [#116](https://github.com/mikmorg/tapectl/issues/116) — **`lto6-measure.sh` §D never writes** — 512 K `dd` against a driver left in 1 M mode, `EINVAL` swallowed by `\|\| true`. Produces a confident wrong answer for the parameter that sizes the ENOSPC buffer. | Phase 6 |
| 🟠 3 | [#117](https://github.com/mikmorg/tapectl/issues/117) — **`lto6-measure.sh` §A reports a fake 3.1× block-size win** — one payload file read once per size, so the first run pays a cold read. Argues for re-engineering the write path on a page-cache artifact. | Phase 2 |
| 🟠 4b | [#121](https://github.com/mikmorg/tapectl/issues/121) — **`block_size` default is 1M while the format constant is 512K** — inert today, but `layout.rs` hardcodes 512K into the on-tape recovery text, so wiring the knob would break the heir path. | Phase 4 |
| 🟢 4 | **MAM remaining-capacity is accurate** — within ~2 MiB of bytes written, erring conservative. No over-report found; the `50M` ENOSPC buffer is not contradicted. (My first pass claimed a 255 MiB over-report; that was a dirty-baseline error, corrected in Phase 6.) | Phase 6 |
| 🟡 5 | [#118](https://github.com/mikmorg/tapectl/issues/118) — **`config check` says "nothing issues MTCOMPRESSION today"** — false. `store.rs:498` issues it unconditionally and the drive confirms (`DCE` 1→0). The *knob* is inert; the *claim* is wrong. | Phase 4 |
| 🟡 6 | [#119](https://github.com/mikmorg/tapectl/issues/119) — **`config check` calls a working `dar` config broken** — `binary = "dar"` reported as "points nowhere" though staging works. `check_dar` uses `Path::exists`, runtime uses `Command::new` (`PATH`). | Phase 3 |
| 🟡 7 | [#120](https://github.com/mikmorg/tapectl/issues/120) — **`report health` shows `corrected=0` while the drive reports 875** — reads only `Total errors corrected`; the parameters that trend on a degrading drive are not surfaced. Raw log *is* stored. | Phase 4 |
| 🟡 8 | [#122](https://github.com/mikmorg/tapectl/issues/122) — **The checklist's bare `diff -r` gives a false failure** on any source containing a dangling symlink. Needs `--no-dereference`. | Phase 4 |
| 🔵 9 | [#123](https://github.com/mikmorg/tapectl/issues/123) — `volume identify` leaves `cartridge_manufacturer` / `tape_length_meters` empty though MAM has both (FUJIFILM, 846 m). | Phase 4 |
| 🔵 10 | [#124](https://github.com/mikmorg/tapectl/issues/124) — Fresh `init` writes `dar` path `/opt/dar/bin/dar`, which exists on no mainstream distro. | Phase 3 |
| 🔵 11 | [#124](https://github.com/mikmorg/tapectl/issues/124) — No command populates `[[backends.lto]]` — first write requires hand-editing TOML. | Phase 4 |

**Two of the three most serious findings are in the validation instrument
itself.** Both would have been believed: they produce plausible numbers, not
errors. §5's block-size question would have been answered "switch to 1 M" and
its ENOSPC question "no over-report exists" — both wrong, both from a script
that exited 0.

## Answers to `v2-open-questions.md` §5

| Q | Answer |
|---|---|
| A. Block size 512 K vs 1 M | **No difference** — 114.0 vs 114.4 MiB/s, <1% across 512 K/1 M/2 M/4 M. Keep 512 K. |
| B. Compression as-found | **ON** out of the box; tapectl **does** disable it per write (verified `DCE` 1→0). |
| C. LBP | **Supported** by this drive (changeable mask `ff 3f c0`), currently off. Enabling remains a considered change. |
| D. MAM over-report | **None measurable** — accurate to ~2 MiB and slightly conservative. The `50M` buffer stands. Worth re-confirming near a full tape. |
| E. EOD semantics | ✅ **PASS** — forward read past EOD returns no data. §3.2's assumption holds. |
| F. Inventory | Captured; all error counters zero at session start. |

Also settled: the drive advertises a **16 MB** max block size (not the 2 MiB the
mhvtl dry-run reported), and **1 M blocks are accepted** on real hardware — the
dry-run's `EBUSY` was an mhvtl/host artifact.

## Not done

- **The ENOSPC drill.** Filling 2.44 TiB at ~114 MiB/s is ~6 hours of continuous
  writing. It is the one check mhvtl actively lies about, so it still matters —
  it just needs a dedicated session.
- **Confirming MAM accuracy near a full cartridge.** It held to ~2 MiB at the
  2 GiB scale; whether it stays accurate at 2 TiB is untested and is the regime
  the ENOSPC buffer actually guards.
- **Release-build throughput for `perf-baselines.md`.** Everything here was a
  debug build; the 2m6s write is CPU-bound and not a tape number. The honest tape
  figure from this session is **~114 MiB/s** via raw `dd`.
- **The mhvtl gate was not re-run** (checklist precondition) — it would contend
  for the same tape lock.

## State left behind

- **Cartridge `EW7VWMVKF6`:** contents destroyed by the Phase 6 experiment. Not
  blank — carries leftover test patterns. Re-label or bulk-erase before reuse.
- **Passthrough:** live and persisted in vm-desk1's config; survives a VM reboot.
- **Test archive:** `/scratch/tapectl-lto6-session/` — disposable. `~/.tapectl`
  was never touched.
- **Throwaway escrow key:** `/scratch/tapectl-lto6-session/keys/` (mode 600,
  outside the repo). Not a Heir Kit; delete with the rest.
---

# Re-validation, 2026-09-10 (autopilot run)

After the findings above were fixed (#115–#124, #127) and landed, everything
was re-validated on the same real HP LTO-6 (`/dev/nst3`, cartridge
`EW7VWMVKF6`). The mhvtl gate is unusable on this VM (dead `lload` IPC — the
library moves media but the tape daemon never sees it), so per CTO decision a
real-hardware round-trip substitutes; re-run the mhvtl gate after a VM reboot.

**`scripts/lto6-measure.sh`, fixed (#116/#117), re-run — all three defects gone:**

- §A block size: `512K 108.1 | 1M 108.7 | 1M 108.7 | 512K 108.4` MiB/s across
  alternating passes — a wash, correctly reported (the fake 3.1× is gone). The
  source-read-rate line (758.8 MiB/s) is printed, so the disk-sourced-payload
  fallback is self-evident.
- §D MAM: the capacity write now happens — remaining fell 2,499,053 → 2,497,003
  (2,050 MiB for a 2,048 MiB write). Over-report **+2 MiB** — MAM is accurate,
  matching the corrected Phase 6 finding. (Was "never moves" — a swallowed
  EINVAL.)
- §E EOD: PASS.

**pm-115 (escrow fix + `--allow-missing-escrow`) + #127 (RESTORE.sh `--unit`
envelope search), validated via `scripts/lifecycle-suite.sh escrow-ordering` on
real media — 29/29 checks GREEN, 0 skips:**

- `stage create` before escrow → refused; `stage list` empty. (The exact
  2026-09-10 hole, now closed.)
- `key rotate` before escrow → refused.
- write with escrow'd slices → `volume verify` 11/11.
- The full restore matrix for BOTH tenants: `restore unit`, `restore file`,
  RESTORE.sh dd/`--verify`/primary-key/backup-key, operator envelope, **escrow
  key**, `restore raw-volume`, cross-tenant isolation.
- The escrow key decrypts a slice's ciphertext directly (`age -d`, 702 KB
  plaintext) — the core #115 fix, which the pre-fix tape could not do.
- `eo-unitA.escrow` passes even though unitA is not in the first-decrypted
  envelope — the #127 fix (RESTORE.sh keeps searching for the unit's envelope).

**Still deferred (own iterations):** `init` creating the escrow identity (CTO
Q3), the `[[backends.lto]]` first-run ergonomics (#124b/#126), the audit check
for already-written pre-escrow volumes (#125), and a full `--all` lifecycle run
across every scenario. The retire-and-reuse and compaction scenarios encode
real-erase reuse semantics that a short-erased single cartridge cannot fully
satisfy — they SKIP or need `--erase long` / a second cartridge.

---

## Phase 7 — post-reboot: the mhvtl gate, and a wrong-tape read

The reboot was the human step needed to fix mhvtl's dead `lload` IPC (CTO Q1
deferred the gate re-run until after it). mhvtl came back healthy. The gate then
failed RED three times in a row, on exactly three checks:

```
heir_find_envelope: FAIL  << UNEXPECTED — regression
heir_restore: FAIL  << UNEXPECTED — regression
heir_restore_symlink_unit: FAIL  << UNEXPECTED — regression
```
```
>>> Trying envelope at file 4...  5...  6...  7...
FATAL: no envelope matched the provided key
```

### What made this hard

Everything passed in isolation. Ruled out, in order: `do_find_envelope` is
byte-identical between the gate's extracted RESTORE.sh and a known-good copy;
alice-primary secret ↔ DB ↔ `.pub` all match; `volume verify` passes (it hashes
every envelope); `tapectl restore` passes; tape reads are byte-consistent across
repeats; and four progressively more faithful replicas — up to an exact gate
replica with 1 M slices and the full fixture set — ALL PASS. Running the gate's
own heir sequence by hand on the preserved tape also passed.

I then chased a red herring for a while. An in-gate probe running a RESTORE.sh
patched *only* to drop `2>/dev/null` from the age command SUCCEEDED where the
unpatched one failed, which looked like a `set -uo pipefail` / SIGPIPE
interaction in the envelope loop. It was not: RESTORE.sh runs as a subprocess
with its own `set -euo pipefail`, so the gate's shell options never reach it.
The probe passed because it also pinned `TAPE_DEVICE=/dev/nst1` — the one
variable that actually mattered. Recorded because the wrong lead was
superficially compelling and cost real time.

### Root cause

`RESTORE.sh` defaults to `${TAPE_DEVICE:-/dev/nst0}` (`src/volume/layout.rs`).
The gate's four heir steps invoked it with no `TAPE_DEVICE`, so they read
`/dev/nst0` regardless of `TAPECTL_GATE_TAPE`. That was harmless for as long as
`/dev/nst0` *was* the mhvtl drive. After the reboot it was not:

```console
$ ls -l /dev/tape/by-id/ | grep -E 'nst[0-9]$'
scsi-HUJ808A5L4-nst -> ../../nst0     ← the REAL HP LTO-6
scsi-XYZZY_A1-nst   -> ../../nst1     ← mhvtl
```

The gate wrote to `nst1` and its heir leg read the real drive's tape. See
`docs/lto6-drive-passthrough.md` — I predicted this exact collision when setting
up the passthrough, but scoped the trigger too narrowly ("if mhvtl fails to
load"). A plain reboot re-racing the SCSI hosts was enough.

**Commands issued against the real drive `/dev/nst0` during the three RED runs**
(via RESTORE.sh, per run): `mt -f /dev/nst0 setblk 524288`, `mt -f /dev/nst0
rewind`, `mt -f /dev/nst0 fsf N`, `dd if=/dev/nst0 bs=524288`. **All read-only.**
The cartridge was repositioned; nothing was written to it, and its volume still
verifies. No `mt erase`, no `dd of=`.

### Why the symptom was so misleading

`heir_info` **PASSED** while reading the wrong tape, because nothing checked
*which* volume it read — the real drive happened to hold a valid sealed volume
from the lifecycle runs. The file maps make it unmistakable:

| | RED run (read `/dev/nst0`) | GREEN run (read `/dev/nst1`) |
|---|---|---|
| tenant envelopes | 7886 / 17089 | 10910 / 14046 |
| operator envelope | 47539 | 46600 |
| first data slice | 1049474 | 703525 |
| **printed label** | **`MHVTLG`** | `MHVTLG` |

Both printed the same label — `$LABEL` is baked into the script at write time
and was never compared with the tape. So the heir leg reported the expected
volume name while describing a completely different tape, and the only visible
failure was "no envelope matched the provided key": to an heir, "your key is
wrong" or "your archive is gone", when it meant neither.

### Fixes

- **`0dbfda7`** — gate pins `TAPE_DEVICE="$TAPE_DEV"` on all four heir steps.
  Gate **GREEN 26/26** against an empty EXPECTED_FAIL manifest, closing the
  deferred CTO Q1 re-run.
- **`3b5d287`** — RESTORE.sh now parses `label` from the ID thunk, announces the
  device and the tape's real label, warns prominently on mismatch, and names
  both labels in the "no envelope matched" message. Warns rather than exits: the
  script is generic apart from `$LABEL`, so a sibling tape is legitimate
  recovery, and an unreadable thunk must not become a hard stop. Labels are
  echoed through a new `safe_str()` because the ID thunk is unauthenticated.

Verified read-only against a genuinely mismatched tape:

```console
$ TAPE_DEVICE=/dev/nst1 ./RESTORE.sh --info      # script built for CHECK1
>>> Tape device:      /dev/nst1 (from TAPE_DEVICE)
>>> Tape identifies as: MHVTLR3
  WRONG TAPE? This script was written for volume 'CHECK1',
  but the tape in /dev/nst1 identifies itself as 'MHVTLR3'.
```

### Standing hazard

The write path was never at risk: `scripts/mhvtl-device.sh` is the single entry
point for every writing harness and refuses a device with no `device.conf` Drive
stanza (`no device.conf Drive matches /dev/nst0 at 0:0:0:0`, exit 2), which the
gate treats as fatal — verified. But **`/dev/nstN` numbering is not stable across
reboots on this VM.** Always address the real drive as
`/dev/tape/by-id/scsi-HUJ808A5L4-nst`, and re-check the mapping after any reboot
before trusting a bare device number in any doc, including this one.
