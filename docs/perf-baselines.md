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

When you add a new baseline row, keep the date and the short commit
context so future regressions have something to diff against.
