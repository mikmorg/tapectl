# tapectl Performance Baselines

These numbers come from `tests/performance.rs`, which is gated behind
`TAPECTL_PERF_TESTS=1`. They are a regression signal, not a target — a 2x
slowdown between runs means something changed.

## How to reproduce

```bash
TAPECTL_PERF_TESTS=1 cargo test --test performance --release -- \
    --ignored --nocapture --test-threads=1
```

Scenarios run disk-side only (no tape hardware). The design document's
production-scale targets (2+ TB units, 100K+ files) require real LTO
hardware and are covered by `lto6-validation-checklist.md` when that
exists.

## Scenario defaults

| scenario                 | knob                        | default |
|--------------------------|-----------------------------|---------|
| `perf_many_files_*`      | `TAPECTL_PERF_FILES`        | 5000    |
| `perf_many_units_audit`  | `TAPECTL_PERF_UNITS`        | 500     |
| `perf_large_single_file` | `TAPECTL_PERF_LARGE_MB`     | 500     |
| `perf_fresh_catalog_*`   | `TAPECTL_PERF_CATALOG_FILES`| 184552  |
| `perf_staging_throughput_per_pass` | `TAPECTL_PERF_STAGE_MIB` | 4096 |
| `perf_staging_throughput_per_pass` | `TAPECTL_PERF_STAGE_SMALL` | 4000 (small files, together a quarter of the source) |
| `perf_staging_throughput_per_pass` | `TAPECTL_PERF_STAGE_SLICE` | 1G (dar `-s`, the 1.1 default) |
| `perf_staging_throughput_per_pass` | `TAPECTL_PERF_HASH_THREADS` | unset: the config default, 1 |

The large-file scenario caps at 500 MiB because this dev VM's /scratch
partition holds 100 GiB — the 2+ TB figure in the design doc's M7
checklist is only reachable on the real archival box. When running on
that hardware, bump `TAPECTL_PERF_LARGE_MB` and record a new row.

## Baselines

Environment: libvirt VM `vm-desk1`, 4 vCPU, 3 GiB RAM, /scratch on a
single virtio disk. Rust release build, dar 2.6.x, mhvtl not involved.
Single run — treat the numbers as rough.

### 2026-04-12 — master @ post-Phase-7

| scenario                 | step                        | time     | notes                     |
|--------------------------|-----------------------------|----------|---------------------------|
| many_files (5000)        | create files on disk        | 0.54 s   |                           |
| many_files (5000)        | init_unit + snapshot_create | 31.84 s  | per-file sha256 dominates |
| many_files (5000)        | stage_create (dar + age)    | 45.28 s  |                           |
| many_units (500)         | bulk insert                 | 0.04 s   | single transaction        |
| many_units (500)         | audit core loop             | 0.06 s   | ~0.12 ms/unit             |
| large_file (500 MiB)     | write source                | 1.52 s   |                           |
| large_file (500 MiB)     | init_unit + snapshot_create | 0.59 s   |                           |
| large_file (500 MiB)     | stage_create                | 26.21 s  | 19.1 MiB/s end-to-end     |

### 2026-10-06 — the source hash across files (issue #366)

Environment: `vm-desk1` (16 vCPU, 9 GiB RAM, shared with other jobs), a
**debug** build, the tree in the page cache. The tree is 1,500 random files of
256 KiB (375 MiB) in 119 directories. The hash pass alone is `hash_files` with
no dar beside it; `stage create` is the whole command.

| step | `hash_threads` | time |
|---|---|---|
| hash pass alone | 1 | 4.47 s, 3.11 s |
| hash pass alone | 4 | 0.84 s, 0.75 s |
| hash pass alone | 8 | 0.41 s |
| `stage create` | 1 | 189.8 s, 110.1 s |
| `stage create` | 4 | 117.1 s, 105.1 s |

The hash pass scales with the threads (about 4x at 4). The whole stage
barely moves here because a debug build's age encryption, about 3.5 MiB/s,
is the slowest part. On home2's release binary the single-core sha256
(~123–163 MB/s) is what holds dar back, so that is where the gain shows.
The figure #366 asks for, a many-file unit on home2 at 1/2/4/8 threads, is
still owed.

Beside dar, each hasher thread reads only within 1 GiB of dar at its own
place in dar's order (1 GiB when these were measured; 128 MiB since #441), and dar is held to the thread furthest behind (so the
source is still read from disk once). The threads therefore overlap only
over files that fit in that window: these 256 KiB files do, a unit of a few
multi-GiB files is hashed about one file at a time.

### Observations

