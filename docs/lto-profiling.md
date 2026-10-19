# Profiling an LTO drive

`scripts/lto-profile.sh` measures a drive and a test cartridge and prints the drive's
`[[backends.lto]]` block. It is the instrument behind the measured keys of that block
(`fill_ceiling`, `post_warning_bytes`), and it records, beside the real end of tape, the
cartridge's wear and the drive's rewrite counters so that a later fit has data (ADR-0012
2026-10-17 items 96-98 and 100, issue #479).

It is a script, not a tapectl command, so a change to a measurement needs no release. It
works for LTO-5 to LTO-9, the generations `src/media.rs` knows, on IBM and HPE drives: the
drive is keyed by what `sg_inq` says (vendor, product, firmware) and the generation of the
loaded cartridge, nothing is assumed from a model name except the drive's generation, which
comes from the same table `first-run.sh` uses.

**It erases the loaded cartridge.** Use a test cartridge you are willing to lose, never a
production tape.

## When to run it

- A new drive model, or new drive firmware, before its first production tape.
- A new cartridge generation in a drive that already has a profile.
- Before turning on `post_warning_bytes` (the slice cap, below), or to see what the runs say
  `fill_ceiling` could be (the emitted block never raises it above the 0.97 default by itself).
- Again on a few cartridges of the same model: the block takes the most conservative figure
  across the runs you give it, and says which run each value came from.

## Running it

The quick run takes minutes and leaves the cartridge ready for the fill; the full run adds
the fill, which takes hours (about 4.5 hours to the early warning on an LTO-6, the cartridge's
capacity divided by the drive's native speed on later generations, plus minutes after it).

```bash
# Minutes: inventory, block size, compression, LBP, MAM's capacity unit, end of data. No fill.
scripts/lto-profile.sh --quick \
    --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
    --i-will-lose-the-cartridge EW7VWMVKF6

# Hours: the same, then the fill past early warning to the hard end. Run it inside tmux.
scripts/lto-profile.sh \
    --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
    --i-will-lose-the-cartridge EW7VWMVKF6 \
    --note "second FUJIFILM cartridge of the batch bought 2026-09"
```

`--device` is the drive's non-rewinding by-id name; there is no default (`/dev/nstN` numbers
move across reboots). `--i-will-lose-the-cartridge` is the cartridge's **medium serial** as MAM
reports it (`sg_read_attr <sg> | grep -i 'medium serial'`), not its barcode label; the script
reads MAM and refuses unless the two are equal, before it writes anything. That is ADR-0008's
consent shape: you name the thing you are destroying.

Before you start, on a host that runs tapectl:

1. Stop the daily drive poll for the duration: `sudo systemctl stop tapectl-drive-poll.timer`
   (start it again afterwards). The script refuses while it is active, because the poll opens
   the drive's sg node and may read TapeAlert.
2. Check that no tapectl command holds the drive: `tapectl-op drive status` names a holder.
   The script also takes `/tmp/tapectl-tape.lock`, the lock the gate and the suites take.
3. Keep the host quiet (`docs/operator-guide.md`, "A quiet host while the tape runs"): a
   steady feed is what a fill measures. The fill's feed is a 2 GiB keystream file read from the
   out directory (`--out`, default `/scratch/tapectl-lto6`, else `~/.cache/tapectl/lto6`; the
   script refuses without room for the file and 512 MiB). It is not read from `/dev/shm`.

Options that matter:

| Option | Meaning |
|---|---|
| `--quick` | Inventory and measurements A-E; no fill. |
| `--fill-only` | The fill without the measurements (what `scripts/lto6-fill.sh` runs). |
| `--no-hard-end` | Skip the walk to the hard end. The walk writes into the drive's end-of-partition reserve (about 99 GB on an LTO-6), capped at `--hard-end-cap-gb` (110). |
| `--stop-at-warning` | Stop at the first refusal; write nothing after it. |
| `--payload-mb N` | Per-run payload of the throughput and MAM measurements (2048). |
| `--note TEXT` | A line stored in the report. |
| `--out DIR` | Where the run directory goes. |

The exit status is 0 when done; 1 for a finding (the fill's readback failed, or a read past end
of data returned data); 2 when a precondition failed and nothing was touched; 3 to 6 are the
fill writer's own codes (3 interrupted, 4 no refusal before the runaway cap, 5 a write error
that is not a refusal, 6 an unexpected error in the writer; partial results are kept). On
mhvtl, expect 1: its readback fails past capacity (below).

