# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tapectl is a Rust CLI tool for managing long-term archival storage across LTO tape and exportable encrypted directories (Blu-ray, USB). It wraps `dar` for archive creation/extraction, uses the `rage` crate for age encryption, and SQLite for catalog/inventory/policy/audit.

**The on-tape format is Layout Version 2** (ADR-0007). For anything touching tape
bytes or the write path, the normative set is, in authority order:
`docs/design/volume-format-v2.md` (the byte format) → `docs/design/layout-session.md`
(session state machine) → `docs/design/v2-open-questions.md` (resolved decisions,
§§1–11) → `docs/design/v2-implementation-plan.md` (the T0–T11 build playbook).
`docs/adr/0001`–`0007` govern all of them.

`tapectl-design-v4_0.md` remains the reference for everything those do NOT cover,
read **together with `docs/design-errata.md`** (the complete list of
superseded/recast sections — note §2.6/§8.1–§8.8 are superseded wholesale by v2,
and §2.9/Appendix C end-of-tape salvage is rejected outright). `CONTEXT.md` is the
vocabulary.

## Current State

Milestones 0 through 5 are complete.

**Milestone 0:** Full round-trip validated — dar → age encrypt → mhvtl tape → read → decrypt → extract. All criteria passed. Validation programs in `validation/`.

**Milestone 1:** Working commands: `init`, `tenant` (add/list/info/delete), `key` (generate/list/export/import), `unit` (init/init-bulk/list/status/tag/rename/discover). Full SQLite schema deployed.

**Milestone 2:** Working commands: `snapshot create/list`, `stage create`, `staging status/clean`. Full pipeline: directory walk → manifest → sha256 validation → dar archive → age multi-recipient encryption → checksums → receipt. dar wrapper with version check, XML catalog parsing, catalog isolation.

**Milestone 3:** Working commands: `volume init/write/verify/identify`. Full 10-file volume layout written to tape via mhvtl: ID thunk, system guide, RESTORE.sh, planning header, encrypted data slices, mini-index, tenant envelopes, dual operator envelopes. Tape ioctl module with fixed block I/O. Verify reads back and validates sha256.

**Milestone 4:** Working commands: `restore unit/file` (read from tape → decrypt → dar extract), `catalog ls/search/locate/stats`. Full round-trip verified: write → restore → diff -r identical.

**Milestone 5:** Working commands: `location add/list/info/rename`, `volume move/retire/read-slices`, `cartridge register/list/info/mark-erased`, `unit mark-tape-only`, `snapshot diff/delete`, `stage list/info`, `export`, `db backup/fsck`. Volume retire shows impact analysis. mark-tape-only enforces min_copies/min_locations. read-slices reads encrypted slices from a volume into staging for writing to another tape via `volume write`.

**Milestone 6:** Working commands: `archive-set create/edit/list/info/sync`, `audit` (compliance check with exit codes 0/1/2, --action-plan, --json), `snapshot mark-reclaimable` (enforced preconditions, tape-only 2x multiplier), `volume compact-read/compact-write/compact-finish/compact`, `report summary/fire-risk/copies/tape-only/dirty/pending/verify-status/health/capacity/age/events/compaction-candidates`. Policy resolver: dotfile > archive_set > defaults.

**Post-M6 completions:** All unassigned CLI commands from design doc implemented: `key rotate`, `tenant reassign`, `snapshot purge`, `unit check-integrity`, `quick-archive`, `db export/import/stats`, `config show/check`. Zero compiler warnings, 17 tests (5 unit + 12 integration), zero clippy errors. No StubCommands remain.

**Milestone 7 (software-side complete):** Phases 1–9 landed. Lib target (Phase 1), module unit tests (Phase 2), sg_logs health collection (Phase 3), full audit trail wiring (Phase 4), mhvtl-gated E2E round-trip (Phase 5), library failure-mode tests (Phase 6), multi-tenant isolation tests (Phase 7 — crypto cross-decrypt rejection, plaintext-leak scan on raw tape bytes, both-tenants self-restore, tape-device lock for parallel mhvtl tests), performance harness (Phase 8 — `tests/performance.rs` gated on `TAPECTL_PERF_TESTS=1`, baselines in `docs/perf-baselines.md`), docs + man pages (Phase 9 — README Testing/Documentation sections, `examples/gen_man.rs` + `docs/man/*.1` via clap_mangen, `docs/lto6-validation-checklist.md` procedure stub).

