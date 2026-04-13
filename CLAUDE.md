# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tapectl is a Rust CLI tool for managing long-term archival storage across LTO tape and exportable encrypted directories (Blu-ray, USB). It wraps `dar` for archive creation/extraction, uses the `rage` crate for age encryption, and SQLite for catalog/inventory/policy/audit.

The sole implementation reference is `tapectl-design-v4_0.md` — no other design documents are required.

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

**Post-M7 hardening (complete):** Design gap audit identified 3 active bugs + 6 unacknowledged gaps. All 8 items fixed: clone-slices restructured to staging-only read-slices (self-describing invariant preserved), restore trial-decrypts with all tenant+operator keys (key rotation no longer breaks restore), compact-finish refuses retirement if live slices lack copies elsewhere, volume_verify records verification_sessions (audit feedback loop closed), staging cleanup reports actual bytes freed, compact-read errors on checksum mismatch, critical DB operations wrapped in transactions, export writes MANIFEST.toml + RECOVERY.md. 105 tests (59 unit + 46 integration/lib/isolation/failure-mode), zero clippy warnings.

**Next:** Real LTO-6 hardware validation — user-gated on physical drive availability. Procedure is in `docs/lto6-validation-checklist.md`.

## Build Commands

```bash
cargo build --release
cargo test
cargo clippy --all-targets
cargo fmt --check
```

## Architecture

**Three-phase pipeline:** `snapshot create` (fast metadata) → `stage create` (dar + sha256 + encrypt) → `volume write` (tape I/O)

**Key subsystems:**
- **CLI layer** (`src/cli/`): clap derive-based subcommands (tenant, unit, snapshot, stage, volume, catalog, restore, audit, etc.)
- **Database** (`src/db/`): SQLite with WAL mode, forward-only numbered migrations, full audit trail
- **Unit management** (`src/unit/`): archival entities tracked via `.tapectl-unit.toml` dotfiles in each directory
- **dar integration** (`src/dar/`): subprocess wrapper; minimum dar 2.6.x; XML catalog parsing via quick-xml
- **Staging** (`src/staging/`): sha256 validation before archiving, age multi-recipient encryption, ephemeral slices
- **Volume management** (`src/volume/`): 10-file self-describing layout, write pipeline, verify, read-slices
- **Tape I/O** (`src/tape/`): kernel st driver via ioctl, fixed 512KB block mode
- **Crypto** (`src/crypto/`): age multi-recipient encryption, per-tenant key isolation
- **Policy** (`src/policy/`): 3-level resolver (dotfile > archive_set > defaults), advisory audit
- **Backend trait** (`src/backend/`): deferred — tape I/O currently direct via `src/tape/`

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
