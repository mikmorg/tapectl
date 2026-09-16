# tapectl

Multi-tenant archival storage management for LTO tape and exportable encrypted directories.

tapectl manages the full lifecycle of archiving data to LTO tape: directory scanning, dar archive creation, age encryption, tape writing with self-describing volume layouts, verification, restore, and policy compliance auditing.

## Features

- **Three-phase pipeline**: `snapshot create` (fast metadata scan) -> `stage create` (dar archive + age encrypt) -> `volume write` (tape I/O)
- **Multi-tenant isolation**: zero content metadata in plaintext on tape; tenant envelopes use age trial-decryption
- **Self-describing volumes**: every tape is fully restorable without the database or tapectl itself (via RESTORE.sh)
- **Policy engine**: archive sets with 3-level resolution (unit dotfile > archive set > system defaults), compliance audit with action plans
- **Compaction workflow**: read live slices from underutilized tapes, rewrite to new tapes, retire old ones
- **Full audit trail**: every state change logged with old/new values
- **12 report types**: summary, fire-risk, copies, tape-only, dirty, pending, verify-status, health, capacity, age, events, compaction-candidates
- **FTS5 catalog search**: fast full-text search across all archived file paths

## Prerequisites

- Rust 1.94+ (for building; pinned via rust-toolchain.toml)
- `dar` >= 2.6 (recommended 2.7.20+) for archive creation/extraction
- `mhvtl` for development/testing (virtual tape library)
- LTO tape drive + `mt-st` for production use

## Build

```bash
cargo build --release
cargo test
```

The binary is at `target/release/tapectl`.

## Quick Start

The guided route: `scripts/first-run.sh` walks from a bare machine to the first
sealed tape — toolchain, `dar`, build, tests, a `tapectl` service user that
owns the keys and catalog, finding the drive by serial, `init` (with the escrow
secret explained before it is printed), the Heir Kit, tenants and units (each
tree granted to the service user by ACL), an optional rehearsal on a test
cartridge, and the first write with `verify --full`. Resumable with `--from N`;
`--home DIR` rehearses against a throwaway home; `--no-service-user` runs
everything as you. The manual route follows.

One drive handles more than one LTO generation. Declare what the drive *is*
(`generation = "LTO-6"`); each cartridge's own generation is read from its
density code when the volume is initialised, and that is what fixes the tape's
capacity and whether the drive may write it at all.


```bash
# Initialize tapectl (creates ~/.tapectl with DB, config, operator keys)
tapectl init --operator mike

# Register a storage location
tapectl location add home-rack --description "Home server rack"

# Add a tenant
tapectl tenant add mike --description "Personal media"

# Register a directory as an archival unit
tapectl unit init /media/tv/breaking-bad --tenant mike --tag tv --tag drama

# Or bulk-register all subdirectories
tapectl unit init-bulk /media/tv --tenant mike --tag tv

# Create a snapshot (fast directory walk)
tapectl snapshot create tv/breaking-bad/s01

# Stage for tape (dar archive + age encrypt)
tapectl stage create tv/breaking-bad/s01

# Initialize a tape volume
tapectl volume init L6-0001 --device /dev/nst0

# Write to tape
tapectl volume write L6-0001 --device /dev/nst0

# Verify
tapectl volume verify L6-0001 --device /dev/nst0

# Or do it all in one step
tapectl quick-archive /media/tv/new-show --tenant mike --volume L6-0001
```

## Command Reference

```
tapectl init                    Bootstrap DB, config, operator tenant + keys
tapectl tenant                  add, list, info, reassign, delete
tapectl key                     generate, list, export, import, rotate, escrow-kit
tapectl unit                    init, init-bulk, list, status, tag, rename,
                                discover, check-integrity, mark-tape-only
tapectl snapshot                create, list, diff, delete, mark-reclaimable, purge
tapectl stage                   create, list, info
tapectl staging                 status, clean
tapectl volume                  init, write, resume, abort, verify, identify,
                                move, retire, read-slices, plan, deposit,
                                compact-read, compact-write, compact-finish, compact
tapectl cartridge               register, relabel, list, info, move, retire, mark-erased
tapectl archive-set             create, edit, list, info, sync
tapectl audit                   Policy compliance (--action-plan, --json)
tapectl catalog                 ls, search, locate, stats, rebuild
tapectl location                add, list, info, rename
tapectl report                  summary, fire-risk, copies, tape-only, dirty,
                                pending, verify-status, health, capacity, age,
                                events, compaction-candidates
tapectl restore                 unit, file, raw-volume
tapectl export                  Encrypted slices to directory
tapectl import                  Pre-existing volume into DB
tapectl collection              sync, status, plan, run
tapectl backend                 add
tapectl quick-archive           Create + stage + write in one flow
tapectl db                      backup, fsck, export, import, stats
tapectl config                  show, check
tapectl completions             Shell completion generation
```

