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

- Rust 1.75+ (for building)
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
tapectl key                     generate, list, export, import, rotate
tapectl unit                    init, init-bulk, list, status, tag, rename,
                                discover, check-integrity, mark-tape-only
tapectl snapshot                create, list, diff, delete, mark-reclaimable, purge
tapectl stage                   create, list, info
tapectl staging                 status, clean
tapectl volume                  init, write, verify, identify, move, retire,
                                clone-slices, plan,
                                compact-read, compact-write, compact-finish, compact
tapectl cartridge               register, list, info, mark-erased
tapectl archive-set             create, edit, list, info, sync
tapectl audit                   Policy compliance (--action-plan, --json)
tapectl catalog                 ls, search, locate, stats
tapectl location                add, list, info, rename
tapectl report                  summary, fire-risk, copies, tape-only, dirty,
                                pending, verify-status, health, capacity, age,
                                events, compaction-candidates
tapectl restore                 unit, file
tapectl export                  Encrypted slices to directory
tapectl import                  Pre-existing volume into DB
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
  cli/          Clap-based subcommands (18 modules)
  db/           SQLite with WAL, forward-only migrations, FTS5
  policy/       3-level policy resolver
  unit/         Archival units, dotfiles, discovery
  staging/      dar + age pipeline, sha256 validation
  volume/       10-file tape layout, write, verify, clone, compact
  tape/         Linux st driver via ioctl
  crypto/       age multi-recipient encryption
  dar/          dar subprocess wrapper, XML catalog parsing
  tenant/       Multi-tenant management
  config.rs     TOML config parsing
  error.rs      Error types + exit codes
  signal.rs     SIGINT handling
```

## License

See LICENSE file.