- **Snapshot creation for many-files is surprisingly expensive** (~32 s
  for 5000 files). Per-file sha256 plus DB inserts is the dominant
  term. If this gets worse, the culprit is likely the per-file INSERT
  path in `snapshot_create` — check whether it's batched.
- **Staging throughput caps at ~19 MiB/s** on the VM. That's a function
  of dar's single-threaded streaming plus age encryption. Real hardware
  should do materially better; confirm on LTO-6 when available.
- **Audit scales linearly and is cheap** — 500 units takes 60 ms. The
  design's "500+ units" target is nowhere near the DB's limits.

### 2026-10-06 — branch `schema` (issue #413), vm-desk1, release build

`perf_fresh_catalog_takes_a_184k_file_version` (knob
`TAPECTL_PERF_CATALOG_FILES`, default 184,552, L6-0001's file count): one
version's file list into a catalog fresh from `db::open`, the write a DR
`catalog rebuild` and a new install's first `snapshot create` both make
through `db::files::insert_version`. Its ceiling is 10 s at the default size.

| scenario                 | step                        | time     | notes                     |
|--------------------------|-----------------------------|----------|---------------------------|
| fresh catalog (184,552)  | file list insert            | 5.6–8.3 s | one `INSERT … SELECT` from a TEMP table; the VM was shared with other builds |
| fresh catalog (184,552)  | `catalog.db` build          | 1.41 s   |                           |
| fresh catalog (184,552)  | `catalog.db` read           | 0.37 s   |                           |

The same insert row by row (one statement per row, as before #413), measured
on the same schema: 4.86 s at 50,000 rows and 157.7 s at 184,552, against
1.20 s and 5.55 s in one statement. Each per-row statement opened a savepoint
at which FTS5 flushed its pending index, so every path became its own index
segment plus merges. Deleting the `paths_fts_data` row `PRAGMA optimize`
leaves in `sqlite_stat1` made no difference to the one-statement insert
(5.9–6.5 s either way).

### 2026-10-07 — branch `schema2` (ADR-0012 amendment 2026-10-07 item 10), vm-desk1

`perf_rebuild_from_a_tape_carrying_a_184k_file_version` (same knob, same
default): the real write session puts one 184,552-row version on a
`MemStore` tape, its `catalog.db` (22 MiB) inside the operator envelope, then
`volume::rebuild::rebuild_from_store` rebuilds it into a fresh catalog with
the operator key. Its one ceiling is 20 s on the whole rebuild at the default
size, in the release build the suite runs in.

| scenario                 | step                                   | debug   | release | notes |
|--------------------------|----------------------------------------|---------|---------|-------|
| rebuild (184,552)        | fixture: insert + `catalog.db` + write | 23.37 s | 10.98 s | reported, not bounded |
| rebuild (184,552)        | `rebuild_from_store`, whole            | 17.40 s | 5.26 s  | ceiling 20 s; VM shared with other builds |

### 2026-10-12 — branch `budgets-416` (issue #416 items 9 and 11), vm-desk1, release

Two scenarios that bound ratios rather than wall clock.
`perf_a_2gib_write_and_full_confirm_through_a_store` (knob
`TAPECTL_PERF_WRITE_MIB`, default 2048) runs the real write session and a full
confirm into a `MemStore`, against an in-test sha256 calibration; ceiling 3.0x
two hash passes. `perf_a_100k_file_unit_scales_linearly` (knob
`TAPECTL_PERF_UNIT_FILES`, default 100,000) snapshots and stages the unit and
a tenth of it; per step, statements ≤ entries + 50 (entries: files, their
directories and the dotfile), at most 20x the time for 10x the files, and
VmHWM ≤ 512 MiB.

| scenario            | step                         | time    | notes |
|---------------------|------------------------------|---------|-------|
| write 2 GiB         | sha256 calibration           | —       | 101 MiB/s (no SHA extensions on this VM) |
| write 2 GiB         | validate → seal              | 18.81 s | 0.93x one hash pass |
| write 2 GiB         | + full confirm               | 33.27 s | 0.82x two passes |
| 10,000-file unit    | `snapshot_create`            | 0.30 s  | 10,025 statements |
| 10,000-file unit    | `stage_create`               | 4.68 s  | 10,030 statements |
| 100,000-file unit   | `snapshot_create`            | 3.63 s  | 100,115 statements (100,101 entries); 12.1x for 10x |
| 100,000-file unit   | `stage_create`               | 44.81 s | 100,030 statements; 9.6x for 10x; VmHWM 87 MiB |

### 2026-10-16 — staging throughput, per pass (epic #364), vm-desk1, release

`perf_staging_throughput_per_pass` stages one unit through the library's real
`snapshot create` and `stage create` and prints the whole run and each phase
in MiB/s (1 MiB = 1,048,576 bytes) over the source's size, the phases read
back from the `phase_timings` rows `stage create` records (not timed by the
test), the bytes the run read from disk, dar included, as a multiple of the
source, and the bytes it wrote. It asserts no timing: the stage succeeds, the
`archive` phase is recorded, and the disk reads are between half the source
and 1.5x it plus 128 MiB. Run on its own it is

```bash
TAPECTL_PERF_TESTS=1 TAPECTL_PERF_SCRATCH=/scratch/tapectl-bench/perf \
    cargo test --test performance --release -- --ignored --nocapture \
    --test-threads=1 perf_staging
```

The first run generates the fixture under `TAPECTL_PERF_SCRATCH`
(`stage-fixture-<MiB>m-<small>f/`) and later runs reuse it while its `READY`
marker, file count and sizes agree with the knobs.

**Environment.** `vm-desk1`: a KVM guest with 16 vCPUs presenting a Westmere
E56xx/L56xx/X56xx (`aes`, `ssse3`, `sse4_2`; no `avx`, no `sha_ni`), 9.7 GiB
RAM (7.9 GiB available), Linux 6.8.0-142, dar 2.7.13, rustc 1.94.1. Source and
staging share one disk: `/scratch` is ext4 on one virtio disk (`/dev/vdb`).
The guest runs on home2's hardware, so the CPU-bound parts are comparable to
home2; its disk is not home2's staging array (a LUKS RAID0 of two desktop
drives). Every version below is a release build of its own tag
(`cargo build --release --bin tapectl`, each in its own worktree and target
directory), not the library test binary.

**Fixture.** 4,003 files, 4,294,569,268 bytes (4095.6 MiB): three large files
of 1 GiB and 4,000 small ones of 64 to 448 KiB (a quarter of the total),
in 40 directories, xorshift64\* bytes, so incompressible and the same on every
machine. `compression = "none"`, `slice_size = "1G"`, `hash_threads` as shipped
(1) unless a row says 4, one tenant, the fixture registered as one unit,
a fresh home and an empty staging directory for every run.

**Method.** Each command ran as `/usr/bin/time -f '%e %U %S %I %O %M'` with
the fixture evicted from the page cache first: `sync`, then
`posix_fadvise(POSIX_FADV_DONTNEED)` on every fixture file (no root, and no
one else's pages are dropped). `mincore(2)` counted 0.0 of 4095.6 MiB resident
after each eviction, in every run (the positive control). "Disk reads" is `%I`
(the process tree's block reads, dar included) over the source size, so a
second read of every byte is 2.0x and a first read only is 1.00x. Three runs
per row; the table gives the median and, in brackets, the range. Every
measured section ran under `flock` on the shared build lock, so no cargo job
from another worker ran during it (when this task began another worker's test
run held the lock at a load of 15; it had finished before the first measured
run).
The 1-minute load just before a run was 1.4 to 3.6, the previous run's tail;
during a run it is the run itself, about 3 cores. The `ps` top five before each
run showed only idle `claude` sessions, the driver's own `python3` and `ps`,
and in one record a transient `gh`. `/usr/bin/time` rounds to 10 ms but these
runs' times fall on half seconds, so read differences under a second as noise.

| version | snapshot s | stage s | total s | total MiB/s | stage disk reads | stage written MiB | stage CPU s |
|---|---|---|---|---|---|---|---|
| v1.0.0 | 24.7 (23.7-25.6) | 204.1 (197.5-204.4) | 228.1 (222.2-229.7) | 18.0 (17.8-18.4) | 2.78x | 8198 | 192 |
| v1.1.0 | 0.12 (0.10-0.15) | 89.0 (87.5-92.0) | 89.1 (87.7-92.2) | 45.9 (44.4-46.7) | 1.80x | 4099 | 242 |
| v1.1.1 | 0.10 (0.09-0.10) | 84.0 (82.0-84.5) | 84.1 (82.1-84.6) | 48.7 (48.4-49.9) | 1.00x | 4099 | 245 |
| v1.2.0 | 0.15 (0.10-0.15) | 85.0 (81.5-85.0) | 85.1 (81.7-85.2) | 48.1 (48.1-50.1) | 1.00x | 4099 | 245 |
| v1.1.0, `hash_threads = 4` | 0.10 (0.10-0.15) | 90.0 (90.0-91.5) | 90.2 (90.1-91.6) | 45.4 (44.7-45.4) | 1.91x | 4099 | 252 |
| v1.2.0, `hash_threads = 4` | 0.10 (0.10-0.15) | 85.5 (85.0-86.0) | 85.7 (85.1-86.1) | 47.8 (47.5-48.1) | 1.00x | 4099 | 255 |
| the new test, library of this tree (v1.2.0 plus docs) | 0.14 (0.13-0.14) | 85.1 (84.6-85.6) | 85.2 (84.8-85.7) | 48.0 (47.8-48.3) | 1.00x | 4100 | n/a |

The phases `stage create` records, in seconds (median), from `phase_timings`.
v1.0.0 records none (no phase log), so its figure is the total above. The
`archive` phase is dar, the source hash, the age encryption and the slice
hashes together, one overlapped pass; its MiB/s counts the dar stream
(4096.8 MiB).

| version | check | archive | archive MiB/s | recheck | catalog | finalize |
|---|---|---|---|---|---|---|
| v1.1.0 | 0.05 | 88.3 | 46.4 | 0.03 | 0.27 | 0.04 |
| v1.1.1 | 0.05 | 83.0 | 49.3 | 0.03 | 0.28 | 0.04 |
| v1.2.0 | 0.08 | 83.5 | 49.0 | 0.03 | 0.85 | 0.04 |
| v1.1.0, `hash_threads = 4` | 0.05 | 89.5 | 45.8 | 0.03 | 0.28 | 0.04 |
| v1.2.0, `hash_threads = 4` | 0.05 | 84.2 | 48.6 | 0.03 | 0.84 | 0.04 |
| the new test | 0.08 | 84.0 | 48.8 | 0.03 | 0.86 | 0.04 |

What moved. From 1.0.0 to 1.1.0 the whole cycle went from 228 s to 89 s (2.6x):
the serial pre-dar hash pass and the plaintext archive on the staging disk are
gone (#370), which halves the bytes written (8.2 to 4.1 GiB) and cuts the disk
reads from 2.78x to 1.80x the source (the 2.78x includes reading the plaintext
archive back); `snapshot create` also fell from 25 s to 0.1 s in that release
(1.0.0 inserted the file list row by row and wrote 235 MiB to the catalog for
these 4,003 files; it reaches the catalog in one statement since #413). From 1.1.0 to 1.1.1 the disk reads
fall to 1.00x (#441: dar's `fadvise` is neutralised, so the pages the hasher
read are still there when dar asks), but the time moves only from 89 to 84 s,
because on this VM the disk is not what staging waits for; where the disk is
slower the read it saves should cost more seconds, which is the figure home2
owes. 1.2.0, which carries 1.1.2's fixes, adds no pass: the same 85 s, and the
`catalog` phase grew from 0.3 to 0.85 s, which is the check of dar's catalogue
against what was hashed (#464).

`hash_threads = 4` (#366's pool) changes nothing here: 85.5 against 85.0 s on
1.2.0, and 90.0 against 89.0 s on 1.1.0. Two diagnostic runs of 1.2.0 (not in
the table) sampled the threads of `stage create` in three 15 s windows each,
from 25 s into the archive phase: no thread of tapectl or dar exceeded about
75% of a core, up to five were busy at once (about 3 cores in all, 245 CPU s
over 85 s), and in the second run `/dev/vdb` was 20 to 32% busy at 44 to
59 MiB/s read and 9 to 68 MiB/s written. Staging at 48 MiB/s on this
VM is therefore limited neither by one core nor by the disk but by the
hand-offs between dar, the source hasher and the encrypter, which this benchmark
does not break down. The measurements are per release, not per child issue: the
other children cannot be separated from the tags above, and 1.0.0 is the
"before every child" row.

**Owed.** home2's own figure (Xeon L5640, no SHA extensions, staging on the
LUKS RAID0) is not measured here and comes with its next production staging run,
using this scenario at its defaults or the `tapectl` rows above; so does the
re-estimate of the first 1.3 TB tape's staging time that #364's third criterion
asks for. For scale only: at this VM's 48 MiB/s that tape would take about 7.2 h
and at 1.0.0's 18 MiB/s about 19 h, against the epic's 16 to 18 h estimate for
home2 on 1.0.0. The per-run raw results and the cross-version driver stay on
vm-desk1 under `/scratch/tapectl-bench/`; the old tags' binaries are not part
of the repository, so the in-tree scenario is the repeatable half.

When you add a new baseline row, keep the date and the short commit
context so future regressions have something to diff against.
