# Holistic Review: Tests, CI-Readiness & Operability

**Date:** 2026-07-20
**Ticket:** [Holistic review: tests, CI-readiness & operability](https://github.com/mikmorg/tapectl/issues/5) (wayfinder map [#1](https://github.com/mikmorg/tapectl/issues/1))
**Method:** single auditor, empirical: full `cargo test` executed and timed on the dev VM; every ungated test binary re-run under `env -i` (no `PATH`, no `HOME`) to prove hermeticity; `cargo fmt --check`, `cargo clippy --all-targets`, `cargo audit`, and the `gen_man` drift procedure run against the working tree; local mhvtl/dar/sg3-utils state inspected; GitHub state read via `gh` (read-only); external package facts verified against packages.ubuntu.com and github.blog/docs.github.com. Severity weighted per the renovation intent: untested restore/heir paths heaviest, polish light. Prior-audit findings ([2026-07-18 code-quality drift](2026-07-18-code-quality-drift.md), H1–H13) are cross-referenced, not re-reported; this audit's question is whether the *test/CI/operability apparatus* would catch them.
**Verdict scope:** all `cargo test` numbers, tool versions, and drift checks are from real runs on 2026-07-20 (worktree at `5308179`, only the pre-existing uncommitted `CLAUDE.md` modification present, left untouched). Gated mhvtl suites could not be executed — that inability is itself finding T2.

---

## Executive summary

The ungated test suite is in unusually good *mechanical* shape for CI: 106 tests pass in ~2.7 s of execution, fully parallel-safe, and — proven by re-running every test binary with an empty environment — they need **zero external binaries, no `$HOME`, no tape device, and no network**. A hosted-runner CI job needs no `apt install` at all to go green on `cargo test` today. Man pages have zero drift. `fmt --check` is clean.

But the audit's central conclusion mirrors the prior audit's:

> **The properties the project weighs heaviest — restore, and above all heir-restore via RESTORE.sh — are exactly the ones the test suite cannot see. The ungated suite proves the SQLite schema and the crypto primitives; everything between `stage create` and a restored file is tested only by six `#[ignore]`d mhvtl tests that currently cannot run on any machine, and RESTORE.sh has never been executed by anything, anywhere.**

Totals: **3 HIGH**, **11 MED**, **7 LOW**.

### Theme 1 — The heir path is generated, broken, and unwatched (T1, T14)

`RESTORE.sh` is written to every tape and asserted only by `contains()` substring checks. No test, script, or checklist item executes it. Prior-audit H1/H2 (mini-index omits envelopes; RECOVERY.md commands don't work) shipped and stayed green precisely because of this. Issue #7's gate spec exists on paper; `scripts/` does not exist in the repo.

### Theme 2 — The only end-to-end coverage is currently unrunnable (T2, T12)

`volume/write.rs` (1,128 lines), `volume/restore.rs`, `staging/mod.rs`, the dar wrappers, and `tape/ioctl.rs` have zero in-module tests; their only coverage is the mhvtl-gated suite. On this VM — the only machine that can run it — the mhvtl userspace and tape media survive under `/usr/bin/vtltape` and `/opt/mhvtl`, but **no `mhvtl.ko` exists for any of the three installed kernels** and `/dev/nst0` is gone: kernel updates silently killed the gate. Every commit since then has landed with the write→restore pipeline entirely untested.

### Theme 3 — The integration suite tests the database, not the program (T3)

30 of 33 tests in `tests/integration.rs` hand-write SQL for both the fixture *and the behavior under test*. `test_encryption_key_rotation` re-implements rotation in SQL — so it stays green while the real `key rotate` strands tenants (prior H13). `test_archive_set_policy_inheritance` inserts `archive_set_id` by hand — masking the fact that no production code path ever writes it (prior MED). The suite would pass if the entire CLI were deleted.

### Theme 4 — CI would fail today for the wrong reason (T4, T5)

On current stable (1.94.1), `cargo clippy --all-targets` emits 12 warning sites — the intended `-D warnings` gate is red on day one, and `CLAUDE.md`'s "zero clippy warnings" is stale toolchain drift (no `rust-toolchain.toml`, no `rust-version`). `cargo audit` fails outright: quick-xml 0.39.2 carries two 7.5-HIGH RUSTSEC advisories — and its only consumer is the dead `catalog_xml` module.

### Theme 5 — Operability is command-complete but signal-blind (T9, T11, T13)

`volume verify` with failed slices and `db fsck` with integrity FAIL both exit 0 — the two commands a monitoring cron would watch are invisible to it. A `tracing` subscriber is never installed, so every `info!`/`warn!` in ten source files (including the crash-recovery sweep warnings) is silently dropped. The operator guide covers commands well but contains no cadence: no restore-drill schedule, no verify rotation, and an mhvtl install instruction (`apt install mhvtl`) that is impossible on Ubuntu.

### Consolidated finding table

| ID | Sev | Finding | Where |
|----|-----|---------|-------|
| T1 | HIGH | RESTORE.sh is never executed by any test or script; only substring asserts | `src/volume/layout.rs:846-864`, `tests/mhvtl_e2e.rs`, `scripts/` (absent) |
| T2 | HIGH | Restore/write pipeline has zero runnable coverage: gated-only, and mhvtl kernel module is missing for all installed kernels | `tests/mhvtl_e2e.rs`, `/lib/modules/6.8.0-{117,134,136}-generic/` |
| T3 | HIGH | Integration suite is raw-SQL schema testing; simulates behavior instead of calling it, masking known defects | `tests/integration.rs` |
| T4 | MED | `clippy -D warnings` fails on current stable (12 sites); no toolchain pin, no MSRV; CLAUDE.md claim stale | `src/cli/report.rs`, `src/cli/archive_set.rs`, `tests/failure_modes.rs`, `Cargo.toml` |
| T5 | MED | `cargo audit`: two 7.5-HIGH advisories in quick-xml 0.39.2, whose only consumer is dead code | `Cargo.toml:29`, `src/dar/catalog_xml.rs` |
| T6 | MED | Design §10 M7 checks off failure-mode tests that don't exist (interrupted write, ENOSPC, raw-volume, corrupted staging) | `tapectl-design-v4_0.md:2025-2026`, `tests/failure_modes.rs` |
| T7 | MED | Zero CLI-layer tests: no clap parse assert, no process-level exit-code test, no `--json` schema pin | `src/cli/` (18 files, 0 tests), `src/main.rs` |
| T8 | MED | Migration testing gap: three fixtures apply migration 001 only; integration setup bypasses the migration runner; no upgrade-path test | `tests/integration.rs:48-58`, `src/staging/validate.rs:81`, `src/policy/mod.rs:158`, `src/db/queries.rs:508` |
| T9 | MED | `volume verify` failures and `db fsck` FAIL exit 0; design §5's warning exit code implemented only by `audit` | `src/cli/volume.rs:149-166`, `src/main.rs:181-198` |
| T10 | MED | Silent-skip pattern: gated perf tests report `ok` when skipped — 3 of the headline 106 are vacuous | `tests/performance.rs:114-116,170,255` |
| T11 | MED | No tracing subscriber ever installed — all runtime logging, incl. crash-recovery warnings, is dropped | `src/main.rs` (absent init), `src/db/mod.rs:61-63` |
| T12 | MED | Operator guide's `apt install mhvtl` is impossible on Ubuntu (unpackaged); no kernel-module maintenance runbook — root cause of T2's breakage | `docs/operator-guide.md:13` |
| T13 | MED | No operator cadence runbook: restore drills, verify schedule, tape rotation absent; LTO-6 checklist an acknowledged stub | `docs/operator-guide.md`, `docs/lto6-validation-checklist.md:5-6` |
| T14 | MED | Generated-recovery-artifact tests assert substrings, not validity: the 2-unit planning-header test would fail under `toml::parse` today | `src/volume/layout.rs:786-799` vs `558-574` |
| T15 | LOW | No `.gitignore` in the repo | repo root |
| T16 | LOW | LICENSE file missing; README §License and Cargo.toml `license="MIT"` point at nothing; GitHub license = null | `README.md:199-201`, `Cargo.toml:6` |
| T17 | LOW | No tags, no CHANGELOG, no branch protection, no issue/PR templates; version pinned at 0.1.0 since first commit | repo/GitHub state |
| T18 | LOW | README "Rust 1.75+" claim unverified; no `rust-version` key | `README.md:20`, `Cargo.toml` |
| T19 | LOW | `--json` depth inconsistencies: `db import` ignores it, `db export` is unconditionally JSON | `src/main.rs:200-230` |
| T20 | LOW | Determinism footnotes: process-local tape lock, `/scratch` hardcodes in gated harnesses, shared `/tmp` path string in fixture config | `tests/mhvtl_e2e.rs:37-42,77`, `tests/performance.rs:52`, `tests/integration.rs:27` |
| T21 | LOW | `config check` is parse-only: no dar/staging/device existence or sanity checks | `src/main.rs:265-287` |

---

## Measured baseline

This section is the spec input for the CI ticket. All numbers from real runs on 2026-07-20.

### Test run (ungated, debug)

Command: `cargo test` (debug profile, per house build rule). Result: **106 passed, 0 failed, 6 ignored**, wall time **55.4 s** including compilation on a warm dependency cache (libvirt VM, 4 vCPU / 3 GiB; `rustc 1.94.1`). Pure test execution is **≈2.7 s**:

| Target | Tests | Time | What it actually covers |
|---|---|---|---|
| lib unit tests (`src/lib.rs`) | 60 | 0.32 s | 10 modules: crypto/keys 13, health parsing 8 (real sg_logs fixtures), layout generators 8, db/queries 8, dotfile 4, validate 4, policy resolver 5, catalog_xml 5 (dead module), unit auto-name 3, db open 2 |
| bin unit tests (`src/main.rs`) | 0 | 0.00 s | nothing |
| `tests/integration.rs` | 33 | 1.62 s | schema/constraints/FTS5 via raw SQL; only 3 tests call `tapectl::` code (audit-event + crash-recovery) |
| `tests/failure_modes.rs` | 5 | 0.72 s | age misuse/tampering, DB crash-recovery sweep via real `db::open` |
| `tests/tenant_isolation.rs` | 5 | 0.02 s | crypto isolation boundary, age header magic pin |
| `tests/performance.rs` | 3 "passed" | 0.00 s | **vacuous** — self-skip when `TAPECTL_PERF_TESTS` unset |
| `tests/mhvtl_e2e.rs` | 0 (6 ignored) | 0.00 s | `#[ignore]` + runtime gate on `TAPECTL_MHVTL=1` and `/dev/nst0` |
| doc-tests | 0 | — | none exist |

Honest headline: **100 meaningful ungated tests** (CLAUDE.md's "106" counts the 3 vacuous perf passes and is otherwise accurate).

### Hermeticity (empirical)

All four ungated test binaries re-run with `env -i` (no `PATH`, no `HOME`, empty environment): 60 + 33 + 5 + 5 all pass. The ungated suite therefore invokes **no external binaries**, reads **no user home**, touches **no device nodes**, and needs **no network**. `Command::new` sites confirm the boundary: `dar` (`src/dar/create.rs:35,114,134`, `src/dar/restore.rs:13,39`, `src/dar/version.rs:18`, `src/dar/catalog_xml.rs:25`, `src/main.rs:356`), `sg_logs` (`src/tape/health.rs:135`), `mtx`/`diff` (`tests/mhvtl_e2e.rs:57,190`) — all reachable only from gated tests or runtime commands.

### Gating and locking

- `tests/performance.rs`: runtime env gate `TAPECTL_PERF_TESTS=1`, not `#[ignore]` → reports `ok` when skipped (T10). Harness roots at hardcoded `/scratch/tapectl-perf` (`performance.rs:52`).
- `tests/mhvtl_e2e.rs`: double-gated (`#[ignore]` + `TAPECTL_MHVTL=1` + `/dev/nst0` existence, `mhvtl_e2e.rs:30-32`). Tape-device lock is a process-local `static Mutex` (`mhvtl_e2e.rs:37-42`) — serializes tests *within* the one e2e binary; nothing guards against a second process (T20).

### Toolchain / lint / audit state

- `rustc 1.94.1` / `cargo 1.94.1`. No `rust-toolchain.toml`; no `rust-version` in `Cargo.toml`; `Cargo.lock` **is** committed (plus three under `validation/`).
- `cargo fmt --check`: **clean** (exit 0).
- `cargo clippy --all-targets`: **12 unique warning sites** (10 lib: `clippy::type_complexity` at `src/cli/archive_set.rs:294`, `src/cli/report.rs:348,405,468,644`, `src/cli/stage.rs:122`, `src/volume/write.rs:974`, module-level at `src/volume/layout.rs:2`, `src/volume/restore.rs:17,179`; 2 test: `cloned_ref_to_slice_refs` at `tests/failure_modes.rs:37,50`). A `-D warnings` gate fails today.
- `cargo audit` (284 dependencies): **2 vulnerabilities** — quick-xml 0.39.2, [RUSTSEC-2026-0194](https://rustsec.org/advisories/RUSTSEC-2026-0194) and [RUSTSEC-2026-0195](https://rustsec.org/advisories/RUSTSEC-2026-0195) (both 7.5 HIGH, 2026-06-29, fix ≥0.41.0) — plus 6 warnings (unmaintained `number_prefix`, `proc-macro-error`, `proc-macro-error2`; unsound notes on `anyhow` 1.0.102, `rand` 0.8.5/0.9.2).

### External tool matrix (dev VM vs ubuntu-latest)

| Tool | Needed by | Dev VM | GitHub `ubuntu-latest` (= Ubuntu 24.04 since 2025-01-17, [github.blog changelog](https://github.blog/changelog/2024-09-25-actions-new-images-and-ubuntu-latest-changes/)) |
|---|---|---|---|
| (none) | **ungated `cargo test`** | — | — nothing to install |
| dar ≥2.6 (`src/dar/version.rs:6`), rec 2.7.20+ | gated perf + mhvtl suites, runtime | 2.7.13 at `/usr/bin/dar` (`/opt/dar` default path absent) | **dar 2.7.13-5.1build4** in noble/universe ([packages.ubuntu.com/noble/dar](https://packages.ubuntu.com/noble/dar), fetched 2026-07-20) — meets the 2.6 minimum, below the 2.7.20 recommendation |
| mhvtl (userspace + `mhvtl.ko`) | mhvtl e2e suite | userspace + media present; **kernel module absent for all 3 installed kernels; `/dev/nst0` missing** | **not packaged at all** ([packages.ubuntu.com search "mhvtl", noble: "no results"](https://packages.ubuntu.com/search?keywords=mhvtl&searchon=names&suite=noble&section=all), fetched 2026-07-20) — source build of an out-of-tree kernel module required |
| sg3-utils (`sg_logs`), mtx, mt | mhvtl suite / runtime | present | packaged, trivially installable |
| age CLI | nothing in tests (crate used); RESTORE.sh at recovery time | present | packaged |

### Repo/GitHub state (read-only)

`mikmorg/tapectl` is **PUBLIC** (`gh repo view`: `"visibility":"PUBLIC"`); branch protection on `master`: **none** (`gh api .../branches/master/protection` → 404 "Branch not protected"); GitHub-detected license: **null**; 34 commits (2026-04-10 → 2026-07-18); 0 tags; no `.github/`, no CI workflows, no templates. Sibling precedents: `mikmorg/homorg` is **PRIVATE** and runs all CI on `runs-on: [self-hosted, vm-desk1]` (`/home/mikmorg/git/homorg/.github/workflows/ci.yml:24`); `mikmorg/lcsas` is **PUBLIC** and runs on `ubuntu-latest` with its kernel-module-dependent suite explicitly excluded (`/home/mikmorg/git/lcsas/.github/workflows/test.yml:68-73`: "cdemu-daemon / cdemu-client packages require a loadable kernel module (vhba) that is not available on GitHub-hosted runners").

---

## Findings — tests and suite truth

#### [HIGH] T1 — RESTORE.sh is never executed by any test, script, or checklist automation
- Where: `src/volume/layout.rs:846-864` (the only tests: `restore_script_is_bash_and_mentions_label`, `restore_script_has_all_modes`, all `contains()`); `tests/mhvtl_e2e.rs` (reads raw tape files but has no RESTORE.sh leg); `scripts/` (does not exist — issue #7's `mhvtl-verify-gate.sh` is spec-only)
- Design ref: §8.3, §10 M7 validation ("Manual RESTORE.sh recovery works")
- What: the single most important artifact tapectl produces — the heir's no-database entry point — is asserted to *contain the substrings* `--restore)`, `age -d -i`, `truncate -s`, and never run. Not even `bash -n` syntax-checks it. This is exactly how prior-audit H1 (mini-index omits envelope entries → every `--find-envelope`/`--restore` fails end-to-end) and H2 (tenant RECOVERY.md commands don't work) shipped green. Issue #7's own comment states this and the gate spec (fixture, four legs, EXPECTED_FAIL manifest) is written but unbuilt.
- Remediation: build issue #7's `scripts/mhvtl-verify-gate.sh` with its heir leg (`dd` file 2 → `--info`/`--find-envelope`/`--restore` → `diff -r`) as the acceptance harness, EXPECTED_FAIL list seeded with H1/H2. Until then, add the two zero-infrastructure checks that need no tape: `bash -n` on `generate_restore_script` output in a unit test, and a shellcheck pass in CI.

#### [HIGH] T2 — The write→restore pipeline has zero runnable automated coverage today
- Where: modules with no in-module tests: `src/volume/write.rs` (1,128 lines), `src/volume/restore.rs` (282), `src/staging/mod.rs` (573), `src/staging/clean.rs`, `src/dar/create.rs`, `src/dar/restore.rs`, `src/tape/ioctl.rs`, `src/config.rs` (403), `src/tenant/mod.rs`, `src/signal.rs`, all 18 `src/cli/*` files (full list in the coverage map below). Sole coverage: 6 `#[ignore]`d tests in `tests/mhvtl_e2e.rs`. Local mhvtl state: userspace at `/usr/bin/vtltape`, config at `/etc/mhvtl/`, media at `/opt/mhvtl/`, but `find /lib/modules -name "mhvtl*"` is empty across kernels 6.8.0-117/-134/-136 and `/dev/nst0` does not exist.
- Design ref: §10 M7 ("Integration tests against mhvtl (automated test suite)" — checked)
- What: even `stage_create` — the dar+sha256+age pipeline — is exercised *only* by gated suites (`tests/performance.rs`, `tests/mhvtl_e2e.rs`); the ungated suite never invokes dar at all (proven by the `env -i` runs). Since whichever kernel update removed `mhvtl.ko`, the entire physical pipeline has been unverifiable on every machine that exists. The last recorded green gated run is the 2026-04-12 baseline in `docs/perf-baselines.md`.
- Remediation: (1) restore mhvtl locally and make it survive kernel updates — build the module via DKMS (the vhba/cdemu modules on this same VM are DKMS-managed and survived to 6.8.0-136) and file a local-infra ticket; (2) record in the mhvtl-gate script a hard *precondition check with a loud failure* ("module missing for running kernel — rebuild") so bit-rot is detected the day it happens, not months later; (3) add ungated dar-based pipeline tests (see missing tests #5/#6) so hosted CI covers stage→slice→encrypt without tape.

#### [HIGH] T3 — The integration suite tests the schema, not the application
- Where: `tests/integration.rs` — 30 of 33 tests use hand-written SQL for both fixture and behavior; only `test_audit_unit_rename_logs_field_change`, `test_audit_unit_path_change_logs_field_change`, `test_audit_crash_recovery_logs_system_event` (integration.rs:1289-1401) call `tapectl::` code
- Design ref: process (§10 M7 "Comprehensive unit tests for every module")
- What: three concrete masking cases. `test_encryption_key_rotation` (integration.rs:695-751) performs rotation *in SQL* and asserts old keys survive — while the real `key rotate` (prior H13) strands tenants keyless on second invocation; this test green-lights the exact broken feature. `test_archive_set_policy_inheritance` (integration.rs:756-802) INSERTs `units.archive_set_id` directly — no production writer exists (prior MED), so the test proves a state the program cannot reach. `test_compaction_candidate_query` (integration.rs:513-535) embeds its own copy of the utilization query rather than calling `report.rs`, so production drift is invisible. The suite as a whole would pass with `src/cli/` deleted.
- Remediation: don't delete these (schema/constraint pinning has value — the CHECK/FK/UNIQUE rejection tests are legitimately useful); reclassify them as schema tests and add real behavior tests that call the library entry points the CLI uses (`tenant::add_tenant`, key rotate's implementation, `cli::operations::*`), the way `failure_modes.rs` and `mhvtl_e2e.rs` already do. Priority order per the missing-tests list below.

#### [MED] T6 — Design §10 M7 checks off failure-mode tests that do not exist
- Where: `tapectl-design-v4_0.md:2025-2026` ("[x] Failure mode tests: interrupted writes, corrupted staging, missing keys, crashed DB recovery, raw-volume restore, ENOSPC recovery") vs `tests/failure_modes.rs` (5 tests: crypto misuse ×4, crash-recovery sweep ×1) and `tests/mhvtl_e2e.rs` (missing-key restore)
- Design ref: §10 M7
- What: of the six listed modes, only "missing keys" (mhvtl-gated) and "crashed DB recovery" have tests. Interrupted-write, ENOSPC, and raw-volume-restore tests don't exist and *can't* — the features are unimplemented (prior H3/H4, missing `restore raw-volume`). "Corrupted staging" is covered only at the crypto layer (tampered ciphertext), not at the staging pipeline layer. The checkbox overclaims exactly where the risk is highest.
- Remediation: un-check the three untestable items in the design doc (tie them to prior-audit H3/H4 tickets), or reword to what is actually covered. When the features land, the issue #7 EXPECTED_FAIL mechanism is the right home for their tests in the interim.

#### [MED] T7 — Zero CLI-layer tests: parsing, exit codes, and `--json` schemas are all unpinned
- Where: `src/cli/` (18 modules, ~5,400 lines, zero `#[cfg(test)]`), `src/main.rs` (363 lines, zero tests); no `Cli::command().debug_assert()` anywhere; no `assert_cmd`/process-level test; audit's 0/1/2 contract (`src/cli/audit.rs:181-235`, `src/main.rs:94-97`) asserted by nothing
- Design ref: §5
- What: a clap attribute typo, an accidental subcommand rename, a changed `--json` key, or a broken exit-code path would ship silently. The man pages are generated from this same `Cli` (so drift checking helps), but nothing executes a parse. The README's "All commands support `--json`" rests on manual discipline (it is, in fact, threaded through all 16 `cli::*::run` signatures — verified — but no test pins any output schema).
- Remediation: three cheap layers: (1) one-line `#[test] fn cli_asserts() { Cli::command().debug_assert(); }`; (2) a handful of `Cli::try_parse_from` cases for the flag surface; (3) process-level smoke via the built binary in a temp home: `init` → `audit --json` (assert exit code + JSON keys), `config check --json`. Layer 3 doubles as the only test of `main.rs` wiring.

#### [MED] T8 — Migrations are untested as migrations, and fixtures have drifted from the runner
- Where: `tests/integration.rs:48-58` (hand-applies 001+002 via `execute_batch`, bypassing `rusqlite_migration`); `src/staging/validate.rs:81`, `src/policy/mod.rs:158`, `src/db/queries.rs:508` (three fixtures apply **001 only** — no FTS5 objects, diverging from production schema); `src/db/mod.rs:24-30` (`open_memory` is `cfg(test)`-only, unusable from integration tests, which is why integration.rs hand-rolls)
- Design ref: §4 Versioning
- What: no test opens a 001-level database and proves 002 applies (the upgraded-DB path every real user hits); a future 003 must be manually added to four separate fixture sites or tests silently run against a stale schema. `db::open` does get exercised as fresh-DB (failure_modes, 1 integration test) — that path is fine.
- Remediation: expose a public test-support constructor (e.g. `db::open` with `TempDir`, as `failure_modes.rs` already does — cheapest is to just use that everywhere) and add one upgrade test: apply `001_initial.sql` manually, call `db::open`, assert `files_fts` exists and triggers fire.

#### [MED] T10 — Gated suites report `ok` when skipped
- Where: `tests/performance.rs:114-116,170-172,255-257` (runtime env check, no `#[ignore]`) — 3 of the headline 106 passes are vacuous; `tests/mhvtl_e2e.rs:204-207` has the same runtime-skip inside `#[ignore]` (visible as "ignored" in default runs, but a `--ignored` run without the env var also prints ok)
- Design ref: process; house precedent `lcsas/.github/workflows/test.yml:108-119` (GATE-10: "green-by-skip is the bug being fixed")
- What: a reader of `cargo test` output — or a CI job summary — sees "106 passed" and cannot tell 3 asserted nothing. As gates accumulate (issue #7), this pattern is how they rot.
- Remediation: add `#[ignore = "TAPECTL_PERF_TESTS gated"]` to the perf tests so skips are visible as `ignored`; in any CI/gate context adopt the lcsas pattern (grep the summary for expected pass counts, or a skip-rot floor).

#### [MED] T14 — Generated-recovery-artifact tests assert substrings, not validity
- Where: `src/volume/layout.rs:786-799` (`planning_header_embeds_unit_rows` builds a **2-unit** fixture and does six `contains()` checks) vs `src/volume/layout.rs:558-574` (`[[units]]` emitted once before the loop → duplicate-key invalid TOML for ≥2 units, prior-audit MED, still live); same class: the two RESTORE.sh tests (T1), `system_guide_contains_label_and_total` (layout.rs:832-836)
- Design ref: §8.4
- What: this is the prior audit's "test too weak to catch invalid TOML" finding, found live and load-bearing: the weak test isn't just insufficient, it is currently *masking* a known bug — changing `contains()` to `body.parse::<toml::Value>()` makes it red today. By contrast `id_thunk_parses_as_toml_body`, `mini_index_parses_as_toml_body`, and `manifest_toml_round_trips_slices` (layout.rs:709-829) show the right pattern already exists in the same file.
- Remediation: convert the planning-header test to a parse assert (it becomes the failing regression test for the prior-audit fix); add `bash -n` to the RESTORE.sh tests; as a rule, every generated on-tape artifact test must parse/execute, never grep. Test-data realism gaps of the same class: no fixture anywhere contains a symlink, a >9-slice archive, unicode filenames, or a multi-unit volume with >1 non-operator tenant in the ungated suite.

---

## Findings — CI feasibility and design

#### [MED] T4 — The intended lint gate fails on current stable; nothing pins the toolchain
- Where: 12 clippy warning sites (list in Measured baseline); no `rust-toolchain.toml`; no `rust-version` in `Cargo.toml`; `CLAUDE.md:33` ("zero clippy warnings") — true when written, false under 1.94.1's lints
- Design ref: process (CLAUDE.md "must stay warning-clean")
- What: this is the concrete answer to "what must change for green in Actions": `cargo test` passes with no installs; `fmt --check` passes; `clippy -D warnings` does not. Without a pinned toolchain, this recurs on every clippy release (homorg pins `1.94.1` for exactly this reason, `ci.yml:29-34`).
- Remediation: fix the 12 sites (10 are `type_complexity` — type aliases; 2 are one-line `&[x.clone()]` changes), add `rust-toolchain.toml` pinned to 1.94.1, mirror the pin in CI.

#### [MED] T5 — `cargo audit` fails today; the vulnerable dependency serves only dead code
- Where: `Cargo.toml:29` (`quick-xml = "0.39"`); sole consumer `src/dar/catalog_xml.rs` (module-wide `#[allow(dead_code)]`, never called — prior-audit dead seam)
- Design ref: process
- What: RUSTSEC-2026-0194/-0195 (both 7.5 HIGH, fixed in quick-xml ≥0.41.0). An audit CI job (homorg has one, `ci.yml:235-248`) would be red from day one. The cleanest fix is also a prior-audit remediation: delete the dead module and the dependency together.
- Remediation: remove `catalog_xml.rs` + quick-xml (preferred), or upgrade to 0.41 and carry the dead code; add the audit job either way. The 6 unmaintained/unsound warnings are non-blocking by default and fine to leave visible.

### CI feasibility verdict for the gated suites

**(a) Hosted runner, ungated only — works today, near-zero cost.** Empirically the ungated suite needs *nothing* installed (see Measured baseline). `apt install dar` (noble: 2.7.13-5.1build4, satisfies the ≥2.6 minimum enforced at `src/dar/version.rs:6`, below the 2.7.20 recommendation — acceptable for CI, note it in the job) additionally enables future ungated stage-pipeline tests, which is where the highest-value missing coverage lands (missing tests #5/#6).

**(b) Self-hosted runner on this VM enabling `TAPECTL_MHVTL=1` — not with the repo public.** Facts: mhvtl requires an out-of-tree kernel module, is not packaged in Ubuntu 24.04 (verified), and GitHub-hosted runners are ruled out by the same reasoning lcsas documents for vhba/cdemu (`test.yml:68-73`). The homorg precedent (`runs-on: [self-hosted, vm-desk1]`) exists on this same infrastructure — but homorg is **PRIVATE** and tapectl is **PUBLIC**, and GitHub's hardening guidance is unambiguous: self-hosted runners "can be persistently compromised by untrusted code in a workflow", and on a public repo "anyone who can fork the repository and open a pull request… [is] able to compromise the self-hosted runner environment" ([docs.github.com security-hardening guide](https://docs.github.com/en/actions/security-for-github-actions/security-guides/security-hardening-for-github-actions), fetched 2026-07-20). vm-desk1 is the primary dev VM with passwordless sudo — attaching it to a public repo is not an acceptable risk. Additionally, the runner would be pointless *right now*: mhvtl is broken on this VM (T2).
**(c) Hybrid — recommended.** Hosted CI for everything ungated + drift + audit; the mhvtl gate stays a local, manually-run script (issue #7's `scripts/mhvtl-verify-gate.sh`) required by convention for restore-path diffs. Upgrade paths if e2e-in-CI is later wanted: make the repo private (then the homorg pattern applies directly), or a dedicated throwaway runner VM on home2 with workflows triggered only on `push` to `master`/`workflow_dispatch` (never `pull_request`), which removes the fork attack surface at the cost of post-merge-only signal.

**Perf suite: does not belong in CI.** Wall-clock assertion ceilings (`performance.rs:159-163,244-248`) are noise on shared runners; the harness hardcodes `/scratch/tapectl-perf` (absent on hosted runners); it requires `--release` (the one sanctioned release-build case, but pointless on non-baseline hardware since `docs/perf-baselines.md` numbers are VM-specific). Keep it manual per the baselines doc.

### Recommended workflow (concrete)

`.github/workflows/ci.yml`, modeled on homorg with lcsas's honesty patterns:

- **Triggers/concurrency:** `push: [master]`, `pull_request: [master]`; `concurrency: ci-${{ github.ref }}` + `cancel-in-progress` (homorg `ci.yml:9-11`).
- **Job `check`** (`ubuntu-latest`, `timeout-minutes: 20`): `dtolnay/rust-toolchain` pinned `1.94.1` + `rustfmt,clippy`; `Swatinem/rust-cache` (or homorg's `actions/cache` keyed on `Cargo.lock`); `cargo fmt --all -- --check` → `cargo clippy --all-targets --locked -- -D warnings` → `cargo test --locked` (debug — never `--release`, per house rule). Gate on T4 fixes landing first. Optionally `sudo apt-get install -y dar && dar --version` once ungated dar tests exist.
- **Job `man-drift`** (`timeout-minutes: 10`): `cargo run --example gen_man && git diff --exit-code docs/man` — the homorg openapi-parity pattern (`ci.yml:144-156`). Verified to pass today (zero drift).
- **Job `audit`** (`timeout-minutes: 10`): `cargo install cargo-audit --locked && cargo audit` (homorg `ci.yml:235-248`). Land after T5, or start `continue-on-error: true` with a linked issue.
- **Not in CI:** mhvtl e2e (verdict above — wire `scripts/mhvtl-verify-gate.sh` locally per issue #7 and record the verdict there), perf suite (manual).
- **Repo prep:** add `rust-toolchain.toml`; add `rust-version` to `Cargo.toml` (verify the README's 1.75 claim or correct it — T18); `.gitignore` (T15); consider branch protection requiring the `check` job (T17).

---

## Coverage-vs-risk map

| Risk-ranked behavior | Coverage today | Runnable where | Gap |
|---|---|---|---|
| **RESTORE.sh heir restore** | none executed; substring asserts only | nowhere | T1 — prior H1/H2 undetectable |
| **tapectl restore unit/file** | `mhvtl_full_round_trip`, `mhvtl_tenant_isolation`, `mhvtl_both_tenants_self_restore` | gated; **currently unrunnable** (T2) | zero ungated coverage; `volume/restore.rs`, `cli/restore.rs` no unit tests |
| **Tenant/export RECOVERY.md recipes** | none (not parsed, not executed) | nowhere | prior H2 + broken `sha256sum -c` recipe invisible |
| **volume write layout/positions** | mhvtl round-trip + layout unit tests (thunk/mini-index/manifest parse; header/script substring-only) | gated + weak ungated | interrupt/ENOSPC/wrong-tape untested (features absent, prior H3–H5); T14 |
| **verify → evidence loop (ADR-0001)** | `mhvtl_full_round_trip` asserts `failed == 0` | gated | `verification_sessions` row never asserted post-verify; exit code 0 on failures (T9) |
| **stage pipeline (dar→sha→age)** | perf suite + mhvtl only; `encrypt_data` unit-level via isolation/failure tests | **gated only** | ungated CI never runs dar; stage-failure path (stuck `staging` rows, prior MED) untested |
| **slice numbering ≥10 slices** | none | — | prior H8 undetectable; largest fixture is 5 files |
| **symlinks/special files in units** | none (no fixture contains a symlink) | — | prior H7 undetectable |
| **key rotate** | raw-SQL simulation (`integration.rs:695`) | ungated | masks prior H13 (T3) |
| **migrations fresh/upgrade** | fresh-DB via `db::open` (2 tests) | ungated | no upgrade test; fixture drift (T8) |
| **db fsck / backup / import** | none | — | `--repair` deletes tape-history rows untested; fsck FAIL exits 0 (T9) |
| **policy resolver** | 5 real unit tests incl. dotfile-wins, NULL-inherit, dangling-set fallback | ungated | good — but production `stage create` bypasses the resolver (prior MED), so green tests ≠ used code |
| **CLI parse / exit codes / --json** | none | — | T7 |
| **signal handling** | none (`signal.rs` untested; `Interrupted` never constructed) | — | prior H3 unwatchable |
| **sg_logs health parse** | 8 tests on real captured fixtures (`tests/fixtures/sg_logs/`, provenance-dated) | ungated | collection/spawn path gated; fine |
| **catalog FTS search** | 2 SQL-level tests incl. tokenization regression | ungated | CLI layer untested; acceptable |

Happy-path sampling: ~20 of the 100 meaningful ungated tests assert failure behavior, and ~13 of those are SQL constraint rejections; **zero** exercise a pipeline failure (interrupt, ENOSPC, stage failure, wrong tape) — consistent with the prior audit's Theme 2.

### Highest-value missing tests (build in this order)

1. **`heir_restore_sh_round_trip`** (mhvtl-gated; = issue #7 legs 2–3): write volume → `dd` file 2 off tape → `--info`, `--find-envelope`, `--restore` with tenant key → `diff -r`; negative: wrong key must fail. Would have caught H1+H2 on day one. Ship with the EXPECTED_FAIL manifest so it can land red.
2. **`planning_header_multi_unit_parses`** (ungated, one-line change): `toml::Value` parse in the existing 2-unit test — converts a masked live bug into a red test (T14).
3. **`restore_script_is_valid_bash`** (ungated): `bash -n` on `generate_restore_script` output; plus shellcheck in CI. Floor under T1 until the gate exists.
4. **`key_rotate_twice_keeps_tenant_decryptable`** (ungated, real code path): call the actual rotate implementation twice; assert ≥1 active key and staging refuses when zero. Red today against H13; replaces the SQL simulation.
5. **`stage_create_ge_10_slices_numbering`** (ungated + dar in CI): tiny slice_size forcing ≥11 slices; assert `stage_slices.slice_number` matches dar's numeric indices. Red today against H8.
6. **`stage_create_symlink_unit`** (ungated + dar): unit with symlink + broken symlink; assert stage either succeeds or errors actionably, never hangs. Red today against H7.
7. **`cli_debug_assert_and_json_smoke`** (ungated): `Cli::command().debug_assert()` + process-level `init`/`audit --json`/`config check --json` asserting exit codes and JSON keys (T7, T9's test side).
8. **`migration_upgrade_001_to_latest`** (ungated): apply 001 manually, `db::open`, assert FTS objects + triggers (T8).
9. **`export_selects_single_stage_set`** (ungated): two staged sets for one unit → export → manifest references exactly one set. Red today against H11.
10. **`verify_failure_exit_code`** (mhvtl-gated, after T9 fix): corrupt one slice on tape → `volume verify` exits non-zero and records a failed `verification_sessions` row (ADR-0001's evidence leg).

---

## Findings — operability

#### [MED] T9 — The two monitorable commands can't be monitored: verify and fsck failures exit 0
- Where: `src/cli/volume.rs:149-166` (Verify prints `N failed`, returns `Ok` → exit 0); `src/main.rs:181-198` (fsck prints `integrity=FAIL`, returns `Ok` → exit 0); contrast `src/main.rs:94-97` (audit alone escalates); `src/error.rs:7-10` (`EXIT_WARNING` declared, `#[allow(dead_code)]`, used by nothing)
- Design ref: §5 ("Exit codes: 0 = success, 1 = warnings, 2 = errors/violations" — global, not audit-only); ADR-0001 (verification is the evidence loop)
- What: a cron'd verify cadence — the exact automation the verify-interval policy implies — cannot detect slice failures except by parsing stdout. Same for a scheduled fsck. `--json` output does carry `failed`/`integrity_ok` fields (machine-readable, good), but exit-code is the contract the design promises scripts.
- Remediation: return exit 1 (or 2) from verify when `failed > 0` and from fsck when `!integrity_ok`, using the existing `EXIT_WARNING` constant; add the process-level test (missing test #10).

#### [MED] T11 — All runtime logging is silently dropped: no tracing subscriber is ever installed
- Where: `src/main.rs` (no `tracing_subscriber` init anywhere — verified by grep across `main.rs`/`lib.rs`); emit sites in 10 files including the crash-recovery sweeps (`src/db/mod.rs:61-63,85-87`: `warn!("recovered orphaned write sessions — marked as aborted")`) and per-file write progress (`src/volume/write.rs:197`)
- Design ref: §5 (global `--verbose`); process
- What: `tracing` + `tracing-subscriber` (with the `json` feature) are compiled in and functionally dead. An operator whose interrupted write was swept to `aborted` at next startup is never told. This is the complementary half of the prior audit's "--verbose parsed but ignored" MED: honoring `--verbose` concretely means `tracing_subscriber::fmt().with_max_level(if cli.verbose { DEBUG } else { WARN }).init()` at the top of `main`, at which point ~30 existing emit sites start paying rent, with WARN-level (crash recovery, dotfile failures) visible by default.
- Remediation: as above — one initializer plus a decision on default level; or remove the two dependencies and the emit calls.

#### [MED] T12 — The operator guide's mhvtl instructions are wrong for Ubuntu, and module maintenance is undocumented
- Where: `docs/operator-guide.md:13` (`sudo apt install mhvtl lsscsi mt-st sg3-utils`); packages.ubuntu.com: no mhvtl package exists in noble (fetched 2026-07-20)
- Design ref: process
- What: mhvtl must be built from source, including an out-of-tree kernel module rebuilt for every kernel — and this VM is the live demonstration of what happens when that isn't documented: userspace and tape media intact, module gone for all three installed kernels, `/dev/nst0` absent, gated suite dead (T2). Nothing in any doc mentions rebuild-after-kernel-update.
- Remediation: correct the install section (source build or the project's packages), add a "after a kernel update" note, and prefer DKMS registration so it stops being a manual step.

#### [MED] T13 — No operator cadence runbook: drills, verify schedule, rotation
- Where: `docs/operator-guide.md` (command reference is good; contains zero scheduling/drill content); `docs/lto6-validation-checklist.md:5-6` (self-declared "**stub** … flesh out each step when hardware arrives" — though its Raw-recovery drill section is already a solid skeleton)
- Design ref: §2.18 (verify intervals), §2.20 (audit cadence), ADR-0004 (evidence age)
- What: the system's whole truth model (claims → evidence, evidence ages) presumes an operator *routine*: run `audit` on a schedule, verify volumes within interval, periodically drill a restore (including the heir path with only an envelope), rotate tapes between locations. None of that exists as a runbook; a future operator (or heir) gets commands but no doctrine. Monitoring hooks exist half-way: `report health --json` and `audit --json` are machine-readable (verified), but T9 breaks exit-code automation.
- Remediation: add a "Cadence" section to the operator guide (monthly `audit` + `report verify-status`, verify volumes crossing their interval, an annual restore drill that runs RESTORE.sh from a real tape, movement checklist), and promote the checklist's raw-recovery drill into the recurring runbook rather than a one-time hardware gate.

#### [LOW] T19 — `--json` inconsistencies in the main-inline commands
- Where: `src/main.rs:221-230` (`db import` prints plain text regardless of `--json`), `src/main.rs:200-219` (`db export` prints JSON regardless of the flag; is also a row-count stub — prior-audit MED, cross-ref)
- Design ref: §5
- What: README's "All commands support `--json`" is otherwise accurate (all 16 `cli::*::run` signatures thread it; verified), with known depth issues cross-referenced from the prior audit (retire silent in JSON; location list drops description).
- Remediation: honor the flag in both; fold into the prior-audit `db export` fix.

#### [LOW] T21 — `config check` validates syntax only
- Where: `src/main.rs:265-287` (TOML parse + serde load; nothing else)
- Design ref: §7
- What: a config pointing dar at a nonexistent binary (the shipped default `/opt/dar/bin/dar` doesn't exist on this VM), a missing staging directory, or an absent tape device all pass `config check`. The dar existence check exists but only runs at `init` (`src/main.rs:326-327`).
- Remediation: extend check to verify dar binary (reusing `check_dar` + version gate), staging dir existence/writability, and device-node presence per configured backend, with warnings not errors.

Error-message quality (sampled): generally good — `thiserror` messages carry the failing name/path (`src/error.rs:14-93`), the uninitialized case tells you the fix (`main.rs:47`), dar errors wrap stderr. Weak spots are pass-throughs: raw rusqlite constraint errors surface verbatim (e.g. the retry-after-interrupt UNIQUE failure, prior H3 note) and `db import` failures are bare. Man pages: 22 pages covering the top level + all 21 subcommands, and the drift check passed (see Verified clean). README spot-check: accurate on test commands, layout table, and command reference; inaccurate on "Rust 1.75+" (T18) and "See LICENSE file" (T16).

---

## Findings — repo/process hygiene

#### [LOW] T15 — No `.gitignore`
- Where: repo root (file absent; verified)
- What: builds only stay out of git because this machine exports `CARGO_TARGET_DIR=/scratch/cargo-target`; any other checkout gets an untracked `target/`, and nothing guards against stray `*.db`/staging residue landing in a commit. (Current test suite writes only to tempdirs — verified — so today's risk is future-shaped.)
- Remediation: standard Rust ignore (`/target`) plus `*.db`, `*.db-wal`, `*.db-shm`.

#### [LOW] T16 — LICENSE file missing while three things point at it
- Where: `README.md:199-201` ("See LICENSE file"), `Cargo.toml:6` (`license = "MIT"`), GitHub license detection = null (`gh api`, 2026-07-20)
- What: on a public repo this is a real ambiguity for anyone reading it; a `cargo publish` would also fail the license-file expectation.
- Remediation: commit the MIT text.

#### [LOW] T17 — No release/process scaffolding
- Where: GitHub state (branch protection 404; 0 tags; no CHANGELOG; no `.github/` templates); `Cargo.toml:3` (0.1.0 across all 34 commits, 2026-04-10 → 2026-07-18)
- What: with CI landing, branch protection requiring the check job is nearly free; tags/CHANGELOG matter once the LTO-6 validation makes a "known-good archival binary" worth naming — the version an heir's tape was written by is a restore-relevant fact (it's embedded in the ID thunk).
- Remediation: protect `master` on the CI job; tag a `v0.1.0` at the LTO-6-validated commit when it exists; CHANGELOG optional at solo scale.

#### [LOW] T18 — MSRV is claimed, not managed
- Where: `README.md:20` ("Rust 1.75+"), `Cargo.toml` (no `rust-version`)
- What: the claim predates current deps (rusqlite 0.39, clap 4.6) and is untested by anything; the only proven-working toolchain is 1.94.1.
- Remediation: add `rust-version` and either verify 1.75 or restate; the pinned-toolchain CI (T4) makes deliberate MSRV mostly moot for a solo tool.

#### [LOW] T20 — Determinism footnotes (nothing red today)
- Where: `tests/mhvtl_e2e.rs:37-42` (tape lock is a process-local `Mutex` — correct for the single e2e binary, no protection across processes/binaries); `tests/performance.rs:52` + `tests/mhvtl_e2e.rs:77` (harness roots hardcoded under machine-specific `/scratch`); `tests/integration.rs:27` (fixture config points staging at shared `/tmp/tapectl-test-staging` — never created or written today, a footgun if fixtures grow real staging)
- What: the ungated suite is otherwise exemplary: per-test `TempDir`s, no `~/.tapectl`, no ordering coupling, green under default parallelism and under `env -i`.
- Remediation: comment the lock's process-locality (or flock the device in the gate script, which issue #7 already specs); derive gated-harness roots from an env var with `/scratch` default; point the fixture staging path into the tempdir.

---

## Verified clean

- **`cargo test` (ungated): 106/106 green**, parallel, deterministic; all four ungated test binaries green again under `env -i` with empty `PATH` and no `HOME` — hermetic against external binaries, user home, devices, and network.
- **`cargo fmt --check`: clean.**
- **Man-page drift: zero.** `cargo run --example gen_man` regenerated all 22 pages; `git status docs/man` and `git diff --stat docs/man` showed no changes; tree restored and re-verified clean (only the pre-existing `CLAUDE.md` modification remains, untouched). `gen_man` is idempotent as its header claims (`examples/gen_man.rs:8-10`).
- **CLAUDE.md test-count claim matches measured reality** (106 = 60+33+5+5+3, with the 3-vacuous caveat in T10); README's Testing section commands are accurate as written.
- **`Cargo.lock` committed** — CI can and should build `--locked`.
- **Crypto-boundary tests are real**: `tests/tenant_isolation.rs` and `tests/failure_modes.rs` call the actual `encrypt_data`/key code with genuine negative cases (wrong key, tamper, malformed recipient), and the age-header-magic pin (`tenant_isolation.rs:104-115`) deliberately protects the mhvtl leak-scan's honesty.
- **Policy resolver unit tests genuinely cover the 3-level precedence** including dotfile-wins, NULL-field inheritance, and dangling `archive_set_id` fallback (`src/policy/mod.rs:180-279`).
- **Health parsing tests use real captured sg_logs fixtures** with dated provenance (`src/tape/health.rs:172-177`, `tests/fixtures/sg_logs/`).
- **The mhvtl e2e suite itself is well-built** (real library calls, tape lock, plaintext-leak scan over raw tape bytes, missing-key negative restore) — its problem is runnability (T2) and the missing RESTORE.sh leg (T1), not quality.
- **No `#[allow]` attributes in `tests/` or `examples/`** hiding drift (the 14 in `src/` are the prior audit's documented dead seams).
- **`audit` implements the §5 exit-code contract** (0/1/2 with `--action-plan`/`--json`) — untested (T7) but correct by inspection.
- **`--json` is threaded through every `cli::*::run`** signature (16/16 verified), and `report health --json` / `audit --json` are usable monitoring hooks today.

---

*External facts cited: dar 2.7.13-5.1build4 in Ubuntu 24.04 noble/universe — packages.ubuntu.com/noble/dar (fetched 2026-07-20). mhvtl absent from noble — packages.ubuntu.com package-name search, zero results (fetched 2026-07-20). `ubuntu-latest` = Ubuntu 24.04 since 2025-01-17 — github.blog/changelog/2024-09-25-actions-new-images-and-ubuntu-latest-changes (fetched 2026-07-20). quick-xml advisories — rustsec.org RUSTSEC-2026-0194/-0195 (published 2026-06-29, surfaced by local `cargo audit` run 2026-07-20). Self-hosted-runner/public-repo risk — docs.github.com Actions security-hardening guide (fetched 2026-07-20). House precedents: `homorg/.github/workflows/ci.yml` (private repo, self-hosted vm-desk1), `lcsas/.github/workflows/test.yml` (public repo, hosted runners, kernel-module suite excluded — the cdemu/vhba precedent, lines 68-73).*
