# tapectl Operator Guide

## Initial Setup

### 1. Install Dependencies

```bash
# dar (archive tool)
sudo apt install dar
# or build from source for 2.7.20+

# For virtual tape testing
sudo apt install mhvtl lsscsi mt-st sg3-utils

# For real LTO hardware
sudo apt install mt-st sg3-utils
```

### 2. Initialize tapectl

```bash
tapectl init --operator mike
```

This creates:
- `~/.tapectl/tapectl.db` (SQLite database)
- `~/.tapectl/config.toml` (system configuration)
- `~/.tapectl/keys/` (age encryption keypairs)
- Operator tenant "mike" with primary + backup keys

### 3. Configure

Edit `~/.tapectl/config.toml`:

```toml
[dar]
binary = "/usr/bin/dar"    # Path to dar binary

[[backends.lto]]
name = "lto-primary"
device_tape = "/dev/nst0"
media_type = "LTO-6"
nominal_capacity = "2500G"

[staging]
directory = "/mnt/staging"  # Needs space for dar + encrypted slices

[defaults]
slice_size = "2400G"
min_copies_for_tape_only = 2
min_locations_for_tape_only = 2

[discovery]
watch_roots = ["/media/tv", "/media/movies"]
```

Validate with:
```bash
tapectl config check
```

## Day-to-Day Operations

### Register Units

```bash
# Single directory
tapectl unit init /media/tv/breaking-bad --tenant mike --tag tv

# All subdirectories at once
tapectl unit init-bulk /media/tv --tenant mike --tag tv

# Auto-discover from watch_roots
tapectl unit discover
```

### Archive to Tape

```bash
# Step 1: Snapshot (fast directory walk)
tapectl snapshot create tv/breaking-bad/s01

# Step 2: Stage (dar archive + encrypt — needs staging disk space)
tapectl stage create tv/breaking-bad/s01

# Step 3: Write to tape
tapectl volume init L6-0001 --device /dev/nst0
tapectl volume write L6-0001 --device /dev/nst0

# Step 4: Verify
tapectl volume verify L6-0001 --device /dev/nst0
```

### Check What's Pending

```bash
tapectl stage list --status staged
tapectl volume plan --copies 2
tapectl report pending
```

### Restore

```bash
# Full unit
tapectl restore unit --unit tv/breaking-bad/s01 --from L6-0001 --to /tmp/restore

# Single file
tapectl restore file --file season1/episode01.mkv --unit tv/breaking-bad/s01 \
  --from L6-0001 --to /tmp/restore

# Dry run
tapectl restore unit --unit tv/breaking-bad/s01 --from L6-0001 --to /tmp --dry-run
```

### Search the Catalog

```bash
tapectl catalog search "episode01"
tapectl catalog ls tv/breaking-bad/s01
tapectl catalog locate tv/breaking-bad/s01
tapectl catalog stats
```

## Safety Operations

### Locations and Movement

```bash
tapectl location add home-rack --description "Home server rack"
tapectl location add parents-house --description "Offsite backup"
tapectl volume move L6-0001 --to parents-house
```

### Copy Management

```bash
# Check copy counts
tapectl report copies
tapectl report fire-risk

# Read slices from tape into staging, then write to a second tape
tapectl volume read-slices --from L6-0001 --unit tv/breaking-bad/s01
# Swap tape, then write with full self-describing layout
tapectl volume write L6-0002
```

### Mark Tape-Only

When local disk copies are no longer needed:

```bash
# Enforces min_copies and min_locations
tapectl unit mark-tape-only tv/breaking-bad/s01

# Check integrity before deleting local data
tapectl unit check-integrity tv/breaking-bad/s01
```

### Retire a Volume

```bash
# Shows impact analysis: which units lose copies
tapectl volume retire L6-0001
```

## Policy and Compliance

### Archive Sets

```bash
# Create a policy template
tapectl archive-set create critical-media \
  --min-copies 3 \
  --required-locations "home-rack,parents-house" \
  --verify-interval-days 180

# Import from config.toml
tapectl archive-set sync
```

### Audit

```bash
# Check compliance
tapectl audit

# Show remediation commands
tapectl audit --action-plan

# JSON for scripting
tapectl audit --json
```

Exit codes: 0 = clean, 1 = warnings, 2 = violations.

### Reports

```bash
tapectl report summary
tapectl report fire-risk
tapectl report copies --unit tv/breaking-bad/s01
tapectl report tape-only
tapectl report capacity --per-volume
tapectl report compaction-candidates
tapectl report events --days 30
```

## Compaction

When tapes become underutilized (snapshots superseded and marked reclaimable):

```bash
# Check candidates
tapectl report compaction-candidates

# Mark old snapshots as reclaimable (enforced preconditions)
tapectl snapshot mark-reclaimable tv/breaking-bad/s01 --version 1

# Three-step compaction
tapectl volume compact-read L6-0001 --device /dev/nst0
# (swap tape)
tapectl volume compact-write --destination L6-0010 --device /dev/nst0
tapectl volume compact-finish L6-0001

# Or interactive one-step
tapectl volume compact L6-0001 --device /dev/nst0
```

## Cartridge Tracking

```bash
tapectl cartridge register --barcode L6-0001 --media-type LTO-6
tapectl cartridge list
tapectl cartridge info L6-0001
tapectl cartridge mark-erased L6-0001  # After physical erase
```

## Key Management

```bash
tapectl key list --tenant mike
tapectl key generate --tenant mike --alias 2026-primary
tapectl key rotate --tenant mike
tapectl key export mike-primary > mike-primary.age.pub
```

Old keys are never deleted — only deactivated. Restore tries all known keys.

## Database Operations

```bash
tapectl db backup --to /backup/tapectl.db
tapectl db fsck --repair
tapectl db stats
tapectl db export  # JSON row counts
```

## Disaster Recovery

Every tape is self-describing. If the database is lost:

1. Read the ID thunk: `tapectl volume identify --device /dev/nst0`
2. Use RESTORE.sh on the tape (file position 2) for guided recovery
3. Operator envelope contains a portable catalog.db subset

## Multi-Tenant Setup

```bash
tapectl tenant add alice --description "Alice's media"
tapectl tenant add bob --description "Bob's documents"

# Each tenant gets independent encryption keys
# Tenant A cannot see Tenant B's data on shared tapes
# Operator can always decrypt everything

# Reassign units between tenants
tapectl tenant reassign alice --to bob
```