**Post-M7 hardening (complete):** Design gap audit identified 3 active bugs + 6 unacknowledged gaps. All 8 items fixed: clone-slices restructured to staging-only read-slices (self-describing invariant preserved), restore trial-decrypts with all tenant+operator keys (key rotation no longer breaks restore), compact-finish refuses retirement if live slices lack copies elsewhere, volume_verify records verification_sessions (audit feedback loop closed), staging cleanup reports actual bytes freed, compact-read errors on checksum mismatch, critical DB operations wrapped in transactions, export writes MANIFEST.toml + RECOVERY.md. RESTORE.sh fleshed out from stub to full emergency recovery script (--info, --find-envelope, --restore modes with sha256 verification and block-padding trimming). 106 tests (60 unit + 46 integration/lib/isolation/failure-mode), zero clippy warnings.

**Renovation (2026-07):** a full renovation stage, charted as a wayfinder map at
[issue #1](https://github.com/mikmorg/tapectl/issues/1) with a phased backlog in
issues #20–#73. The three audits under `docs/audits/` found the happy path solid but
the heir/emergency and unhappy paths broken. The milestone claims above describe
happy-path completeness only — treat them accordingly.

**Format v2 regear (complete, 2026-07-28).** Holistic R&D
(`docs/research/2026-07-21-ontape-format-and-write-design.md`) established that a
plan-first, write-once medium should not imitate a streaming format. The whole
write path was rebuilt to Layout v2 and landed as playbook tasks T0–T10:

- **On tape:** a plaintext **front index** (File 3) carrying every file's
  position/type/size + ciphertext sha256; **envelopes before slices**; a trailing
  plaintext **seal marker** binding the front index. End-of-tape salvage is gone —
  a real EOT is a clean abort to an unsealed tape, and the pre-flight capacity gate
  is the sole capacity defense.
- **In code:** a typestate **write session** (`src/volume/session.rs`:
  build → validate → plan → execute → seal → confirm), a **Store trait** with a
  shared chain walk (`src/store.rs`), Layout build/materialize (`src/volume/build.rs`),
  format parsers (`src/volume/format.rs`), and a **Collection** layer
  (`src/collection/`, `collection sync|status|plan|run`) for folder-per-unit
  archiving (CONTEXT.md: "Collection" is the source-root concept; "library" is
  reserved for the tape library / changer).
- **Verified:** 270 ungated tests; `tests/format_v2.rs` is a keyless synthetic-heir
  acceptance suite (proves the byte layout from recorded bytes alone); mhvtl e2e 9/9
  on real tape including Rust-vs-bash chain-walk parity on both a good and a
  corrupted tape; `scripts/mhvtl-verify-gate.sh` GREEN with exactly the two ticketed
  EXPECTED_FAIL entries (H7 #33, H8 #34 — both phase-2).

**Next:** issues #22–#28 describe the *pre-v2* design and must be read against the
normative set above, not implemented literally. Phase 2 next (#33/#34 would empty
the gate manifest). Real LTO-6 hardware validation stays deferred by choice — an
LTO-6 drive is owned, but development is mhvtl-first; the open hardware questions
(block size 512K vs 1M, LBP acceptance, MAM over-report bounds) are recorded in
`docs/design/v2-open-questions.md` §5 and the procedure is in
`docs/lto6-validation-checklist.md`.

## Build Commands

Default to `check`/debug. Release builds (`--release`) are only for publishing or the
gated performance suite — don't produce release artifacts unless asked.

```bash
cargo check --all-targets     # fast compile verification (preferred while iterating)
cargo build                   # debug binary at target/debug/tapectl
cargo clippy --all-targets    # must stay warning-clean
cargo fmt --check
```

There is no CI; run `clippy`, `fmt --check`, and `cargo test` locally before committing.

The crate is a **dual lib + bin target**: `src/main.rs` is a thin wrapper and all logic
lives in the `tapectl` library crate (`src/lib.rs`). Integration tests import `tapectl::`
directly, so keep command logic in library modules, not `main.rs`.

Regenerate man pages after any CLI (clap) change:

```bash
cargo run --example gen_man   # writes docs/man/*.1
```

## Testing

Default `cargo test` runs unit + integration + tenant-isolation + failure-mode tests;
none need tape hardware or mhvtl.

```bash
cargo test                              # everything ungated
cargo test --lib                        # unit tests only (in-module)
cargo test --test integration           # one integration file
cargo test test_volume_write_positions  # a single test by name (substring match)
```

Two suites are gated (they skip at runtime unless the env var is set):

```bash
# mhvtl end-to-end round-trip + on-tape tenant isolation. Needs /dev/nst0 backed by
# mhvtl. Tests are also #[ignore], so pass --ignored.
TAPECTL_MHVTL=1 cargo test --test mhvtl_e2e -- --ignored --nocapture

# Performance scenarios (thousands of files, large archives); ~2 min. This is the one
# case a release build is expected.
TAPECTL_PERF_TESTS=1 cargo test --test performance --release -- \
    --ignored --nocapture --test-threads=1
```

## Architecture

**Three-phase pipeline:** `snapshot create` (fast metadata) → `stage create` (dar + sha256 + encrypt) → `volume write` (tape I/O)

**Key subsystems:**
- **CLI layer** (`src/cli/`): clap derive-based subcommands (tenant, unit, snapshot, stage, volume, catalog, restore, audit, etc.)
- **Database** (`src/db/`): SQLite with WAL mode, forward-only numbered migrations, full audit trail
- **Unit management** (`src/unit/`): archival entities tracked via `.tapectl-unit.toml` dotfiles in each directory
- **dar integration** (`src/dar/`): subprocess wrapper; minimum dar 2.6.x; XML catalog parsing via quick-xml
- **Staging** (`src/staging/`): sha256 validation before archiving, age multi-recipient encryption, ephemeral slices
- **Volume management** (`src/volume/`): Layout-v2 self-describing layout (`volume-format-v2.md`), the typestate write session (`session.rs`), Layout build/materialize (`build.rs`), front-index/seal parsers (`format.rs`), verify, read-slices
- **Tape I/O** (`src/tape/`): kernel st driver via ioctl, fixed 512KB block mode
- **Crypto** (`src/crypto/`): age multi-recipient encryption, per-tenant key isolation
- **Policy** (`src/policy/`): 3-level resolver (dotfile > archive_set > defaults), advisory audit
- **Store trait** (`src/store.rs`): built (ADR-0006) — `capacity`/`execute`/`confirm`/`read_file`, streaming so RAM tracks block size not slice size. `TapeStore` and `MemStore` share one chain-walk implementation, so MemStore-based tests exercise the real confirm path. `WarehouseStore`/`ExportStore` are the remaining peers (#72/#73)
- **Collection** (`src/collection/`): `[[collections]]` config → `collection sync|status|plan|run`; folder-per-unit registration, alphabetical first-fit batch selector, stage-once/write-N-copies/release

**Design principles:**
- Volumes are self-describing — full restore possible without the database or tapectl
- Strict tenant isolation — zero content metadata in plaintext on tape; tenant envelopes use age trial-decryption
- Multi-tenant bin-packing on shared volumes
- Physical cartridges tracked separately from logical volumes (cartridges can be erased/reused)
- Policy audit is advisory, never blocking (exit codes: 0=clean, 1=warn, 2=violation)

## External Dependencies

- `dar` ≥2.6 (recommended 2.7.20+) — archive creation/extraction
- `sg3-utils` — drive health diagnostics
- `mhvtl` — virtual tape library for development/testing
- `lsscsi`, `mt-st` — optional device discovery and debugging

## Key Rust Dependencies

- `clap` 4.6 (derive), `rusqlite` 0.39 (bundled), `age`/`rage` 0.11 (pinned: pre-1.0 API unstable)
- `quick-xml` 0.39, `nix` 0.29 (ioctl/fs), `sha2` 0.10, `uuid` 1 (v4/v7)
- `thiserror` 2, `anyhow` 1, `chrono` 0.4, `walkdir` 2, `serde` 1

## Configuration

- System config: `~/.tapectl/config.toml` (dar path, backends, locations, defaults, exclusions, policy)
- Database: `~/.tapectl/tapectl.db`
- Per-unit config: `.tapectl-unit.toml` in each archival directory
