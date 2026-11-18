# 2026-11-18: the whole tape chain on `layout-3` after Stage 3

The plan (`docs/design/v3-implementation-plan.md`, Stage 3) has the coordinator run the whole
tape chain on `layout-3` after waves 3A and 3B, while it is still a Layout 2 tree, before
Stage 4 starts. This is that run.

- **Commit:** `e0940273` on `layout-3` (Stage 3 complete: one baseline schema, a 1.x catalog
  refused; one on-tape `catalog.db` shape, older tapes refused; the renamed spellings and old
  config keys gone; the scripts' 1.x one-time moves gone).
- **Host:** vm-desk1, mhvtl only (A1 and A2 ULT3580-TD8, A3 ULT3580-TD6); the production host
  and its drive were not touched.
- **Tools:** age 1.1.1, tar (GNU tar) 1.35, dar as installed; a debug build.
- **Window:** granted by the host's broker; 2026-11-18 01:24Z to 02:02Z.

| Leg | Result |
|---|---|
| build | ok |
| gate (`scripts/mhvtl-verify-gate.sh`, TD8 A2, TD6 A3) | **GREEN**, 67 of 67 checks |
| first-run reentry (A1) | **GREEN**, 35 checks, 0 failed |
| upgrade (`scripts/upgrade-mhvtl.sh`, A1) | **stopped as designed**, exit 2 before any drive access: the newest tag before HEAD is v1.9.2, and there is no upgrade from 1.x (item 258(c), T3.4). Its first real run is the release after 2.0.0 |
| short seal `--mhvtl` (A3) | **PASS** |
| span `--mhvtl` (A3) | **PASS** |
| plaintext scan | **PASS** |
| lifecycle `--all` (A2) | **GREEN**, 487 checks: 472 passed, 0 failed, 15 skipped; all 17 scenarios |

The figures equal the run on `master` after Stage 2 (`docs/runs/2026-11-17-stage-2-chain.md`),
so removing compatibility changed nothing the tape shows.
