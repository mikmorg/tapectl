# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tapectl is a Rust CLI tool for managing long-term archival storage across LTO tape and exportable encrypted directories (Blu-ray, USB). It wraps `dar` for archive creation/extraction, uses the `rage` crate for age encryption, and SQLite for catalog/inventory/policy/audit.

The sole implementation reference is `tapectl-design-v4_0.md` — no other design documents are required.

## Current State

The project is in pre-implementation phase. Only the design document exists. Implementation follows a milestone-gated roadmap where Milestone 0 (external dependency validation with a full round-trip: dar → encrypt → tape → read → decrypt → extract) is a hard gate before any further work.

## Build Commands (once Cargo.toml exists)

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
- **Backend trait** (`src/backend/`): pluggable storage — LTO (`lto.rs`), export directories (`export.rs`), S3 stub (deferred)
- **Volume management** (`src/volume/`): 8-file self-describing layout, best-fit-decreasing bin packing, ENOSPC recovery, compaction
- **Tape I/O** (`src/tape/`): kernel st driver via ioctl, MAM chip queries, variable block mode
- **Crypto** (`src/crypto/`): age/rage multi-recipient encryption, per-tenant key isolation
- **Policy** (`src/policy/`): archive sets (named policy templates), resolver with inheritance (unit > archive_set > defaults), advisory audit

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
