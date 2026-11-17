# 2026-11-17: the whole tape chain on master after Stage 2

The plan (`docs/design/v3-implementation-plan.md` §3, Stage 2) has the coordinator run the
whole tape chain on `master` after Stage 2's third wave, because T2.1 and T2.2 change the
write path and only the tape proves that. This is that run.

- **Commit:** `9e3e0829` (Stage 2 complete: fifteen fixes in three waves, ADR-0012 item 292's go).
- **Host:** vm-desk1, mhvtl only (A1 and A2 ULT3580-TD8, A3 ULT3580-TD6); the production host and its drive were not touched.
- **Tools:** 1.1.1, tar (GNU tar) 1.35, dar as installed on the host; a debug build.
- **Window:** granted by the host's broker; 2026-11-17 19:56Z to 21:05Z.

| Leg | Script | Result |
|---|---|---|
| build | `cargo build`, `--example short_seal_filler` | ok |
| gate | `scripts/mhvtl-verify-gate.sh` (TD8 A2, TD6 A3) | **GREEN**, 67 of 67 checks, 0 expected failures |
| first-run reentry | `scripts/first-run-reentry-mhvtl.sh` (A1) | **GREEN**, 35 checks, 0 failed (#414, #432) |
| upgrade | `scripts/upgrade-mhvtl.sh` (A1), v1.9.2 to the candidate | **GREEN**, 18 checks, 0 failed (#505) |
| short seal | `scripts/short-seal-acceptance.sh --mhvtl` (A3) | **PASS**, steps 1 to 7 |
| span | `scripts/span-acceptance.sh --mhvtl` (A3) | **PASS**, steps 1a to 6 |
| plaintext scan | `scripts/plaintext-scan.sh` | **PASS**; positive control 4 slices, canary visible |
| lifecycle | `scripts/lifecycle-suite.sh --all` (A2) | **GREEN**, 487 checks: 472 passed, 0 failed, 15 skipped; all 17 scenarios |

The lifecycle figures equal the 1.9.x baseline (472 / 0 / 15). On mhvtl every write warns that
Logical Block Protection is unavailable on the TD6 at the full block size and that the feed
ratio is above threshold; both are mhvtl's, recorded before (CLAUDE.md, 1.7.2 and 1.1.2).

The driver was a scratch script; each leg's log was kept until this record was written and
then removed with the run's scratch homes.