All commands support `--json` for machine-readable output.

## Volume Layout

Each tape contains a self-describing 10-file layout:

| Position | Contents | Encrypted? |
|----------|----------|-----------|
| 0 | ID thunk (label, layout, metadata) | No |
| 1 | System guide (recovery manual) | No |
| 2 | RESTORE.sh (automated recovery) | No |
| 3 | Planning header | Operator |
| 4..N | Data slices (dar + age) | Tenant+Operator |
| N+1 | Mini-index (position map) | No |
| N+2..K | Tenant envelopes (shuffled) | Per-tenant |
| K+1,K+2 | Operator envelopes (dual) | Operator |

## Configuration

System config at `~/.tapectl/config.toml`:

```toml
[dar]
binary = "/opt/dar/bin/dar"

[staging]
directory = "/mnt/staging"

[defaults]
slice_size = "2400G"
encrypt = true
min_copies_for_tape_only = 2
min_locations_for_tape_only = 2

[compaction]
utilization_threshold = 0.50
```

Per-unit config at `.tapectl-unit.toml` in each directory.

## Architecture

```
src/
  cli/          Clap-based subcommands (21 modules)
  collection/   Folder-per-unit source roots: sync, plan, run
  db/           SQLite with WAL, forward-only migrations, FTS5;
                ontape_catalog.rs is the operator envelope's catalog.db
  policy/       3-level resolver; coverage.rs (copy/location SQL),
                escrow.rs (escrow coverage: covered / unknown / gap)
  store.rs      The Store seam (ADR-0006): TapeStore, MemStore
  unit/         Archival units, dotfiles, discovery
  staging/      dar + age pipeline, sha256 validation
  volume/       Layout v2: build, session (typestate write), format
                (front index / seal / ID thunk parsers), manifest
                (MANIFEST.toml both directions), envelope (reads one
                back), restore_script (RESTORE.sh from named awk
                fragments), rebuild (catalog rebuild), raw, restore
  tape/         Linux st driver via ioctl
  crypto/       age multi-recipient encryption
  dar/          dar subprocess wrapper, XML catalog parsing
  tenant/       Multi-tenant management
  config.rs     TOML config parsing
  error.rs      Error types + exit codes
  signal.rs     SIGINT handling
```

## Testing

Default `cargo test` runs unit tests, integration tests, the tenant-
isolation crypto tests, and library-level failure-mode tests — none
require tape hardware or mhvtl:

```bash
cargo test
```

**`dar` must be installed and on `PATH`.** The ungated suite is not
hermetic: 13 tests build and extract real archives, so they need the same
`dar >= 2.6` the tool itself requires (Debian/Ubuntu: `sudo apt install
dar`). `tests/test_dependencies.rs` checks this once and fails with
instructions naming what would otherwise break, rather than letting the
absence surface as a dozen unexplained archive errors.

Two gated test suites exist for heavier validation:

```bash
# mhvtl end-to-end round-trip, tenant isolation on real tape layout,
# health log collection. DEVICE NUMBERING IS NOT STABLE across reboots on a
# host that also has a real drive: always set TAPECTL_GATE_TAPE, and check
# `ls -l /dev/tape/by-id/` first (scsi-XYZZY_A* are mhvtl).
TAPECTL_GATE_TAPE=/dev/nst1 TAPECTL_MHVTL=1 \
    cargo test --test mhvtl_e2e -- --ignored --nocapture

# The two operator-level suites on mhvtl: the 26-leg verification gate, and
# the lifecycle suite (years of use in minutes, 13 scenarios, a 10-way
# restore matrix). Both are documented in docs/.
TAPECTL_GATE_TAPE=/dev/nst1 TAPECTL_MHVTL=1 scripts/mhvtl-verify-gate.sh
scripts/lifecycle-suite.sh --scenario first-year --device /dev/nst1

# Performance scenarios (many files, many units, large file).
# Gated because a full run takes ~2 minutes.
TAPECTL_PERF_TESTS=1 cargo test --test performance --release -- \
    --ignored --nocapture --test-threads=1
```

## Documentation

- `docs/operator-guide.md` — day-to-day operations, worked examples
- `docs/perf-baselines.md` — performance regression baselines
- `docs/lto6-validation-checklist.md` — real-hardware validation steps
- `docs/man/` — generated man pages (`man -l docs/man/tapectl.1`)
- `tapectl-design-v4_0.md` — full design document and implementation
  reference

Regenerate man pages after any CLI change:

```bash
cargo run --example gen_man
```

## License

See LICENSE file.
