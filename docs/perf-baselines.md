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

When you add a new baseline row, keep the date and the short commit
context so future regressions have something to diff against.
