# tapectl Operator Guide

## Initial Setup

### 1. Install Dependencies

```bash
# dar (archive tool)
sudo apt install dar
# or build from source for 2.7.20+

# For virtual tape testing — mhvtl is NOT packaged in Ubuntu; build from source
sudo apt install lsscsi mt-st sg3-utils mtx
git clone https://github.com/markh794/mhvtl.git && cd mhvtl
make && sudo make install          # userspace daemons + systemd units

# The kernel module must be registered with DKMS or every kernel update
# silently kills mhvtl (module gone, /dev/nst0 disappears, gated tests
# skip quietly). Copy kernel/ + include/ to /usr/src/mhvtl-<ver>/ with a
# dkms.conf and run: dkms add/build/install -m mhvtl -v <ver>
# This VM's working setup: /usr/src/mhvtl-1.8.0 (built 2026-07-20).

# For real LTO hardware
sudo apt install mt-st sg3-utils
```

**After any kernel update:** DKMS rebuilds mhvtl automatically if registered
(verify with `dkms status`). If `/dev/nst0` is missing, check
`lsmod | grep mhvtl`, then `systemctl start mhvtl.target`. Note that SCSI
enumeration can shuffle across module reloads — discover the changer with
`lsscsi -g` (look for `mediumx`) rather than assuming `/dev/sg0`, and load a
media-type-compatible cartridge for the drive (an L6 tape for a TD6 drive:
`mtx -f <changer-sg> load <slot> <dte>`).

#### Virtual tape storage must NOT live on `/` (2026-07-27)

mhvtl defaults to `/opt/mhvtl`, which on this VM is the 22 GB root partition.
Each virtual tape grows to `CAPACITY` (500 MB here), so a handful of full tapes
fills `/` and breaks the machine — the gated suites write real volumes, so this
is a live risk, not a theoretical one. Tape storage now lives on `/scratch`
(the partition designated for anything that needs no backup):

```bash
/scratch/mhvtl                      # actual tape images
/opt/mhvtl -> /scratch/mhvtl        # symlink (see "why a symlink" below)
/etc/mhvtl/mhvtl.conf               # MHVTL_HOME_PATH=/scratch/mhvtl
/etc/mhvtl/device.conf              # ' Home directory: /scratch/mhvtl' (BOTH library stanzas)
```

**Why a symlink is required, not just config.** `MHVTL_HOME_PATH` is a
**compile-time `#define`** in the mhvtl userspace binaries, not a runtime
setting — `strings /usr/bin/vtltape` contains the fully-formed message
`Unable to change directory to /opt/mhvtl`, and `vtltape` accepts no `-H`
override (unlike `mktape`). Each `vtltape` daemon `chdir()`s to that baked-in
path at startup and **exits 255 if it does not exist**, no matter what the
config files say. `vtllibrary` is unaffected (it does not chdir), so the
symptom is: libraries start, all four tape daemons fail, no `/dev/nst*`
appears. Editing both config files is still necessary — that is what actually
relocates the media — but the symlink is what lets the daemons start.

**Verifying the relocation really took effect** (a clean `systemctl start` is
not sufficient evidence — the symlink means either path resolves):

```bash
# Load a tape, then confirm the daemon's open files are under /scratch:
for p in $(pgrep -f 'vtltape -F'); do sudo ls -l /proc/$p/fd; done | grep mhvtl
# Expect: /scratch/mhvtl/<PCL>/data.0 — never /opt/mhvtl/...
```

**Reinstall/upgrade risk:** `make install` or a package upgrade may rewrite
`/etc/mhvtl/*.conf` back to `/opt/mhvtl` and could replace the symlink with a
real directory. After any mhvtl reinstall, re-check all three items above —
otherwise tapes silently start filling `/` again with no error until the disk
is full.

Note: loading a tape rewrites its `mam` and `mhvtl_data` files (mount counter,
last-mount timestamp) while `data.0`/`indx.0`/`meta.0` stay byte-identical —
expected, not corruption, when comparing tape trees before and after a mount.

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
2. Extract RESTORE.sh from the tape (file position 2):
   ```bash
   mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 2
   dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE.sh
   chmod +x RESTORE.sh
   ```
3. Use RESTORE.sh for guided recovery (requires mt, dd, age, dar, sha256sum):
   ```bash
   ./RESTORE.sh --info                                    # see tape layout
   ./RESTORE.sh --find-envelope --key your.age.key        # find your data
   ./RESTORE.sh --restore --key your.age.key --to /dest   # full restore
   ```
4. Operator envelope contains a full catalog across all tenants

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