## What each measurement takes

| | Measures | Source | Takes (LTO-6) |
|---|---|---|---|
| inventory | vendor, product, firmware, drive serial, supported log pages, READ BLOCK LIMITS, the cartridge's MAM (generation, maker, serial, wear), the wear counters as found | `sg_inq`, sysfs, `sg_read_attr`, `sg_logs` 0x00 0x02 0x03 0x0c 0x17 | seconds |
| A. block size | whether the drive and the st driver accept 512 K and 1 M blocks, and the throughput of incompressible data, two passes in alternating order | `mt setblk`, `dd`; the source is read from tmpfs so the disk is not measured | 4 writes of the payload, about 20 s each, plus rewinds |
| B. compression | whether hardware compression is on as found (tapectl turns it off per write) | `sg_modes --page=0x0f`, the DCE bit | seconds |
| C. LBP | the logical block protection method the drive reports | `sg_modes --page=0x0a,0xf0` (MODE SENSE only; MODE SELECT is not attempted, because enabling LBP changes the block format of every later command) | seconds |
| D. MAM capacity | the unit of MAM's remaining-capacity attribute (MiB or MB) and its over-report, from the decrease of remaining capacity against the bytes written and the growth of page 0x17's used native capacity | `sg_read_attr` and `sg_logs` 0x17 before and after one payload write | one payload write |
| E. end of data | a forward read past end of data must error, not return stale data (the chain walk's assumption) | `mt fsf 3`, then a `dd` read past the third filemark | seconds |
| F. the fill | the bytes accepted before the first write refusal (early warning); what write(2) returns after it; whether a 1 GiB slice, a filemark, a seal-sized block and a filemark are accepted; the room from the warning to the hard end; a readback by LOCATE | one process holding the device open from BOT (`scripts/lto6-fill-writer.py`), page 0x0c, MAM | hours to the warning; minutes after |

Every log page is read with `--maxlen`, one LOG SENSE per page, and never page 0x2E (TapeAlert
may be read-to-clear; only tapectl's own sweep reads it).

## The run directory

`<out>/profile-<UTC timestamp>-<medium serial>/`:

| File | What |
|---|---|
| `profile.json` | The report. Machine-readable; format `tapectl-lto-profile`, version 1 (below). |
| `PROFILE.txt` | The same figures as plain lines, each with its source. The page to read. |
| `profile.log` | The run's log, with timestamps. |
| `meta.json`, `facts.tsv`, `blocksize.tsv` | What the script recorded about the run (options, host, timings, the throughput table) for the report builder. |
| `inq.txt`, `mt-status.txt`, `block-limits.txt`, `logs-00.txt`, `modes-0f.txt`, `modes-0a-f0.txt`, `eod-read.txt`, `setblk-*.txt`, `dd-*.txt` | Raw command output. A summary without the raw capture cannot be re-examined once the tape is unloaded. |
| `mam-preflight.txt`, `snap-<tag>.mam.txt`, `snap-<tag>.page_0xNN.txt`, `.bin` | MAM and log-page snapshots. `snap-asfound` is read before this run writes anything; `snap-final` after the last operation. |
| `fill/` | The fill's own files: `result.json`, `readback.json`, `writer.log` (every attempt of the post-warning slice), `summary.txt`, snapshots `before`, `at-warning`, `after`, `after-rewind`, `dmesg-tail.txt`. |

`scripts/lto-profile.sh --report RUN_DIR` rebuilds `profile.json` and `PROFILE.txt` from the
raw captures. The whole directory is the record: send it back whole.

## The report

`profile.json` has these top-level keys:

| Key | Content |
|---|---|
| `format`, `format_version` | `"tapectl-lto-profile"`, `1`. `--emit` reads only this version. |
| `tool`, `run` | The script, the tapectl commit it ran from, the run id (the directory name), mode (`full`, `quick`, `fill-only`), start and finish, host, device, the note, the options, the fill writer's exit codes. |
| `drive` | `vendor`, `product`, `firmware`, `serial`, `generation` (the drive's own, from the product string), and `key`: `vendor\|product\|firmware\|generation`. The key a profile belongs to (item 97). |
| `medium` | `generation` and where it was read (MAM medium density code, else the format density code, else the st driver's density register), `maker` (MAM medium manufacturer: recorded, not a key), `serial`, `barcode`, `length_m`, `manufacture_date`, `nominal_capacity_bytes` (the marketed figure from the generation table). |
| `emulated` | `is_emulated` and the evidence (an mhvtl drive serial, the `linuxVTL` maker). Positively checked, never assumed. |
| `problems` | Why the pair could not be profiled (a drive that cannot write the medium, an unknown generation). `--emit` refuses a run with problems. |
| `figures` | One entry per measurement: `value`, `unit`, `source` (the log page, MAM attribute, errno or timing it came from), and extra fields. `block_size`, `block_limits`, `hardware_compression_as_found`, `lbp`, `mam_capacity_unit`, `mam_over_report`, `mam_remaining_capacity`, `eod_semantics`, `early_warning`, `post_warning_room_measured`, `post_warning_room_reported`, `drive_inventory`, `supported_log_pages`. A measurement that did not run is absent and named in `not_measured`. |
| `early_warning` | `observed`, `bytes_before_first_refusal`, `first_refusal_errno`, `fraction_of_nominal`, the host feed in MB/s, `native_used_per_data_byte`, MAM's remaining capacity at the warning. |
| `post_warning` | What write(2) returned after the first refusal; whether the slice, the filemark, the seal block and the next filemark were accepted; the readback verdict; the walk to the hard end (`bytes_past_first_refusal`, `reached_hard_end`, `stop`); the drive's own page 0x0c figure (`reported_ew_to_eop`); `room_bytes` and `room_basis`. |
| `wear` | Item 98's record: `snapshots` (as found and final) of the MAM attributes (load count, initialization count, lifetime and last-load MiB written and read, TapeAlert flags, manufacture date, the drive ring) and of log pages 0x02, 0x03, 0x0c and 0x17 as label-to-number maps, plus `drive_rewrite_counters` (pages 0x02 and 0x03 "total rewrites or rereads", 0x17's write retries and unrecovered write errors), and `end_of_tape`, the early-warning record. Recorded, not fitted: collect these across runs. |
| `not_measured` | One line for each measurement that did not run or could not be decoded. |

The room past the warning (`room_bytes`) is **the lower of** the measured walk and the drive's
page 0x0c figure ("minimum native capacity from EW to EOP"). A walk that stopped at its cap
rather than at a hard error is a lower bound, and says so. A run whose readback **failed**
does not count its walk: bytes the drive accepted past the warning are not proven readable,
and only the drive's own figure sets the room.

## The block

```bash
scripts/lto-profile.sh --emit \
    /scratch/tapectl-lto6/profile-20261104-091500-EW7VWMVKF6 \
    /scratch/tapectl-lto6/profile-20261105-083000-EW7VWMVKFX
```

prints a `[[backends.lto]]` block for that drive model on standard output (and warnings on
standard error). The runs must be of one vendor, one product, one drive generation and one
medium generation; the emitter refuses to combine others, naming both. Different firmware is
refused too, because the profile is keyed by it, unless you pass `--allow-mixed-firmware`.
The media makers are listed, not keyed.

Only drive-level keys are set (item 97): capacity stays a property of the cartridge (ADR-0010).

- **`fill_ceiling`** is the lowest, over the runs, of (bytes written before the first write
  refusal ÷ the generation's marketed capacity), less a margin (`--margin`, default 0.02),
  rounded **down** to a whole percent, and **never above the 0.97 default** (ADR-0012 item 98:
  the default stays conservative until a fitted per-cartridge wear allowance is put to the CTO).
  A derived value above 0.97 is printed in the comment ("the runs support 0.98; kept at the 0.97
  default until the wear allowance of ADR-0012 item 98 is fitted") and the key says 0.97. With no
  run that measured an early warning, it stays at 0.97 and says why. If the measurement supports
  **less** than 0.97, the ceiling comes **down**, with a warning in the block and on standard
  error: a drive that stops short of the default would otherwise abort tapes at its early warning.
- **`post_warning_bytes`** is the smallest, over the runs, of the room above. With no run that
  walked or reported a figure, the key is left out, and absent means nothing is checked.
- `name` and `device_tape` are placeholders you change (`--name` and `--device-tape` set them):
  `device_tape` defaults to the by-id path of the profiled drive.

Each value carries a comment naming the run or runs it came from, and the rules are in the
block's header. Runs on an emulator (mhvtl) are ignored for every derived value unless you
pass `--trust-emulated`, which is for rehearsing the machinery only.

An example, from the HP LTO-6 figures of 2026-09-23 (the test fixture
`tests/fixtures/lto_profile/hp_lto6_run_a`: the early warning at 2,501,995,134,976 bytes and
page 0x0c's 99,008 MB are the recorded values; the hard-end walk in it is synthetic, since the
real drive has not yet been walked to the hard end):

```toml
# tapectl LTO profile block, emitted by scripts/lto-profile.sh --emit on 2026-10-19T15:34:36Z
# drive:   HP Ultrium 6-SCSI, firmware 35GD (LTO-6 drive); medium generation LTO-6
# runs:    hp_lto6_run_a (cartridge EW7VWMVKF6, FUJIFILM, 2026-09-23)
# media makers seen (recorded, not a key, ADR-0012 item 97): FUJIFILM
# rules:   fill_ceiling = the LOWEST over runs of (bytes written before the first write refusal / the marketed
#          capacity), less a 0.02 margin, rounded DOWN to a whole percent, and NEVER ABOVE the 0.97 default
#          (ADR-0012 item 98: a higher value waits for a fitted wear allowance); 0.97 when no run measured an
#          early warning. post_warning_bytes = the SMALLEST over runs of the lower of the measured walk from
#          the first refusal to the hard end and the drive's page 0x0c 'minimum native capacity from EW to
#          EOP'.
#          Only drive-level keys are set; capacity stays a property of the cartridge (ADR-0010).
# Set name and device_tape for your drive, then paste this block into config.toml (`tapectl config check`).
[[backends.lto]]
name = "hp-ultrium-6-scsi"
device_tape = "/dev/tape/by-id/scsi-HUJ808A5L4-nst"
generation = "LTO-6"
# fill_ceiling from hp_lto6_run_a (cartridge EW7VWMVKF6, FUJIFILM, 2026-09-23): the lowest of 1 run(s) --
#   first write refusal at 1.0008 of the 2,500,000,000,000-byte marketed capacity (2,501,995,134,976 bytes),
#   less the 0.02 margin, rounded down to a whole percent: the runs support 0.98; kept at the 0.97 default
#   until the wear allowance of ADR-0012 item 98 is fitted
fill_ceiling = 0.97
# post_warning_bytes from hp_lto6_run_a (cartridge EW7VWMVKF6, FUJIFILM, 2026-09-23): the smallest of 1 run(s)
#   -- the lower of the measured walk and the drive-reported figure: measured walk 99,377,741,824 vs
#   drive-reported page 0x0c 99,008,000,000
post_warning_bytes = "99008M"
# (a slice plus the seal must fit in half of post_warning_bytes: config check and stage create refuse a larger slice_size)
```

Paste it into `config.toml` (replace the drive's existing block), then:

```bash
tapectl config check
```

## `post_warning_bytes` and the slice cap

`post_warning_bytes` (a size in **decimal** units, like `capacity_override`: `99008M`, `99G`, or a
bare byte count; above zero; `config set` validates it like the other keys and refuses a value that breaks the cap) is the room a write
has between the drive's early warning and the hard end of the medium. Tapectl uses it for one
check (ADR-0012 item 100): with the key on any configured drive, **a slice plus the seal must fit
in half of the smallest such room** (a 2x margin for wear and variance), so that a tape can still
be sealed after the warning.

- The slice is counted as it lands on tape: age encrypts it (a 16-byte tag per 64 KiB chunk and a
  header) and the writer pads it to whole 512 KiB blocks. The seal is a 16 MiB allowance (the seal
  marker with its full copy of the front index, about 170 bytes per tape file, covers a 30,000-file
  tape).
- `config check` reports a `[defaults] slice_size` or an `[[archive_sets]]` `slice_size` that does
  not fit as an `error:` line, naming the key, the drive and the numbers and the largest
  `slice_size` that does, and exits non-zero (`--json`: `slice_cap_problems`). `config set` refuses
  an edit (of `slice_size` or of `post_warning_bytes`) that would make one. `stage create` refuses
  the slice size in force for the unit (its dotfile's `[policy]`, its archive set, or `[defaults]`)
  before it touches the staging directory or dar.
- **The config still loads.** Every other command, `restore`, `volume verify`, `volume read-slices`
  and `catalog rebuild` among them, runs as before: a staging setting must never block reading a
  tape in an emergency.
- With the key absent on every drive, nothing is checked.

The default `slice_size` stays 1 GiB (ADR-0012 2026-10-06 item 10) unless the room is smaller.
For the 99 GB of an LTO-6, a 1 GiB slice is far inside half of the room. The check matters for a
drive that leaves little room, or a large `slice_size`.

## What is real on mhvtl

mhvtl lets the toolkit's mechanics run, but its figures are fiction, and the report says so for
a device it recognises (`emulated`): it is not a drive.

- MAM's capacity attributes and log pages 0x0c and 0x17 are static (the used native capacity does
  not grow, so the capacity unit cannot be derived); wear counters do not move.
- It does emulate an ENOSPC at the size the cartridge was made with, then accepts every second
  write; it keeps no data past it, so the fill's readback FAILS (exit 1). The post-warning
  figures are the emulator's.
- 1 MiB blocks are refused by the host's st driver with EBUSY although the emulated drive
  advertises 2 MiB; throughput is disk speed.
- A throwaway cartridge made with `mktape` is small. Pass `--fill-cap-bytes` (and `--chunk-gib 1
  --slice-blocks 32 --hard-end-cap-gb 1`) so a runaway cap derived from MAM's absurd maximum
  capacity does not decide how long the run goes on.

Block-size acceptance and end-of-data behaviour describe the emulator, not any drive.

## On home2, with the real drive

The real-drive run of #479 is the operator's, with the HP LTO-6 and the test cartridge
EW7VWMVKF6 (the same run as #420's, which it contains):

```bash
sudo systemctl stop tapectl-drive-poll.timer
tmux new -s profile
scripts/lto-profile.sh \
    --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
    --i-will-lose-the-cartridge EW7VWMVKF6 \
    --note "home2, HP LTO-6, test cartridge, first full profile"
sudo systemctl start tapectl-drive-poll.timer
```

About five hours. Read `PROFILE.txt`; exit 1 with "MEASUREMENT COMPLETE (readback: FAIL)" in
the fill's summary is a finding, not a failure. Send back the **whole** run directory. Erase the
cartridge afterwards before it serves as the step-12 test cartridge again.

## The old entry points

`scripts/lto6-measure.sh` and `scripts/lto6-fill.sh` remain as thin wrappers of this script
(`--quick` and `--fill-only`). Their options mean what they meant, except that
`lto6-measure.sh --erase-cartridge` takes the medium serial and `--allow-unverified-barcode` is
refused (a consent check that can be switched off is not one), and the run directory is now
`profile-<timestamp>-<serial>/`, with the fill's files in its `fill/` directory. The fill's
writer keeps its historical name, `scripts/lto6-fill-writer.py`.
