# tapectl — Multi-Tenant Archival Storage Management System

## Design Document v4.0 (Comprehensive Implementation Specification)

*Consolidates v3.0 spec, compaction/cartridge addendum, design decisions with
rationale, worked SQL queries, and implementation guidance from deep analysis
review. This is the sole implementation reference — no other design documents
are required.*

---

## 1. Overview

`tapectl` is a Rust CLI tool for managing long-term archival storage across
LTO tape (any generation) and exportable encrypted directories (for Blu-ray,
USB, etc.). It wraps `dar` for archive creation/extraction, uses the `rage`
crate (pure-Rust age implementation) for encryption, and SQLite for catalog,
inventory, health tracking, policy enforcement, and audit logging.

**S3-compatible object storage is designed into the backend trait but deferred
from the initial release.** The architecture supports it; the implementation
ships when the core LTO+export workflow is proven. All schema, config, and
CLI structures accommodate S3 without future migration.

### 1.1 Capabilities

- **Multi-tenant** — tenants are data owners with isolated encryption keys;
  operator holds all keys; per-tenant encrypted envelopes on shared volumes;
  tenants can self-restore from tape with their key + dar + age
- **Three-phase pipeline** — `snapshot create` (fast metadata), `stage create`
  (dar + sha256 validation + encrypt), `volume write` (tape I/O)
- **Stage sets** — bridge between logical snapshots and physical volumes;
  reusable across writes; tracks byte-identity of copies
- **Self-describing volumes** — 8-file layout: ID thunk, LLM-friendly system
  guide, RESTORE.sh, planning header, data slices, mini-index, per-tenant
  encrypted envelopes, dual operator envelopes. Full restore possible without
  database or tapectl installation
- **Strict tenant isolation** — zero content metadata in plaintext on tape;
  all filenames, sizes, hashes, unit names, tenant names inside encrypted
  envelopes only; tenants find their envelope via age trial-decryption
- **Volume sharing** — multiple tenants bin-packed on shared volumes; per-tenant
  encrypted envelopes ensure isolation
- **Physical cartridge tracking** — cartridges are tracked separately from
  logical volumes; cartridges can be erased and reused across multiple volume
  lifetimes; MAM chip data recorded per cartridge
- **Volume compaction** — underutilized volumes are flagged by audit;
  explicit three-step compaction workflow (compact-read → compact-write →
  compact-finish) relocates live data and frees cartridges for reuse
- **Archive sets** — named policy templates: required locations, min copies,
  encryption key, compression, verification interval, slice size
- **Policy audit** — `tapectl audit` reports compliance gaps with action plans;
  advisory, never blocking; exit codes for scripting (0=clean, 1=warn, 2=violation)
- **Accurate capacity management** — MAM chip query for real tape capacity;
  no static guessing; hardware compression disabled for encrypted data
- **Bitrot protection** — always sha256 source validation before staging;
  `tapectl unit check-integrity` compares disk against staged checksums
- **Volume clone** — tape-to-tape byte-copy for tape-only data redundancy
  recovery; no decryption needed
- **End-of-tape safety** — layered ENOSPC recovery: normal metadata write in
  early-warning zone; fallback overwrites incomplete slice position; last-resort
  sacrifices last complete slice. Tape always remains self-describing.
- **Verification journal** — per-session, per-slice results with drive error
  counter tracking
- **Full audit trail** — every state change to every entity logged
- **Filesystem faithful** — xattrs, ACLs, ownership, symlinks (dar `-D`)
- **Safe Ctrl+C** — atomic write tracking; interrupted sessions resumable
- **DB safety** — WAL mode, auto-recovery, `db fsck --repair`, passphrase-
  encrypted backup including keys + catalogs

### 1.2 dar Requirements

**Minimum version: dar 2.6.x** (supports `-T xml` listing format).
**Recommended version: dar 2.7.20** or dar 2.8.4 (both released March 2026).

dar's XML listing mode (`dar -l ARCHIVE -T xml`) produces structured XML
output parsed by tapectl using the `quick-xml` crate. Available since dar 2.4.x.

**Installation:** Debian stable ships dar 2.6.2. For 2.7+, compile from
source with `--prefix=/opt/dar`:

```bash
apt install build-essential libgcrypt20-dev libargon2-dev \
    zlib1g-dev libbz2-dev liblzma-dev libzstd-dev \
    libcurl4-openssl-dev libthreadar-dev pkg-config

wget https://sourceforge.net/projects/dar/files/dar/2.7.20/dar-2.7.20.tar.gz
tar xf dar-2.7.20.tar.gz && cd dar-2.7.20
./configure --prefix=/opt/dar
make -j$(nproc) && make install-strip
```

dar is fully backward-compatible with all older archive formats.
Each stage_set records the dar version and exact command line used.
Development uses **mhvtl** (virtual tape library). Validate on real hardware.

**Why dar over tar:** dar provides built-in slicing across volumes, file-level
catalog without extracting the whole archive, structured XML listing output,
and native support for encryption-per-slice. tar would have required building
all of this on top. dar is the archive format; tapectl is the management
layer above it.

### 1.3 Technology Stack Summary

| Component | Choice | Version | Rationale |
|-----------|--------|---------|-----------|
| Language | Rust | stable | Correctness, long-term maintainability |
| Archive | dar (subprocess) | ≥2.6 | Only tool with native volume splitting + catalogs for tape |
| Encryption | age via `rage` crate | 0.11.x (pin) | Simple multi-recipient, pure Rust, no GPG complexity |
| Database | SQLite via `rusqlite` | 0.39+ bundled | Single-file, embedded, WAL mode, no daemon |
| Tape I/O | Kernel st driver + nix ioctl | /dev/nst0 | Simple, auto-filemark on crash, mt-st compatible |
| Tape diagnostics | sg_logs (shell-out) | sg3-utils | Complex SCSI log pages, infrequent (2-3x per session) |
| XML parsing | quick-xml | 0.39 | 50x faster than xml-rs, serde integration |
| CLI | clap | 4.6 | Derive API, subcommands, completions |

**Explicitly rejected:** sled (unreliable, abandoned pre-1.0), LTFS (slow for
large archives, filesystem metaphor breaks at scale), Bacula/Amanda
(backup-oriented with retention cycles, not archival), GPG/sequoia (complex
key management, keyring daemon), full SG_IO interface (unnecessary ~2000 lines
of SCSI protocol code for standalone drive).

### 1.4 Operator Environment

- **Server:** Debian, SSH-only, home lab with HDD RAID staging
- **Tape drive:** LTO-6, standalone (no autoloader), tested and working
- **dar version:** 2.6.2 installed (system package), 2.7.20 recommended
  (install from source to /opt/dar)
- **Data volume:** 30-100 TB of archival media
- **Tenants:** 5-20 expected
- **Physical locations:** 2+ (home + offsite), flexible growth

---

## 2. Core Concepts

### 2.1 Tenants

A **tenant** is a data owner. The operator is a tenant — no special cases.
The operator generates all keypairs and holds all private keys.

- Each tenant has their own age keypair(s) (primary + backup)
- Slices encrypted to: tenant's active key(s) + operator's active key(s)
- Operator can decrypt everything; tenants only their own data
- Multiple tenants bin-packed on shared volumes
- Per-tenant envelopes on volumes contain only that tenant's catalog
- Tenant can restore from tape with: their private key + dar + age +
  RECOVERY.md instructions from their envelope

**Offboarding:** `tapectl tenant reassign SOURCE --to TARGET` moves all
units, then `tapectl tenant delete SOURCE`.

**Design rationale:** The operator is "just a tenant" with the `is_operator`
flag. This eliminates special-case code paths for operator data. The operator
generates keypairs for all tenants and distributes private keys, since the
operator has sole physical access to drives. Per-tenant encrypted envelopes
on shared volumes give isolation without wasting capacity on per-tenant
dedicated tapes.

### 2.2 Units

A **unit** is a logical archival entity. Always a **single directory**.
Identified by a UUID in `.tapectl-unit.toml`:

```toml
[unit]
uuid = "a1b2c3d4-5678-9abc-def0-1234567890ab"
name = "tv/breaking-bad/s01"
created = "2026-04-07T12:00:00Z"
tags = ["tv", "drama", "breaking-bad"]
tenant = "mike"
archive_set = "bulk-media"

[policy]
checksum_mode = "mtime_size"
compression = "none"

[excludes]
patterns = ["*.nfo", "*.txt", "Thumbs.db", ".DS_Store"]
```

- Auto-naming from directory path. Rename with `tapectl unit rename`.
- Name uniqueness enforced. Collision suffixes on `init-bulk`.
- Dotfile included in dar archives for self-registering restores.
- `discover` reads dotfiles → DB. CLI updates DB → dotfile. DB wins on conflict.
- Nested unit detection: `unit init` and `snapshot create` check parent/child. Both errors.
- Empty units: warn but allow.

**Design rationale:** The UUID decouples identity from filesystem path —
if directories are reorganized/moved, tracking is maintained. Nested units
are prohibited because archiving the parent would include the child's data,
creating ambiguity. Tags (many-to-many) are used for grouping instead of
hierarchical collections, as they are simpler and more flexible.

### 2.3 Three-Phase Pipeline

```
snapshot create  →  stage create    →  volume write
  (seconds)          (min-hours)        (min-hours)
  manifest only      dar+sha256+encrypt tape I/O
```

**Phase 1 — `tapectl snapshot create`:**
Fast directory walk. Records manifest (paths, sizes, mtimes, metadata).
Populates `files` table (sha256=NULL). Warns on files > `large_file_warn_threshold`.
No dar involvement. Status: `created`.

**Phase 2 — `tapectl stage create`:**
Validates source against manifest (**always sha256**, regardless of checksum_mode).
Runs dar (with `-D`, `-am`, `--acl`, `--fsa-scope linux_extX`, excludes, `--hash sha256`).
Encrypts slices (multi-recipient: tenant keys + operator keys).
Extracts local dar catalog (first stage per snapshot).
Backfills `files.sha256` and `manifest_entries.sha256` (first stage only).
Creates `stage_set` + `stage_slices`. Writes receipt. Status: `staged`.
Optional: `--verify` runs `dar -t` after archive creation.

**Phase 3 — `tapectl volume write`:**
Queries MAM for real tape capacity. Writes ID thunk, system guide, RESTORE.sh,
planning header. Writes data slices. On completion: writes mini-index, per-tenant
envelopes (shuffled), dual operator envelopes. Creates `write` + `write_positions`.
Status: `current` (first completed write). Optional: `--write-verify` reads back.

**Reuse:** Staged slices persist until `tapectl staging clean`. Second write
reuses same slices (checksums verified first).

**Re-staging:** After cleanup, `stage create` re-validates source, re-runs dar,
creates new stage_set. Different archive bytes, identical logical content.

**Design rationale:** The three-phase separation decouples content identity from
physical artifacts. Each phase has a clear input, output, and failure mode. Each
is independently resumable. Create is fast (seconds), stage is CPU/IO-bound
(minutes-hours), write is tape-bound (minutes-hours). Keeping them separate
lets the operator control when each resource-intensive phase runs and provides
natural break points during evening/weekend sessions.

**Implementation note:** The sha256 backfill creates a temporal dependency —
`files.sha256` and `manifest_entries.sha256` are NULL after `snapshot create`
and populated during first `stage create`. Commands that need checksums
(check-integrity, catalog search with hash comparison) should use a helper
function `ensure_staged(snapshot_id)` that returns a clear error directing
the operator to stage first.

### 2.4 Data Model

```
tenant
  └── unit (UUID, single directory)
       └── snapshot (content identity: manifest + file catalog)
            └── stage_sets (each dar+encrypt run)
                 ├── stage_slices (encrypted artifacts)
                 └── writes (each volume copy)
                      └── write_positions (slice positions on volume)
```

- **Snapshot** = WHAT (content identity)
- **Stage set** = HOW (dar+encrypt execution)
- **Write** = WHERE (volume copy)
- Two writes from same stage_set = byte-identical
- Two writes from different stage_sets of same snapshot = logically identical, different bytes

**Design rationale:** Stage sets solve three problems simultaneously: staging
pressure (slices are ephemeral, re-created from source), multi-copy identity
(writes from the same stage_set are byte-identical), and verification anchoring
(each write's checksums trace back to a specific dar execution). This three-level
model (snapshot/stage_set/write) avoids the common mistake of conflating "what
I archived" with "where I put it."

### 2.5 Physical Cartridges vs. Logical Volumes

A **cartridge** is a physical tape medium with a barcode, wear history, and
load count. A **volume** is a logical write session — a specific set of data
written to a cartridge.

This separation enables cartridge reuse: when a volume is compacted or all its
data expires, the underlying cartridge can be erased and reused for a new
volume. LTO cartridges are rated for hundreds of full write passes and decades
of shelf life.

```
cartridge (physical, barcode "L6-0001")
  └── cartridge_volumes (join table with time range)
       ├── volume "L6-0001-A" (written 2026-04, active)
       └── volume "L6-0001-B" (written 2027-01, future reuse)
```

**Cartridge lifecycle:**

```
available → in_use → pending_erase → available (reuse)
                   → retired_permanent (end of life / too many errors)
         → offsite (physically moved to offsite location)
```

**Volume lifecycle:**

```
blank → initialized → active → full → retired → erased
                                     → compacted → erased
```

When a cartridge is registered, tapectl reads MAM data via `sg_read_attr` to
auto-populate manufacturer, serial number, tape length, and load count. Manual
registration is available for cartridges without MAM access.

### 2.6 Volume Layout (LTO)

Every tapectl LTO volume follows this 8-zone layout:

```
File 0:  ID thunk           (plaintext — short identity + "read next file")
File 1:  System guide       (plaintext markdown — LLM-friendly full recovery manual)
File 2:  RESTORE.sh         (plaintext bash — emergency restore script)
File 3:  Planning header    (age-encrypted to operator — planned contents list)
Files 4..N:                 (age-encrypted dar slices — actual archived data)
File N+1: Mini-index        (plaintext — structural position map, zero content metadata)
Files N+2..N+K:             (age-encrypted tenant envelopes — shuffled random order)
File N+K+1: Operator envelope     (age-encrypted to operator — full catalog)
File N+K+2: Operator envelope bk  (age-encrypted to operator — identical backup copy)
```

**Tenant isolation rule:** NOTHING in plaintext (Files 0, 1, 2, N+1) reveals
content, ownership, filenames, sizes, hashes, unit names, tenant names, or
key fingerprints. The mini-index contains only file position numbers, byte sizes,
and structural type labels (data_slice, tenant_envelope, etc.).

**Envelope shuffling:** Per-tenant envelopes are written in random order on
each tape. A tenant finds their envelope by trial-decrypting each envelope
header with their age key. This is built into the age format (recipient
stanzas in the header) and takes milliseconds per envelope.

**On append:** New slices + new metadata files written after old metadata.
Old metadata becomes dead space. No mid-tape overwrites ever (except EOT recovery).

See Section 8 for detailed file format specifications.

### 2.7 Volume Layout (Export)

`tapectl export --unit NAME --to /path/` dumps encrypted slices + envelope +
RESTORE.sh + system guide to a directory. Operator burns to BD-R, copies to USB.

### 2.8 Capacity Model

**LTO file marks consume only bytes, not megabytes.** The tape's block structure
is logical; file marks are small markers in the data stream.

Three-layer capacity model:

1. **Static default** (config, offline planning before tape loaded):
   `nominal_capacity × usable_capacity_factor` (default 0.92)
2. **MAM query** (tape loaded, before write):
   `sg_read_attr` reads `Remaining Capacity In Partition` and
   `Maximum Capacity In Partition` from the cartridge memory chip.
   Real per-cartridge capacity, accounting for ECC, rewrites, wear.
3. **ENOSPC detection** (during write):
   Early-warning zone (~20-30 MB before physical EOT). Hard stop.

**Hardware compression MUST be disabled** — encrypted data is random;
compression wastes CPU and may add framing overhead.

Bin packing formula:
```
available = mam_remaining_bytes            # if tape loaded and queried
          OR (nominal_capacity × usable_capacity_factor  # if planning offline
              - bytes_written)
          - manifest_reserve               # 200M default, scales with tenants
          - enospc_buffer                  # 50M safety margin
```

`tapectl volume calibrate` writes a test file and measures actual overhead.

### 2.9 End-of-Tape Recovery

Layered strategy ensures tape always remains self-describing:

1. **Normal (99%+):** ENOSPC during data slice → stop slices, write metadata
   (mini-index + envelopes) in early-warning zone. Metadata is small (typically
   < 80 MB even with many tenants); early-warning zone is 20-30 MB minimum.
2. **Fallback:** ENOSPC during metadata write → seek back to position of
   incomplete/failed slice, write metadata there. The partial slice was lost
   anyway; this reclaims its space for metadata.
3. **Last resort:** ENOSPC during metadata write AND no incomplete slice
   position available → seek back to start of last *complete* data slice,
   overwrite with metadata. Sacrifices one good slice (~2.4 GB) to ensure
   tape self-description. This slice is marked `sacrificed` in the write record.

The write record tracks: `eot_recovery` mode (normal/overwrite_incomplete/
sacrifice_last_slice) and `sacrificed_slice_id` if applicable.

### 2.10 Volume Compaction

Compaction reclaims underutilized tape cartridges by relocating live data from
sparse volumes to new volumes, then erasing and reusing the freed cartridges.

**When to compact:** `tapectl audit` flags volumes where the ratio of live data
to total written data falls below `compaction.utilization_threshold` (default
0.50). This is advisory — compaction is never automatic.

**Compaction trigger conditions:** A volume becomes underutilized when snapshots
on it are superseded and their predecessors are marked `reclaimable`. Until
explicitly marked reclaimable, superseded snapshots still count as live data.

**Three-step compaction workflow:**

```bash
# Step 1: Read live encrypted slices from source to staging
tapectl volume compact-read L6-0012
# (eject source tape)

# Step 2: Write compaction slices to destination via bin packing
tapectl volume compact-write
# (normal bin packing picks destination, may prompt for tape)

# Step 3: Retire source volume, free cartridge
tapectl volume compact-finish L6-0012
```

**Orchestration wrapper:** `tapectl volume compact L6-0012` walks through all
three steps interactively, prompting for tape swaps.

**Compaction mechanics:** Compaction reads encrypted slices without decryption
and re-stages them. The normal `volume write` pipeline then writes them to the
destination with a full self-describing layout. The original stage_set is
reused, so verification checksums remain valid. Bin packing treats compaction
slices the same as any other pending staged data.

**Compaction report:** `tapectl report compaction-candidates` shows per-volume
utilization breakdown including live data ratio, reclaimable bytes, and
estimated freed cartridges.

### 2.11 Reclaimable Gating

Snapshots do not become reclaimable automatically. The operator must explicitly
mark them:

```bash
tapectl snapshot mark-reclaimable "tv/bb/s01" --version 1
```

**Enforced preconditions** before a snapshot can be marked reclaimable:

1. A superseding snapshot must exist and have status `current`
2. The superseding snapshot must meet the unit's archive set policy
   (min_copies, min_locations)
3. For tape-only units, the requirements are multiplied by
   `tape_only_safety_multiplier` (default 2) — e.g., if min_copies=2,
   tape-only units need 4 copies of the superseding snapshot before the
   old one can be marked reclaimable

`--force` overrides all preconditions. The design philosophy is: never lose
data silently, but never block the operator who knows what they're doing.

### 2.12 Slice Size

Fixed at stage time. Resolution order:
1. Unit dotfile `[policy] slice_size`
2. Archive set `slice_size`
3. System `defaults.slice_size`

### 2.13 Change Detection and Integrity

- **Dirty detection** (`unit status --dirty`): Uses unit's `checksum_mode` (fast).
- **Source validation** (`stage create`): Always sha256. Bitrot protection.
  Shows estimated I/O time for large units.
- **Integrity checking** (`unit check-integrity`): sha256 disk vs staged checksums.
  Reports: OK, BITROT, MISSING, NEW. Requires ≥1 stage_set.

**Design rationale:** `checksum_mode` governs the fast `snapshot create` scan
and `unit status` checks. But staging is the archival commitment point — full
sha256 every time. This catches bitrot that mtime+size would miss. The sha256
double-read (validation pass + dar archival) is expensive for large units but
acceptable for v1; dar's `--hash sha256` could serve double duty as a future
optimization.

### 2.14 Exclusions

Global in config.toml + per-unit in dotfile. Merged (per-unit adds to global).
Mapped to dar `-X` (patterns) and `-P` (paths).

### 2.15 Symlinks

dar `-D`: archive symlinks as symlinks. Don't follow. Safe default. Following
symlinks could archive unexpected data from outside the unit.

### 2.16 Encryption

`rage` crate (pin to 0.11.x — pre-1.0 API unstable). Multi-recipient:
tenant key(s) + operator key(s). Old keys never deleted, only marked inactive.
`tapectl restore` tries all known keys.

Key onboarding: operator generates all keypairs, distributes private keys
to tenants. `tapectl key export ALIAS [--qr]` for paper backup.

### 2.17 Bin Packing

Best-fit-decreasing, plan-then-approve. Tenant-agnostic packing.
`--policy-aware` prioritizes slices resolving audit violations.
`--copies N` with location assignment ensures copies don't share volumes.

**Volume plan reports staging needs:** When `tapectl volume plan` identifies
snapshots that need re-staging (staged slices cleaned), it explicitly reports
which snapshots need staging before the write can proceed.

### 2.18 Verification

Per-session, per-slice. Types: `full` (read+decrypt+checksum), `quick`
(read+checksum). Verifies against stage_slices for the specific write's stage_set.

### 2.19 Archive Sets and Policy

Named policy templates, defined in DB via CLI. `config.toml` as optional
import source via `tapectl archive-set sync`.

**Policy resolution:** Unit dotfile `[policy]` > Archive set > System `[defaults]`.

### 2.20 Policy Audit

`tapectl audit` compares reality against policy. Advisory, never blocking.
Checks: copy count, location presence, verification age, encryption compliance,
dirty status, **compaction candidates**. `--action-plan` shows exact commands.
`--format json` for scripting. Exit codes: 0=clean, 1=warnings, 2=violations.

### 2.21 Volume Read-Slices (tape-to-staging for copy)

```bash
tapectl volume read-slices --from L6-0012 --unit "tv/bb/s01"
# swap tape
tapectl volume write L6-0025
```

Reads encrypted slices from source volume into staging. No decryption. The
normal `volume write` pipeline then writes them to the destination tape with
the full self-describing 10-file layout (ID thunk, guide, envelopes, etc.).
Creates new write record referencing the SAME stage_set. Checksums identical.

**Two-step workflow:** `read-slices` stages data → swap tape → `volume write`
writes with full metadata. This ensures the destination tape is always
self-describing and recoverable without the database.

**Design rationale:** When data is tape-only and a tape approaches end of life,
you can't re-run dar because the source is gone. Read-slices copies encrypted
bytes directly into staging, and volume write creates a new write referencing
the same stage_set. Checksums remain identical and verification still works.

### 2.22 Mark-Tape-Only Enforcement

Enforced: `min_copies_for_tape_only` (default 2), `min_locations_for_tape_only`
(default 2). `--force` overrides. If dirty, shows specific changes, requires `--force`.

### 2.23 Retirement Impact

`tapectl volume retire LABEL` shows: affected units, dropped copy counts,
whether any data drops to zero copies, suggests remediation commands.

### 2.24 Signal Handling

SIGINT sets atomic flag. Write loop checks between slices. On interrupt:
stop writing, mark write `interrupted`, exit cleanly. write_positions only
created after confirmed written. Resume on next `volume write`.

### 2.25 Audit Trail

Every state change to every entity logged in `events` table. Entity type,
ID, label, action, old/new values, JSON details, tenant reference.

### 2.26 Filesystem Metadata

dar: `-am` (xattrs), `--acl` (ACLs), `--fsa-scope linux_extX` (fs attributes).
Stores uid/gid + username/groupname. Warns on non-root restore.

### 2.27 Receipts

Per-write receipt in `~/.tapectl/receipts/{date}_{write_id}.txt`.

### 2.28 Labels

`L6-NNNN` — generation prefix + zero-padded sequence. Physical = DB.

### 2.29 LTO Drive Access

**Hybrid approach:** Data path uses kernel st driver (`/dev/nst0`) with
`nix` crate for ioctl. Diagnostics shell out to `sg_logs`/`sg_read_attr`.

- Always non-rewind device (`/dev/nst0`)
- Variable block mode (`MTSETBLK 0`) on every open
- `MTWEOFI` between data slices (fast, no flush)
- `MTWEOF` for final files (synchronous flush)
- ENOSPC detection from `write()` return
- Auto-filemark on close (crash safety net)
- `mt-st` optional for manual debugging, not called by tapectl

**Why not full SG_IO:** The st driver provides automatic position tracking,
crash-safe filemark-on-close, ENOSPC translation, and mt-st compatibility.
Full SG requires ~2000 lines of SCSI protocol code for the same functionality.
The SG interface remains available as an escape hatch via the backend trait if
a future requirement (tape library changer, hardware encryption) demands it.

---

## 3. Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                          tapectl CLI                                 │
│     Global: --json, --dry-run, --verbose, --yes, --config PATH       │
├───────┬──────┬───────┬────────┬────────┬───────┬───────┬───────────┤
│tenant │ unit │ snap/ │ volume │catalog │report │ audit │  restore  │
│       │      │ stage │cartrdg │       │       │       │           │
├───────┴──────┴───────┴────────┴────────┴───────┴───────┴───────────┤
│                         Core Library                                 │
│ ┌──────┐ ┌─────┐ ┌────────┐ ┌────┐ ┌──────┐ ┌───────┐ ┌────────┐  │
│ │ dar  │ │tape │ │manifest│ │rage│ │events│ │policy │ │ SQLite │  │
│ │wrap  │ │ioctl│ │engine  │ │    │ │logger│ │engine │ │        │  │
│ └──────┘ └─────┘ └────────┘ └────┘ └──────┘ └───────┘ └────────┘  │
│ ┌──────────┐ ┌──────────────────────────────────────────────────┐   │
│ │bin packer│ │ backend registry                                 │   │
│ └──────────┘ │  ┌─────┐  ┌────────────────────────┐            │   │
│              │  │ LTO │  │ export (directory dump) │            │   │
│              │  └─────┘  └────────────────────────┘            │   │
│              │  ┌─────────────────────────────────────┐        │   │
│              │  │ S3 (trait impl deferred, stub only)  │        │   │
│              │  └─────────────────────────────────────┘        │   │
│              └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

**Backend trait is synchronous.** The S3 backend (when implemented) wraps an
internal tokio runtime at the S3 boundary.

```rust
trait VolumeBackend {
    fn initialize(&self, volume: &Volume, config: &BackendConfig) -> Result<()>;
    fn identify(&self, config: &BackendConfig) -> Result<VolumeIdentity>;
    fn write_slice(&self, volume: &Volume, slice_path: &Path,
                   position_hint: usize) -> Result<SlicePosition>;
    fn read_slice(&self, volume: &Volume, position: &SlicePosition,
                  dest: &Path) -> Result<()>;
    fn write_metadata_file(&self, volume: &Volume, data: &[u8],
                           file_type: MetadataFileType) -> Result<SlicePosition>;
    fn read_metadata_file(&self, volume: &Volume, position: &SlicePosition,
                          dest: &Path) -> Result<()>;
    fn remaining_capacity(&self, volume: &Volume) -> Result<u64>;
    fn query_mam(&self, volume: &Volume) -> Result<Option<MamData>>;
    fn collect_health(&self, volume: &Volume) -> Result<Option<HealthData>>;
    fn verify_slice(&self, volume: &Volume,
                    position: &SlicePosition) -> Result<VerifyResult>;
    fn is_interactive(&self) -> bool;
    fn supports_append(&self) -> bool;
}
```

### External Dependencies

| Tool | Package (Debian) | Required | Purpose |
|------|-----------------|----------|---------|
| `dar` | `dar` (≥2.6, 2.7+ rec.) | Yes | Archive creation/extraction |
| `sg3-utils` | `sg3-utils` | Yes | Drive health (sg_logs), MAM query (sg_read_attr) |
| `lsscsi` | `lsscsi` | Optional | Device discovery |
| `mt-st` | `mt-st` | Optional | Manual debugging/recovery |
| `growisofs` | `growisofs` | Optional | BD burning (manual) |

`age` is NOT external — uses `rage` crate. `dd` is NOT needed — tapectl
writes directly to tape fd.

### File Layout

```
~/.tapectl/
├── tapectl.db
├── catalogs/{uuid_prefix}/{uuid}_v{N}.1.dar
├── keys/{tenant}-{alias}.age.pub
├── keys/{tenant}-{alias}.age.key
├── receipts/{date}_{write_id}.txt
├── config.toml
└── logs/
```

---

## 4. SQLite Schema

### Versioning

`meta` table with `schema_version`. Forward-only numbered migrations.
Prompts for DB backup before applying.

### Recovery

On startup: WAL mode, detect orphaned `in_progress`/`interrupted` sessions →
mark `aborted`, warn. `PRAGMA integrity_check` only on `tapectl db fsck`.

### Full Schema

```sql
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- TENANTS
CREATE TABLE tenants (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT,
    is_operator INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL DEFAULT 'active'
                CHECK(status IN ('active','inactive','deleted')),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    notes       TEXT
);

-- ENCRYPTION KEYS (per-tenant)
CREATE TABLE encryption_keys (
    id          INTEGER PRIMARY KEY,
    tenant_id   INTEGER NOT NULL REFERENCES tenants(id),
    alias       TEXT NOT NULL UNIQUE,
    fingerprint TEXT NOT NULL UNIQUE,
    public_key  TEXT NOT NULL,
    key_type    TEXT NOT NULL DEFAULT 'primary'
                CHECK(key_type IN ('primary','backup')),
    is_active   INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    description TEXT
);

-- ARCHIVE SETS (policy templates)
CREATE TABLE archive_sets (
    id                       INTEGER PRIMARY KEY,
    name                     TEXT NOT NULL UNIQUE,
    description              TEXT,
    min_copies               INTEGER,
    required_locations       TEXT,           -- JSON array of location names
    encrypt                  INTEGER,
    compression              TEXT,
    checksum_mode            TEXT,
    slice_size               INTEGER,
    verify_interval_days     INTEGER,
    preserve_xattrs          INTEGER,
    preserve_acls            INTEGER,
    preserve_fsa             INTEGER,
    dirty_on_metadata_change INTEGER,
    created_at               TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at               TEXT NOT NULL DEFAULT (datetime('now'))
);

-- UNITS
CREATE TABLE units (
    id              INTEGER PRIMARY KEY,
    uuid            TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL UNIQUE,
    tenant_id       INTEGER NOT NULL REFERENCES tenants(id),
    archive_set_id  INTEGER REFERENCES archive_sets(id),
    current_path    TEXT,
    checksum_mode   TEXT NOT NULL DEFAULT 'mtime_size'
                    CHECK(checksum_mode IN ('mtime_size','sha256','sha256_on_archive')),
    encrypt         INTEGER NOT NULL DEFAULT 1,
    status          TEXT NOT NULL DEFAULT 'active'
                    CHECK(status IN ('active','tape_only','missing','retired')),
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    last_scanned    TEXT,
    notes           TEXT
);

CREATE TABLE tags (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE unit_tags (
    unit_id INTEGER NOT NULL REFERENCES units(id),
    tag_id  INTEGER NOT NULL REFERENCES tags(id),
    PRIMARY KEY (unit_id, tag_id)
);

CREATE TABLE unit_path_history (
    id          INTEGER PRIMARY KEY,
    unit_id     INTEGER NOT NULL REFERENCES units(id),
    path        TEXT NOT NULL,
    observed_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- SNAPSHOTS (content identity)
CREATE TABLE snapshots (
    id               INTEGER PRIMARY KEY,
    unit_id          INTEGER NOT NULL REFERENCES units(id),
    version          INTEGER NOT NULL,
    snapshot_type    TEXT NOT NULL DEFAULT 'full'
                     CHECK(snapshot_type IN ('full','differential','incremental')),
    base_snapshot_id INTEGER REFERENCES snapshots(id),
    status           TEXT NOT NULL DEFAULT 'created'
                     CHECK(status IN ('created','staged','current','superseded',
                                      'reclaimable','purged','failed')),
    source_path      TEXT NOT NULL,
    total_size       INTEGER,
    file_count       INTEGER,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    superseded_at    TEXT,
    notes            TEXT,
    UNIQUE(unit_id, version)
);

-- Snapshot status lifecycle:
--   created     → snapshot create recorded manifest; not yet staged
--   staged      → at least one stage_set exists (dar has run)
--   current     → at least one write exists on a volume
--   superseded  → a newer snapshot is now current
--   reclaimable → operator explicitly marked for cleanup (preconditions enforced)
--   purged      → volume copies removed; files + manifests deleted
--   failed      → staging failed; needs cleanup

-- MANIFESTS (change detection baseline)
CREATE TABLE manifests (
    id          INTEGER PRIMARY KEY,
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE manifest_entries (
    id           INTEGER PRIMARY KEY,
    manifest_id  INTEGER NOT NULL REFERENCES manifests(id),
    path         TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    mtime        TEXT NOT NULL,
    sha256       TEXT,
    is_directory INTEGER NOT NULL DEFAULT 0,
    mode         INTEGER,
    uid          INTEGER,
    gid          INTEGER,
    username     TEXT,
    groupname    TEXT,
    has_xattrs   INTEGER DEFAULT 0,
    has_acls     INTEGER DEFAULT 0
);

-- FILE CATALOG (browse/search — sha256 backfilled on first stage)
CREATE TABLE files (
    id              INTEGER PRIMARY KEY,
    snapshot_id     INTEGER NOT NULL REFERENCES snapshots(id),
    path            TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    sha256          TEXT,
    modified_at     TEXT,
    is_directory    INTEGER NOT NULL DEFAULT 0,
    UNIQUE(snapshot_id, path)
);

-- STAGE SETS (each dar+encrypt execution)
CREATE TABLE stage_sets (
    id                   INTEGER PRIMARY KEY,
    snapshot_id          INTEGER NOT NULL REFERENCES snapshots(id),
    status               TEXT NOT NULL DEFAULT 'staging'
                         CHECK(status IN ('staging','staged','failed','cleaned')),
    dar_version          TEXT,
    dar_command          TEXT,
    catalog_path         TEXT,
    slice_size           INTEGER NOT NULL,
    compression          TEXT,
    encrypted            INTEGER NOT NULL DEFAULT 1,
    key_fingerprints     TEXT,
    num_slices           INTEGER,
    total_dar_size       INTEGER,
    total_encrypted_size INTEGER,
    source_validated_at  TEXT,
    staged_at            TEXT,
    cleaned_at           TEXT,
    created_at           TEXT NOT NULL DEFAULT (datetime('now')),
    notes                TEXT
);

-- STAGE SLICES (encrypted artifacts, shared across writes)
CREATE TABLE stage_slices (
    id               INTEGER PRIMARY KEY,
    stage_set_id     INTEGER NOT NULL REFERENCES stage_sets(id),
    slice_number     INTEGER NOT NULL,
    size_bytes       INTEGER NOT NULL,
    encrypted_bytes  INTEGER NOT NULL,
    sha256_plain     TEXT NOT NULL,
    sha256_encrypted TEXT NOT NULL,
    staging_path     TEXT,
    UNIQUE(stage_set_id, slice_number)
);

-- LOCATIONS
CREATE TABLE locations (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- PHYSICAL CARTRIDGES
CREATE TABLE cartridges (
    id                    INTEGER PRIMARY KEY,
    barcode               TEXT NOT NULL UNIQUE,
    media_type            TEXT NOT NULL,         -- "LTO-6", "LTO-7", etc.
    manufacturer          TEXT,                  -- from MAM
    serial_number         TEXT,                  -- from MAM
    tape_length_meters    INTEGER,               -- from MAM
    nominal_capacity      INTEGER NOT NULL,      -- marketed capacity in bytes
    status                TEXT NOT NULL DEFAULT 'available'
                          CHECK(status IN ('available','in_use','pending_erase',
                                           'retired_permanent','offsite')),
    total_load_count      INTEGER DEFAULT 0,     -- cumulative from MAM
    total_bytes_written   INTEGER DEFAULT 0,     -- lifetime cumulative
    total_bytes_read      INTEGER DEFAULT 0,     -- lifetime cumulative
    first_use             TEXT,
    last_use              TEXT,
    error_history         TEXT,                  -- JSON: [{date, type, count}]
    location_id           INTEGER REFERENCES locations(id),
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    notes                 TEXT
);

-- CARTRIDGE ↔ VOLUME RELATIONSHIP
CREATE TABLE cartridge_volumes (
    id            INTEGER PRIMARY KEY,
    cartridge_id  INTEGER NOT NULL REFERENCES cartridges(id),
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    mounted_at    TEXT NOT NULL DEFAULT (datetime('now')),
    unmounted_at  TEXT,
    UNIQUE(volume_id)   -- each volume lives on exactly one cartridge
);

-- VOLUMES (logical write sessions)
CREATE TABLE volumes (
    id                    INTEGER PRIMARY KEY,
    label                 TEXT NOT NULL UNIQUE,
    backend_type          TEXT NOT NULL,
    backend_name          TEXT NOT NULL,
    media_type            TEXT,
    capacity_bytes        INTEGER NOT NULL,
    mam_capacity_bytes    INTEGER,
    mam_remaining_at_start INTEGER,
    bytes_written         INTEGER NOT NULL DEFAULT 0,
    num_data_files        INTEGER NOT NULL DEFAULT 0,
    has_manifest          INTEGER NOT NULL DEFAULT 0,
    location_id           INTEGER REFERENCES locations(id),
    status                TEXT NOT NULL DEFAULT 'blank'
                          CHECK(status IN ('blank','initialized','active','full',
                                           'retired','missing','erased')),
    storage_class         TEXT,
    first_write           TEXT,
    last_write            TEXT,
    notes                 TEXT,
    created_at            TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE volume_movements (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    from_location INTEGER REFERENCES locations(id),
    to_location   INTEGER NOT NULL REFERENCES locations(id),
    moved_at      TEXT NOT NULL DEFAULT (datetime('now')),
    notes         TEXT
);

-- WRITES (each volume copy from a stage set)
CREATE TABLE writes (
    id                  INTEGER PRIMARY KEY,
    stage_set_id        INTEGER NOT NULL REFERENCES stage_sets(id),
    snapshot_id         INTEGER NOT NULL REFERENCES snapshots(id),
    volume_id           INTEGER NOT NULL REFERENCES volumes(id),
    status              TEXT NOT NULL DEFAULT 'planned'
                        CHECK(status IN ('planned','in_progress','completed',
                                         'failed','aborted','interrupted')),
    write_verified      INTEGER NOT NULL DEFAULT 0,
    eot_recovery        TEXT CHECK(eot_recovery IN (NULL, 'normal',
                                   'overwrite_incomplete', 'sacrifice_last_slice')),
    sacrificed_slice_id INTEGER REFERENCES stage_slices(id),
    started_at          TEXT,
    completed_at        TEXT,
    notes               TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(stage_set_id, volume_id)
);

-- WRITE POSITIONS
CREATE TABLE write_positions (
    id               INTEGER PRIMARY KEY,
    write_id         INTEGER NOT NULL REFERENCES writes(id),
    stage_slice_id   INTEGER NOT NULL REFERENCES stage_slices(id),
    position         TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK(status IN ('pending','writing','written','verified',
                                      'failed','sacrificed')),
    written_at       TEXT,
    sha256_on_volume TEXT,
    UNIQUE(write_id, stage_slice_id)
);

-- VERIFICATION
CREATE TABLE verification_sessions (
    id              INTEGER PRIMARY KEY,
    volume_id       INTEGER NOT NULL REFERENCES volumes(id),
    started_at      TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at    TEXT,
    verify_type     TEXT NOT NULL DEFAULT 'full'
                    CHECK(verify_type IN ('full','quick')),
    outcome         TEXT NOT NULL DEFAULT 'in_progress'
                    CHECK(outcome IN ('in_progress','passed','failed',
                                      'partial','aborted')),
    slices_checked  INTEGER NOT NULL DEFAULT 0,
    slices_passed   INTEGER NOT NULL DEFAULT 0,
    slices_failed   INTEGER NOT NULL DEFAULT 0,
    slices_skipped  INTEGER NOT NULL DEFAULT 0,
    notes           TEXT
);

CREATE TABLE verification_results (
    id                      INTEGER PRIMARY KEY,
    session_id              INTEGER NOT NULL REFERENCES verification_sessions(id),
    write_position_id       INTEGER NOT NULL REFERENCES write_positions(id),
    stage_slice_id          INTEGER NOT NULL REFERENCES stage_slices(id),
    result                  TEXT NOT NULL
                            CHECK(result IN ('passed','failed_checksum','failed_read',
                                             'failed_decrypt','skipped')),
    expected_sha256         TEXT,
    actual_sha256           TEXT,
    read_errors_corrected   INTEGER DEFAULT 0,
    read_errors_uncorrected INTEGER DEFAULT 0,
    verified_at             TEXT NOT NULL DEFAULT (datetime('now')),
    notes                   TEXT
);

-- HEALTH
CREATE TABLE health_logs (
    id                  INTEGER PRIMARY KEY,
    volume_id           INTEGER NOT NULL REFERENCES volumes(id),
    session_id          INTEGER REFERENCES verification_sessions(id),
    logged_at           TEXT NOT NULL DEFAULT (datetime('now')),
    operation           TEXT NOT NULL
                        CHECK(operation IN ('write','read','verify','clean')),
    total_bytes         INTEGER,
    total_uncorrected   INTEGER,
    total_corrected     INTEGER,
    total_retries       INTEGER,
    total_rewritten     INTEGER,
    raw_log             TEXT
);

-- AUDIT TRAIL
CREATE TABLE events (
    id           INTEGER PRIMARY KEY,
    timestamp    TEXT NOT NULL DEFAULT (datetime('now')),
    entity_type  TEXT NOT NULL,
    entity_id    INTEGER NOT NULL,
    entity_label TEXT,
    action       TEXT NOT NULL,
    field        TEXT,
    old_value    TEXT,
    new_value    TEXT,
    details      TEXT,
    tenant_id    INTEGER REFERENCES tenants(id)
);

-- INDEXES
CREATE INDEX idx_units_uuid ON units(uuid);
CREATE INDEX idx_units_status ON units(status);
CREATE INDEX idx_units_tenant ON units(tenant_id);
CREATE INDEX idx_units_archive_set ON units(archive_set_id);
CREATE INDEX idx_encryption_keys_tenant ON encryption_keys(tenant_id);
CREATE INDEX idx_snapshots_unit ON snapshots(unit_id);
CREATE INDEX idx_snapshots_status ON snapshots(status);
CREATE INDEX idx_files_snapshot ON files(snapshot_id);
CREATE INDEX idx_files_path ON files(path);
CREATE INDEX idx_manifest_entries_manifest ON manifest_entries(manifest_id);
CREATE INDEX idx_stage_sets_snapshot ON stage_sets(snapshot_id);
CREATE INDEX idx_stage_sets_status ON stage_sets(status);
CREATE INDEX idx_stage_slices_stage_set ON stage_slices(stage_set_id);
CREATE INDEX idx_writes_stage_set ON writes(stage_set_id);
CREATE INDEX idx_writes_snapshot ON writes(snapshot_id);
CREATE INDEX idx_writes_volume ON writes(volume_id);
CREATE INDEX idx_writes_status ON writes(status);
CREATE INDEX idx_write_positions_write ON write_positions(write_id);
CREATE INDEX idx_write_positions_stage_slice ON write_positions(stage_slice_id);
CREATE INDEX idx_volumes_location ON volumes(location_id);
CREATE INDEX idx_volumes_status ON volumes(status);
CREATE INDEX idx_cartridges_status ON cartridges(status);
CREATE INDEX idx_cartridges_barcode ON cartridges(barcode);
CREATE INDEX idx_cartridge_volumes_cartridge ON cartridge_volumes(cartridge_id);
CREATE INDEX idx_cartridge_volumes_volume ON cartridge_volumes(volume_id);
CREATE INDEX idx_verification_sessions_volume ON verification_sessions(volume_id);
CREATE INDEX idx_verification_results_session ON verification_results(session_id);
CREATE INDEX idx_health_volume ON health_logs(volume_id);
CREATE INDEX idx_unit_path_history_unit ON unit_path_history(unit_id);
CREATE INDEX idx_events_entity ON events(entity_type, entity_id);
CREATE INDEX idx_events_timestamp ON events(timestamp);
CREATE INDEX idx_events_tenant ON events(tenant_id);
```

### Schema Changes from v3.0

| Change | Rationale |
|--------|-----------|
| New `cartridges` table | Physical tape media tracking with MAM data, wear, lifecycle |
| New `cartridge_volumes` table | Cartridge ↔ volume join table with time range |
| `volumes.status` gains `'erased'` | Tracks volumes whose cartridge has been physically erased |
| `cartridges.status` enum | `available`, `in_use`, `pending_erase`, `retired_permanent`, `offsite` |
| Cartridge indexes added | barcode, status, cartridge_volumes lookups |

### Purge Behavior

Snapshot purge keeps `stage_set` + `stage_slice` rows (writes reference them).
Purge deletes: `files`, `manifests`, `manifest_entries`. Marks stage_sets as
`cleaned`. Snapshot row stays as audit record.

### Key Queries

**"Where are all copies of this unit?"**
```sql
SELECT v.label, v.media_type, l.name as location,
       w.completed_at, ss.num_slices
FROM snapshots s
JOIN stage_sets ss ON ss.snapshot_id = s.id
JOIN writes w ON w.stage_set_id = ss.id
JOIN volumes v ON v.id = w.volume_id
LEFT JOIN locations l ON l.id = v.location_id
WHERE s.unit_id = ? AND s.status = 'current' AND w.status = 'completed';
```

**"Were these two tapes written from the same staged data?"**
```sql
SELECT w1.volume_id, w2.volume_id, w1.stage_set_id = w2.stage_set_id as identical
FROM writes w1
JOIN writes w2 ON w1.stage_set_id = w2.stage_set_id
WHERE w1.id != w2.id;
```

**"What's staged and ready to write?"**
```sql
SELECT s.id as snapshot_id, u.name, ss.id as stage_set_id,
       ss.num_slices, ss.total_encrypted_size, ss.status
FROM stage_sets ss
JOIN snapshots s ON s.id = ss.snapshot_id
JOIN units u ON u.id = s.unit_id
WHERE ss.status = 'staged';
```

**"Can I mark this unit tape-only?"**
```sql
SELECT COUNT(DISTINCT w.id) as copy_count,
       COUNT(DISTINCT l.id) as location_count
FROM snapshots s
JOIN stage_sets ss ON ss.snapshot_id = s.id
JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
JOIN volumes v ON v.id = w.volume_id
LEFT JOIN locations l ON l.id = v.location_id
WHERE s.unit_id = ? AND s.status = 'current';
```

**"What was the receipt for a specific write?"**
```sql
SELECT w.id, w.completed_at, v.label as volume,
       ss.dar_version, ss.dar_command,
       sl.slice_number, sl.sha256_plain, sl.sha256_encrypted,
       sl.size_bytes, sl.encrypted_bytes,
       wp.position
FROM writes w
JOIN stage_sets ss ON ss.id = w.stage_set_id
JOIN stage_slices sl ON sl.stage_set_id = ss.id
JOIN write_positions wp ON wp.write_id = w.id AND wp.stage_slice_id = sl.id
JOIN volumes v ON v.id = w.volume_id
WHERE w.id = ?
ORDER BY sl.slice_number;
```

**"What stage data can I clean?"**
```sql
SELECT ss.id, u.name, ss.total_encrypted_size, ss.staged_at,
       COUNT(w.id) as write_count
FROM stage_sets ss
JOIN snapshots s ON s.id = ss.snapshot_id
JOIN units u ON u.id = s.unit_id
LEFT JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
WHERE ss.status = 'staged'
GROUP BY ss.id;
```

**"Which volumes are compaction candidates?"**
```sql
SELECT v.label, v.bytes_written,
       SUM(CASE WHEN s.status NOT IN ('reclaimable','purged') THEN sl.encrypted_bytes ELSE 0 END) as live_bytes,
       CAST(SUM(CASE WHEN s.status NOT IN ('reclaimable','purged') THEN sl.encrypted_bytes ELSE 0 END) AS REAL) / v.bytes_written as utilization
FROM volumes v
JOIN writes w ON w.volume_id = v.id AND w.status = 'completed'
JOIN stage_sets ss ON ss.id = w.stage_set_id
JOIN snapshots s ON s.id = ss.snapshot_id
JOIN stage_slices sl ON sl.stage_set_id = ss.id
WHERE v.status IN ('active','full')
GROUP BY v.id
HAVING utilization < 0.50;
```

---

## 5. CLI Interface

### Global Flags

```
tapectl [--json] [--dry-run] [--verbose] [--yes] [--config PATH] <SUBCOMMAND>
```

Exit codes: 0 = success, 1 = warnings, 2 = errors/violations.

### Subcommands

```
init              Initialize everything (DB, config, operator tenant, keys)
tenant            Manage tenants (add, list, info, reassign, delete)
key               Manage encryption keys (generate, list, export, import, rotate)
unit              Manage units (init, discover, status, tag, rename, integrity, mark-tape-only)
snapshot          Manage snapshots (create, list, diff, delete, mark-reclaimable, purge)
stage             Stage snapshots (create, list, info)
staging           Manage staging area (status, clean)
volume            Manage volumes (init, identify, plan, write, append, verify,
                  move, retire, read-slices, calibrate,
                  compact-read, compact-write, compact-finish, compact)
cartridge         Manage cartridges (register, list, info, mark-erased)
archive-set       Manage archive set policies (create, edit, list, info, sync)
audit             Policy compliance audit (full, action-plan, json)
catalog           Browse/search file catalog (ls, search, locate, stats)
location          Manage locations (add, list, info, rename)
report            Reports (summary, fire-risk, copies, tape-only, dirty, pending,
                  verify-*, health, capacity, age, events, compaction-candidates)
restore           Extract from volumes (unit, file, dry-run, raw-volume)
export            Dump encrypted slices to directory
quick-archive     Convenience: create + stage + write in one interactive flow
db                Database ops (backup, export, import, fsck, stats)
config            Configuration (show, set, add, remove, check)
completions       Shell completions (bash, zsh, fish)
```

### Cartridge Commands (v4.0)

```bash
# Register a physical cartridge (auto-reads MAM if tape loaded)
tapectl cartridge register --barcode L6-0001 --media-type LTO-6

# List cartridges with status filter
tapectl cartridge list [--status available|in_use|pending_erase|retired_permanent|offsite]

# Cartridge details and volume history
tapectl cartridge info L6-0001

# Confirm physical erase, make cartridge available for reuse
tapectl cartridge mark-erased L6-0001
```

### Compaction Commands (v4.0)

```bash
# Read live slices from underutilized volume to staging
tapectl volume compact-read L6-0012

# Write compaction slices to destination via bin packing
tapectl volume compact-write

# Retire source volume, free cartridge
tapectl volume compact-finish L6-0012

# Interactive orchestration wrapper (walks through all three steps)
tapectl volume compact L6-0012

# Detailed per-volume utilization breakdown
tapectl report compaction-candidates
```

### Other Commands Added Since v3.0

```bash
# Query MAM and measure real capacity overhead
tapectl volume calibrate --device /dev/nst0

# Quick identification from tape (reads ID + planning header)
tapectl volume identify

# Mark a superseded snapshot as reclaimable (enforces preconditions)
tapectl snapshot mark-reclaimable "tv/bb/s01" --version 1 [--force]
```

---

## 6. dar Integration

### Archive Creation Flags

```bash
dar -c /staging/{uuid}_v{N} \
    -R /unit/path \
    -s {slice_size} \
    [-z {compression}] \
    -an \
    --hash sha256 \
    -am --acl --fsa-scope linux_extX \
    -D \
    -X "*.nfo" -X "Thumbs.db" \
    -P "extras/samples"
```

Dotfile included (not excluded). dar version + full command line stored in stage_sets.

### Catalog Management

Extracted on first stage via `dar -C`. Stored in `~/.tapectl/catalogs/`.
**Per-snapshot, not per-unit** — new snapshot's first stage creates new catalog.
Per-write catalog embedded in volume envelopes for self-contained restore.

**Implementation note:** The catalog should be per-snapshot. The code must
create a NEW catalog for each new snapshot's first stage, not reuse the old
snapshot's catalog. When a volume has writes from multiple stage_sets of the
same snapshot (re-staged and wrote to same tape in different sessions), each
write's envelope includes the dar catalog from that write's specific stage_set.

---

## 7. Configuration

```toml
[dar]
binary = "/opt/dar/bin/dar"

[[backends.lto]]
name = "lto-primary"
device_tape = "/dev/nst0"
device_sg = "/dev/sg1"
media_type = "LTO-6"
nominal_capacity = "2500G"
usable_capacity_factor = 0.92
manifest_reserve = "200M"
enospc_buffer = "50M"
block_size = "1M"
hardware_compression = false

# S3 backend configuration retained for future use
# [[backends.s3]]
# name = "backblaze-archive"
# ...

[[archive_sets]]
name = "critical-media"
min_copies = 3
required_locations = ["home-rack", "parents-house"]
encrypt = true
compression = "none"
checksum_mode = "sha256_on_archive"
verify_interval_days = 180
slice_size = "2400G"

[[archive_sets]]
name = "bulk-media"
min_copies = 2
required_locations = ["home-rack", "parents-house"]
encrypt = true
compression = "none"
checksum_mode = "mtime_size"
verify_interval_days = 365

[defaults]
slice_size = "2400G"
compression = "none"
hash = "sha256"
checksum_mode = "mtime_size"
encrypt = true
preserve_xattrs = true
preserve_acls = true
preserve_fsa = true
dirty_on_metadata_change = false
global_excludes = ["*.nfo", "Thumbs.db", ".DS_Store", "*.tmp"]
large_file_warn_threshold = "100G"
min_copies_for_tape_only = 2
min_locations_for_tape_only = 2

[staging]
directory = "/mnt/staging"

[discovery]
watch_roots = ["/media/tv", "/media/movies", "/media/music"]

[packing]
strategy = "best_fit_decreasing"
fill_threshold = 0.95
min_free_for_append = "50G"

[compaction]
utilization_threshold = 0.50
tape_only_safety_multiplier = 2

[labels]
format = "L{gen}-{seq:04}"

[logging]
level = "info"
format = "json"
```

### Config Changes from v3.0

| Added | Purpose |
|-------|---------|
| `[compaction]` section | Compaction threshold and tape-only safety multiplier |

---

## 8. Volume File Format Specifications

### 8.1 File 0 — ID Thunk

Short plaintext identity block. Points reader to File 1 for full instructions.
Hybrid format: brief prose header + TOML metadata.

Contains: magic, label, layout_version, tapectl_version, backend, media_type,
MAM capacity, creation timestamp, and a `[layout]` section mapping file numbers
to each zone (guide, script, planning header, data range, mini-index, envelopes).

**Zero content metadata.** No unit names, tenant names, filenames, or checksums.

**Maximum size:** 8 KB (fits in a single tape block at any block size).

```
================================================================
                     TAPECTL ARCHIVAL VOLUME
================================================================

Label:   {label}
Media:   {media_type}
Created: {created_date}

This tape contains encrypted archival data managed by tapectl,
an open-source archival storage tool.

>>> COMPLETE INSTRUCTIONS ARE IN THE NEXT FILE ON THIS TAPE. <<<

To read the next file (the full recovery guide):

    mt -f /dev/nst0 fsf 1
    dd if=/dev/nst0 bs=64k > GUIDE.md
    less GUIDE.md

If you just read this file and the tape is already positioned
past it, read the next file directly:

    dd if=/dev/nst0 bs=64k > GUIDE.md

The guide explains everything: what tools you need, how to find
your encryption key, and how to recover your data step by step.
It is written so that an AI assistant can follow it to help you.

================================================================
              MACHINE-READABLE METADATA (TOML)
================================================================

[volume]
magic = "tapectl-volume-v1"
label = "{label}"
layout_version = 1
tapectl_version = "{version}"
backend = "lto"
media_type = "{media_type}"
nominal_capacity_bytes = {nominal_capacity}
mam_capacity_bytes = {mam_capacity}
created_at = "{iso8601_timestamp}"

[layout]
system_guide = 1
restore_script = 2
planning_header = 3
data_start = {data_start}
data_end = {data_end}
mini_index = {mini_index_pos}
first_envelope = {first_envelope_pos}
num_envelopes = {num_envelopes}
operator_envelope = {op_envelope_pos}
operator_envelope_backup = {op_backup_pos}
total_files = {total_files}

[media]
cartridge_manufacturer = "{mam_manufacturer}"
cartridge_serial = "{mam_serial}"
tape_length_meters = {mam_length}
load_count_at_write = {mam_loads}
```

**Field definitions:**

| Field | Type | Description |
|-------|------|-------------|
| `magic` | string | Always `"tapectl-volume-v1"`. Identifies this as a tapectl tape. |
| `label` | string | Volume label matching physical label (e.g., `"L6-0015"`). |
| `layout_version` | integer | Format version. Determines parsing rules for all other files. |
| `tapectl_version` | string | SemVer of tapectl that wrote this tape. Informational only. |
| `backend` | string | Backend type: `"lto"`, `"export"`. |
| `media_type` | string | Media specification: `"LTO-6"`, `"LTO-7"`, etc. |
| `nominal_capacity_bytes` | integer | Marketed capacity in bytes. |
| `mam_capacity_bytes` | integer | Actual usable capacity from MAM chip. 0 if unavailable. |
| `created_at` | string | ISO 8601 timestamp of volume initialization. |
| `layout.*` | integer | File number (0-indexed) for each zone. |
| `media.*` | string/int | From MAM chip. Empty string if unavailable. |

### 8.2 File 1 — System Guide (Markdown)

Comprehensive plaintext document designed so that **an LLM running on a machine
with a tape drive can orchestrate full interactive multi-tape restoration**.

Contents:
1. System overview — what tapectl is, the multi-tenant model
2. Tape layout explanation — what each file zone contains
3. Tools required — mt, dd, age, dar with install instructions and URLs
4. Key management guide — what keys are, where to find them, what they look like
5. Single-tape recovery — step-by-step procedure
6. Multi-tape recovery — inventory all tapes, build cross-tape index, plan restore
7. dar archive details — slice reassembly, catalog usage, useful commands
8. Encryption format details — age format, multi-recipient, how trial-decryption works
9. Layout version history — enables forward compatibility
10. About tapectl — project URLs, license
11. Tape-specific information — this tape's file positions and counts

**Identical across all tapes** except for Section 11 (tape-specific data).
Written so it never becomes stale — describes the format, not current project state.

The complete system guide template is provided in Appendix A.

### 8.3 File 2 — RESTORE.sh

Plaintext bash script. Exhaustively self-documented in comments.
Implements the procedures described in the system guide:
- `--info`: read ID block + mini-index (no key needed)
- `--find-envelope --key KEY`: trial-decrypt each envelope
- `--restore --key KEY --manifest DIR --unit NAME --destination DIR`: full restore

Only tape-specific values substituted: label, layout version, file positions.
**Zero content metadata** in the script itself.

**Exit codes:** 0=success, 1=usage error, 2=tape error, 3=decryption error,
4=dar error, 5=verification error.

**Error handling:** Every `mt`, `dd`, `age`, and `dar` invocation checks return
code and prints a human-readable error message explaining what went wrong
and what to try next.

**Temporary files:** Created in `$TMPDIR/tapectl-restore-$$` (cleaned on exit
via trap). Never writes to tape.

### 8.4 File 3 — Planning Header

Age-encrypted to operator keys only. Written before data slices.
Contains the planned packing list: unit names, UUIDs, tenant names,
expected slice counts/sizes, MAM capacity data, estimated fill percentage.

Purpose: quick tape identification by operator without seeking to end-of-tape.
Labeled `status = "planned"` — not a receipt (receipt is in operator envelope).

### 8.5 Files 4..N — Encrypted Data Slices

Each file is one dar slice encrypted with age (multi-recipient: tenant + operator).
Slices for one unit are always contiguous on the same tape.

### 8.6 File N+1 — Mini-Index

Plaintext markdown with prose header explaining what the file is and how to use it,
followed by TOML metadata. Lists every file on the tape by position number,
byte size, and structural type.

**Contains NO:** filenames, tenant names, unit names, checksums, key fingerprints,
or any content/ownership metadata.

Type enum (exhaustive for layout_version=1): `id_thunk`, `system_guide`,
`restore_script`, `planning_header`, `data_slice`, `mini_index`,
`tenant_envelope`, `operator_envelope`.

### 8.7 Files N+2..N+K — Tenant Envelopes

Each is a tar archive encrypted with age to one tenant's key(s) + operator key(s).
Written in **shuffled random order** — no positional correlation to tenant identity.

Envelope contents:
```
MANIFEST.toml    — structured metadata: units, slices, positions, checksums
RECOVERY.md      — human-readable step-by-step restore guide (markdown)
catalogs/        — dar catalog files for selective restore
```

MANIFEST.toml contains: unit names, UUIDs, snapshot versions, stage_set IDs,
dar versions/commands, per-slice tape positions, per-slice SHA-256 checksums
(both plaintext and encrypted), complete file listings with checksums.

**MANIFEST.toml field notes:**

- `units` entries appear in the order their slices are written on tape.
- `units.slices` entries are ordered by `number` (ascending).
- `units.slices.tape_position` is the tape file number — use with
  `mt -f /dev/nst0 fsf {tape_position}` to seek to that slice.
- `units.files` may be absent if the operator configured
  `include_file_listing_in_envelope = false`. Tenants can still restore
  via dar catalog; they just lose the searchable file index.
- `sha256_plain` and `sha256_encrypted` enable verification: read the
  encrypted blob from tape, compute its SHA-256, compare against
  `sha256_encrypted`. Then decrypt and compare against `sha256_plain`.
- `dar_command` records the exact command used, enabling reproduction
  or debugging of archive creation.

**RECOVERY.md format specification:**

Markdown document with:

1. Header identifying tenant, tape, and date
2. Summary table: unit names, slice counts, approximate sizes
3. Tools required section (brief, references system guide for details)
4. Per-unit restore section with:
   - Literal `mt`/`dd` commands using exact tape positions
   - `age` decryption commands with key file placeholder
   - `dar` extraction commands
   - SHA-256 verification commands against MANIFEST.toml values
5. Troubleshooting section (common errors + fixes)

All shell commands use fenced code blocks (triple backtick + `bash`).
Every position, filename, and slice number is literal — no variables
or placeholders that require the reader to look up values elsewhere.

### 8.8 Files N+K+1, N+K+2 — Operator Envelopes

Two identical copies encrypted to operator key(s) only. Contains merged
manifest for ALL tenants — the god-view. Same structure as tenant envelopes
but unfiltered. Also includes a portable SQLite subset with all write/slice/
position data for this tape.

**catalog.db schema:** Contains copies of these tables, filtered to only
rows relevant to writes on this volume: `tenants`, `units`, `snapshots`,
`stage_sets`, `stage_slices`, `writes`, `write_positions`, `encryption_keys`
(public keys only — private keys are NEVER written to tape).

---

## 9. Rust Crate Structure

```
tapectl/
├── Cargo.toml
├── src/
│   ├── main.rs
│   ├── cli/
│   │   ├── mod.rs, tenant.rs, key.rs, unit.rs, snapshot.rs
│   │   ├── stage.rs, staging.rs, volume.rs, catalog.rs
│   │   ├── archive_set.rs, audit.rs, location.rs
│   │   ├── report.rs, restore.rs, export.rs
│   │   ├── db_cmd.rs, config.rs, integrity.rs
│   │   ├── clone.rs, cartridge.rs, compact.rs
│   ├── db/
│   │   ├── mod.rs, migrations/, schema.rs, models.rs
│   │   ├── queries.rs, fsck.rs, events.rs, backup.rs
│   ├── tenant/
│   │   └── mod.rs
│   ├── unit/
│   │   ├── mod.rs, dotfile.rs, discovery.rs
│   │   ├── manifest.rs, integrity.rs, nesting.rs
│   ├── dar/
│   │   ├── mod.rs, version.rs, create.rs
│   │   ├── catalog_xml.rs, restore.rs
│   ├── staging/
│   │   ├── mod.rs, validate.rs, clean.rs
│   ├── backend/
│   │   ├── mod.rs, trait_def.rs, registry.rs
│   │   ├── lto.rs, export.rs
│   │   └── s3_stub.rs
│   ├── volume/
│   │   ├── mod.rs, layout.rs
│   │   ├── id_block.rs, system_guide.rs, restore_script.rs
│   │   ├── planning_header.rs, mini_index.rs
│   │   ├── envelope.rs, manifest_toml.rs
│   │   ├── packing.rs, health.rs
│   │   ├── retirement.rs, clone.rs, calibrate.rs
│   │   └── compact.rs
│   ├── cartridge/
│   │   ├── mod.rs, mam.rs
│   ├── crypto/
│   │   ├── mod.rs, keys.rs, encrypt.rs
│   ├── policy/
│   │   ├── mod.rs, archive_set.rs, resolver.rs, audit.rs
│   │   └── compaction.rs
│   ├── verify/
│   │   ├── mod.rs, session.rs
│   ├── tape/
│   │   ├── mod.rs, ioctl.rs, mam.rs, health.rs
│   ├── signal.rs, config.rs, error.rs
```

### Dependencies

```toml
[dependencies]
clap = { version = "4.6", features = ["derive"] }
clap_complete = "4"
rusqlite = { version = "0.39", features = ["bundled", "serde_json", "backup"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
sha2 = "0.10"
uuid = { version = "1", features = ["v4", "v7", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "2"
anyhow = "1"
dialoguer = "0.11"
tabled = "0.15"
walkdir = "2"
bytesize = "1"
glob = "0.3"
age = { version = "0.11", features = ["cli-common"] }    # pin: pre-1.0
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }
quick-xml = { version = "0.39", features = ["serialize"] }
ctrlc = "3"
nix = { version = "0.29", features = ["ioctl", "fs"] }
indicatif = "0.17"
rand = "0.8"              # envelope shuffling
tar = "0.4"               # envelope packing
rusqlite_migration = "2.4"
```

**Removed from day-1:** `aws-sdk-s3`, `aws-config`, `tokio` (re-add when S3 ships).
**Changed:** `rage` → `age` (the library crate, not the CLI binary).

---

## 10. Implementation Roadmap

**Context:** Solo developer, side project (evenings/weekends). Each milestone
produces testable, working functionality. Every milestone has explicit
validation criteria that can be verified without an advisor.

**Total estimated calendar time:** 10-14 months.
**First real tape write:** ~8-10 weeks from start.

### Milestone 0: Validate External Dependencies + Full Round-Trip (2-3 weeks)

**Goal:** Confirm every external tool and crate works before writing tapectl code.
**Hard gate:** Milestone 0 is not complete until the full end-to-end round-trip
works. Do not proceed to Milestone 1 until all criteria pass.

**Individual dependency validation:**

- [ ] Install dar 2.7.20 to `/opt/dar`, verify:
  - Multi-slice archive (`-s 10M`)
  - Catalog isolation (`-C`)
  - XML listing (`-T xml`) — parse output manually
  - Symlinks (`-D`), xattr/ACL (`-am --acl --fsa-scope linux_extX`)
  - Per-file checksums (`--hash sha256`)
- [ ] Write standalone Rust program validating `age` crate:
  - Generate X25519 keypair
  - Multi-recipient encrypt (2 recipients)
  - Decrypt with each recipient independently
  - Streaming encrypt/decrypt of 1 GB file
  - Verify age CLI interop (encrypt with crate, decrypt with `age` binary)
- [ ] Set up mhvtl, verify:
  - Device appears as `/dev/nst0` and `/dev/sg*`
  - `mt -f /dev/nst0 status` works
  - Write + read + verify cycle with `dd`
  - `sg_read_attr` returns MAM data (mhvtl may simulate this)
  - `sg_logs` returns error counters
- [ ] Write standalone Rust program testing tape ioctl:
  - Open `/dev/nst0`, set variable block mode
  - Write a test file, write file mark
  - Read back and compare
  - MTIOCGET for position
  - Rewind, seek forward, verify position

**Full pipeline round-trip (HARD GATE):**

Write a throwaway Rust program that performs the complete pipeline end-to-end:
dar → age encrypt → write to mhvtl → read back → age decrypt → dar extract.

This de-risks the real project risk: the seams between components. Does dar's
sliced output stream cleanly through age's streaming encryption at the sizes
you'll actually use? Does a 2.4GB encrypted slice round-trip through tape
without silent corruption?

- [ ] Round-trip with at least 3 dar slices (tests slice boundary handling)
- [ ] At least one slice ≥ 1 GB (tests streaming at production scale)
- [ ] Decrypt with both recipient keys independently (tests multi-recipient)
- [ ] `diff -r` between source and restored directory shows zero differences
- [ ] dar `-t` (test) passes on decrypted slices before extraction
- [ ] SHA-256 of encrypted slices on disk matches SHA-256 of slices read
  back from tape (tests tape fidelity)

**Do not proceed to Milestone 1 until all criteria pass.**

### Milestone 1: Foundation (3-4 weeks)

**Goal:** `tapectl init`, tenant/key management, unit registration.

Tasks:
- [ ] DB schema creation with `rusqlite_migration`
- [ ] Config parsing (config.toml with `[dar]`, `[[backends.lto]]`, `[defaults]`)
- [ ] Error types (`thiserror` for library, `anyhow` for CLI)
- [ ] Signal handler (SIGINT atomic flag)
- [ ] Event logging framework (events table)
- [ ] `tapectl init` — create DB, config, operator tenant + keypair, dar validation
- [ ] `tapectl tenant add/list/info` — with keypair generation
- [ ] `tapectl key generate/list/export/import`
- [ ] `tapectl unit init` — dotfile creation, nesting check
- [ ] `tapectl unit init-bulk` — batch registration with auto-naming
- [ ] `tapectl unit list/status/tag/rename`
- [ ] Unit dotfile sync (DB ↔ dotfile)
- [ ] `tapectl unit discover` — scan watch_roots for dotfiles

**Validation:**
```bash
tapectl init
tapectl tenant add alice --description "Alice's media"
tapectl unit init /media/tv/bb/s01 --tenant mike --tag tv
tapectl unit init /media/tv/bb/s02 --tenant mike --tag tv
tapectl unit list --tenant mike
tapectl unit status "tv/bb/s01"
# Verify: dotfiles created, DB populated, events logged
```

### Milestone 2: Pipeline — Snapshot + Stage (4-5 weeks)

**Goal:** Full create → stage pipeline producing encrypted dar slices.

Tasks:
- [ ] `tapectl snapshot create` — directory walk, manifest, files table
- [ ] dar wrapper — version check, archive creation, XML catalog parsing
- [ ] Source sha256 validation (always, regardless of checksum_mode)
- [ ] age encryption pipeline — multi-recipient streaming encrypt
- [ ] `tapectl stage create` — full pipeline: validate → dar → encrypt → checksums
- [ ] Stage set + stage slices DB records
- [ ] sha256 backfill into files/manifest_entries (first stage)
- [ ] Local dar catalog extraction
- [ ] `tapectl staging status/clean`
- [ ] Pre-flight staging space check
- [ ] Receipt generation
- [ ] `tapectl snapshot list/diff`
- [ ] `tapectl unit check-integrity`

**Validation:**
```bash
tapectl snapshot create "tv/bb/s01"
tapectl stage create "tv/bb/s01"
# Verify: staging dir has encrypted .dar.age files
# Verify: checksums in DB match files on disk
# Verify: age -d with tenant key produces valid dar archive
# Verify: dar -t on decrypted slices passes
# Verify: receipt file created
tapectl staging status
tapectl staging clean
# Verify: staged files removed, DB updated
```

### Milestone 3: Tape I/O — Write + Read (5-6 weeks)

**Goal:** Write to tape (mhvtl), read back, verify. Full 8-file volume layout.

Tasks:
- [ ] LTO backend implementing VolumeBackend trait
- [ ] Tape ioctl module (open, MTSETBLK, MTWEOFI, MTWEOF, MTIOCGET, MTEOM)
- [ ] MAM query via `sg_read_attr` shell-out
- [ ] Disable hardware compression (MODE SELECT shell-out or ioctl)
- [ ] `tapectl volume init LABEL` — write ID thunk
- [ ] Volume write pipeline (complete 8-file layout)
- [ ] Signal handling during write (Ctrl+C between slices)
- [ ] ENOSPC detection and layered recovery
- [ ] Write position tracking in DB
- [ ] `tapectl volume verify` (full + quick)
- [ ] `tapectl volume identify` (read ID + planning header)
- [ ] `tapectl volume append`
- [ ] Health data collection (sg_logs after write)
- [ ] Volume calibrate command
- [ ] MANIFEST.toml generation for envelopes
- [ ] RECOVERY.md generation for envelopes
- [ ] System guide template with tape-specific substitution

**Validation (on mhvtl):**
```bash
tapectl volume init L6-0001
tapectl snapshot create "tv/bb/s01"
tapectl stage create "tv/bb/s01"
tapectl volume write L6-0001
# Verify all 8 file zones written correctly:
mt -f /dev/nst0 rewind
dd if=/dev/nst0 bs=64k > file0.txt && cat file0.txt    # ID thunk
dd if=/dev/nst0 bs=64k > file1.md && cat file1.md      # System guide
dd if=/dev/nst0 bs=64k > file2.sh && cat file2.sh      # RESTORE.sh
# ... continue for all files
# Verify: planning header decrypts with operator key
# Verify: mini-index matches actual file positions
# Verify: tenant envelope decrypts with tenant key, contains MANIFEST.toml
# Verify: operator envelope decrypts, contains all-tenant manifest
# Verify: RESTORE.sh --info works on this tape
# Verify: RESTORE.sh --find-envelope finds correct envelope
tapectl volume verify L6-0001
# Verify: all slices pass checksum verification
```

**ENOSPC validation (mhvtl with small virtual tape):**
```bash
# Configure mhvtl with ~500MB tape
# Write data exceeding capacity
# Verify: metadata written despite ENOSPC
# Verify: write record shows eot_recovery mode
# Verify: tape is readable and self-describing
```

### Milestone 3b: ENOSPC Real-Hardware Test Session (1-2 weeks)

**Goal:** Validate ENOSPC recovery paths with intentionally loosened capacity
parameters on real LTO-6 hardware (or mhvtl with small virtual tape).

Tasks:
- [ ] Configure capacity parameters to force ENOSPC during data write
- [ ] Verify: metadata written despite ENOSPC (normal recovery)
- [ ] Configure parameters to force ENOSPC during metadata write
- [ ] Verify: fallback recovery (overwrite incomplete slice position)
- [ ] Configure parameters to force ENOSPC with no incomplete slice
- [ ] Verify: last-resort recovery (sacrifice last complete slice)
- [ ] Verify: in all cases, tape is readable and self-describing
- [ ] Verify: write records accurately reflect eot_recovery mode
- [ ] Verify: sacrificed slices are tracked in DB
- [ ] Verify: subsequent verification correctly handles sacrificed slices

**This milestone exists because ENOSPC recovery is the highest-stakes
failure mode in the system. Paper-designing it is insufficient — the
layered recovery must be validated against real tape behavior.**

### Milestone 4: Restore + Catalog (3-4 weeks)

**Goal:** Full restore from tape. Catalog browsing. Raw-volume recovery.

Tasks:
- [ ] `tapectl restore --unit NAME --from LABEL --to DIR`
- [ ] `tapectl restore --file PATH --unit NAME --from LABEL --to DIR`
- [ ] `tapectl restore --dry-run` (shows what would be restored)
- [ ] `tapectl restore --raw-volume` (manifest-based, no DB needed)
- [ ] `tapectl catalog ls/search/locate/stats`
- [ ] `tapectl snapshot delete` (unwritten only)
- [ ] `tapectl quick-archive` convenience command

**Validation:**
```bash
# Full round-trip test:
tapectl quick-archive /media/tv/bb/s01 --tenant mike --tag tv --volume L6-0001
rm -rf /tmp/restore-test && mkdir /tmp/restore-test
tapectl restore --unit "tv/bb/s01" --from L6-0001 --to /tmp/restore-test/
# Compare: diff -r /media/tv/bb/s01 /tmp/restore-test/tv/bb/s01

# Raw-volume restore (no DB):
tapectl restore --raw-volume --from L6-0001 --key ~/.tapectl/keys/mike-primary.age.key --to /tmp/raw-restore/

# Manual restore using only RESTORE.sh:
mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 2
dd if=/dev/nst0 bs=64k > /tmp/RESTORE.sh
chmod +x /tmp/RESTORE.sh
/tmp/RESTORE.sh --info
/tmp/RESTORE.sh --find-envelope --key ~/.tapectl/keys/mike-primary.age.key
```

### Milestone 5: Safety + Operations (4-5 weeks)

**Goal:** Location tracking, retirement, clone, mark-tape-only, DB safety,
cartridge tracking.

Tasks:
- [ ] `tapectl location add/list/info/rename`
- [ ] `tapectl volume move LABEL --to LOCATION`
- [ ] `tapectl volume retire LABEL` + impact analysis
- [ ] `tapectl unit mark-tape-only` with min_copies/min_locations enforcement
- [ ] `tapectl volume read-slices` (staging-only, then `volume write` for dest)
- [ ] `tapectl cartridge register/list/info/mark-erased`
- [ ] Cartridge ↔ volume relationship tracking
- [ ] `tapectl db backup` (passphrase + keys + catalogs)
- [ ] `tapectl db fsck --repair`
- [ ] `tapectl snapshot diff`
- [ ] `tapectl export --unit NAME --to DIR`
- [ ] `tapectl unit discover` change detection

**Validation:**
```bash
tapectl cartridge register --barcode L6-0001 --media-type LTO-6
tapectl volume move L6-0001 --to home-rack
tapectl volume init L6-0002
tapectl volume write L6-0002    # second copy
tapectl volume move L6-0002 --to parents-house
tapectl unit mark-tape-only "tv/bb/s01"
# Verify: succeeds (2 copies, 2 locations)
tapectl volume retire L6-0001
# Verify: impact analysis shows drop to 1 copy, suggests remediation
tapectl db backup --passphrase
```

### Milestone 6: Policy + Reporting + Compaction (4-5 weeks)

**Goal:** Archive sets, policy audit, comprehensive reporting, compaction workflow.

Tasks:
- [ ] `tapectl archive-set create/edit/list/info/sync`
- [ ] Policy resolution (dotfile > archive_set > defaults)
- [ ] `tapectl audit` — compliance check + action plan + JSON + compaction candidates
- [ ] `tapectl snapshot mark-reclaimable` with enforced preconditions
- [ ] `tapectl volume compact-read/compact-write/compact-finish/compact`
- [ ] `tapectl report summary/fire-risk/copies/tape-only/dirty/pending`
- [ ] `tapectl report verify-status/health/capacity/age/events/compaction-candidates`
- [ ] `--json` output for all commands
- [ ] Shell completions

**Validation:**
```bash
tapectl archive-set create "critical-media" --min-copies 3 --required-locations "home-rack,parents-house"
tapectl audit
# Verify: reports violations and compaction candidates
tapectl audit --action-plan
tapectl audit --format json | jq .
tapectl snapshot mark-reclaimable "tv/bb/s01" --version 1
# Verify: preconditions enforced
tapectl report compaction-candidates
```

### Milestone 7: Hardening + Real Hardware (3-4 weeks)

**Goal:** Production readiness. Real LTO-6 validation.

Tasks:
- [x] Full audit trail (every field change with old/new values)
- [x] Health trending from sg_logs data
- [x] FTS5 search index for catalog
- [x] `tapectl import` (pre-existing volumes)
- [x] Comprehensive unit tests for every module
- [x] Integration tests against mhvtl (automated test suite)
- [x] End-to-end tests: init → multi-tenant write → multi-tape restore
- [x] Failure mode tests: interrupted writes, corrupted staging, missing keys,
  crashed DB recovery, raw-volume restore, ENOSPC recovery
- [x] Performance tests: large units, many units (500+), many files (5K+) — dev-VM
  scale; production 2+ TB / 100K+ targets gated on real hardware (see
  `docs/perf-baselines.md`)
- [ ] **Real LTO-6 hardware validation** — full write + verify + restore cycle
  (procedure in `docs/lto6-validation-checklist.md`; user-gated on hardware)
- [x] Multi-tenant isolation validation (tenant A cannot see tenant B's data)
- [x] Documentation: README, operator guide, man pages

**Validation:** All automated tests pass. Full round-trip on real LTO-6 tape
with multi-tenant data. Manual RESTORE.sh recovery works on real hardware.
ENOSPC recovery tested on real hardware (fill a tape).

---

## 11. Pre-Implementation Validation

**Before writing any tapectl Rust code:**

```bash
# 1. dar
dar --version                                  # ≥ 2.6.x?
dar -c /tmp/dar-test -R /tmp/dar-test-data -s 10M
dar -l /tmp/dar-test -T xml                    # verify XML
dar -C /tmp/dar-test-catalog -A /tmp/dar-test  # catalog isolation

# 2. age crate
# Standalone Rust program: multi-recipient, streaming, CLI interop

# 3. Tape (mhvtl)
mt -f /dev/nst0 status
sg_read_attr -r /dev/sg1                       # MAM data
sg_logs --page=0x02 /dev/sg1                   # error counters

# 4. Tape ioctl
# Standalone Rust program: open, write, filemark, read, position check

# 5. FULL ROUND-TRIP (HARD GATE)
# Standalone Rust program: dar → age encrypt → tape → age decrypt → dar extract
# Must pass ALL criteria in Milestone 0 before proceeding
```

---

## 12. Implementation Risk Assessment

| Risk | Severity | Impact | Mitigation |
|------|----------|--------|------------|
| dar XML output inconsistencies | HIGH | HIGH | Milestone 0 validation; parse real output early |
| dar slice boundary + age streaming interaction | HIGH | HIGH | Full round-trip test in Milestone 0 |
| rage/age crate API instability (pre-1.0) | MEDIUM | LOW | Pin to 0.11.x; crypto boundary isolated in `crypto/` |
| LTO ENOSPC edge cases on real hardware | MEDIUM | HIGH | Milestone 3b dedicated test session |
| SQLite under WAL mode with concurrent access | LOW | LOW | Single-operator tool; non-issue |
| Staging peak disk pressure | LOW | MEDIUM | Peak = total_unit_size + one_slice_size; document clearly |

---

## 13. Key Design Decisions with Rationale

This section captures every significant design decision. Understanding *why*
things are the way they are prevents re-litigating settled questions and helps
make consistent future decisions.

### Foundation Choices

| Decision | Rationale |
|----------|-----------|
| dar over tar | Built-in slicing, file-level catalog, XML listing, encryption-per-slice. tar would require building all of this on top. |
| Rust | Correctness and long-term maintainability via type system. Not about performance — about invariants. |
| SQLite (rusqlite, bundled) | Single-file, no daemon, embedded, WAL mode. DB travels with the tool. |
| age encryption (rage crate) | Simple key files, no keyring daemon, no trust model complexity. Multi-recipient in single pass. Pure Rust (no C deps). |
| Not LTFS/Bacula/Amanda | LTFS slow at scale, filesystem metaphor breaks. Bacula/Amanda are backup-oriented with retention cycles, not archival. |

### Pipeline Design

| Decision | Rationale |
|----------|-----------|
| Three-phase pipeline (create/stage/write) | Decouples content identity from physical artifacts. Maps to natural operator work sessions. |
| Re-run dar per staging | No long-lived staging pressure. Source always validated fresh. Peak staging = total_unit_size + one_slice_size. |
| Always sha256 before staging | Bitrot protection at the archival commitment point. checksum_mode governs fast scans only. |
| Reusable staged slices (manual cleanup) | Avoids redundant dar runs for immediate second copies. Operator controls disk space. |
| Stage sets as bridge entity | Captures dar+encrypt execution; enables reuse; tracks byte-identity of multi-copy writes. |
| Files table has no slice mapping | dar catalog handles file-to-slice mapping internally. Tracking it in tapectl would be fragile across re-staging. |

### Tape and Volume Design

| Decision | Rationale |
|----------|-----------|
| Kernel st driver (not SG_IO) | Simpler, auto-filemark safety, mt-st compatible. SG is escape hatch for future needs. |
| sg_logs/sg_read_attr shell-out | Complex SCSI log pages, infrequent (2-3x per session), existing tools handle it. |
| MAM query for real capacity | No static guessing; per-cartridge accuracy from hardware. |
| Hardware compression disabled | Encrypted data is random; compression wastes CPU. |
| File mark overhead eliminated | LTO file marks cost bytes, not megabytes. |
| Self-describing volumes (8-file layout) | Full restore without DB or tapectl installation. |
| System guide designed for LLM recovery | Future-proof: AI-assisted multi-tape restoration. |
| Manifest at end of tape (not beginning) | Can't know contents until write completes. ID block at File 0 is small and fixed. |
| Append model (no mid-tape overwrites) | Dead metadata space (~100 MB) negligible on 2.5 TB tape. |
| Planning header at File 3 | Quick tape identification without seeking to EOT. |
| Single tape per session | With standalone drive and manual swaps, multi-tape sessions add complexity without benefit. |

### Security and Isolation

| Decision | Rationale |
|----------|-----------|
| Strict tenant isolation on tape | Zero content metadata in plaintext. Envelopes only. |
| Envelope shuffling | No positional correlation to tenant identity on tape. |
| Trial-decryption for envelope discovery | age header check, fast (milliseconds), leaks nothing. |
| Per-tenant keys + operator key | Tenant isolation; operator can always recover. |
| Operator is "just a tenant" | Eliminates special-case code paths for operator data. |
| Old keys never deleted, only deactivated | Restore tries all known keys. No key loss risk. |

### Safety and Operations

| Decision | Rationale |
|----------|-----------|
| ENOSPC layered recovery | Tape always self-describing, even at physical limits. |
| Ctrl+C safe (SIGINT atomic flag) | Write positions only created after confirmed written. DB always consistent. |
| Policy audit is advisory, never blocking | Exit codes enable scripting without blocking human judgment. |
| Locations are informational, not enforced | Audit reports compliance gaps; doesn't prevent writes. |
| Volume read-slices for tape-only recovery | Staging-only read of encrypted bytes; `volume write` handles dest tape with full layout. Same stage_set, same checksums. |
| Cartridge/volume separation | Enables physical cartridge reuse across volume lifetimes. |
| Explicit compaction steps (not automatic) | Consistent with three-phase philosophy: explicit steps for control, wrapper for convenience. |
| Reclaimable gating with preconditions | Never lose data silently. Superseding snapshot must meet policy before old one is reclaimable. |

### Deferred Decisions (explicitly punted to post-v1)

| Decision | Status |
|----------|--------|
| Incremental/differential snapshots | Schema includes `snapshot_type` and `base_snapshot_id` for future migration |
| S3 backend | Backend trait designed for it; ships when core LTO+export proven |
| Web UI for catalog browsing | Schema designed for web-layer integration |
| Small-unit batching | Each small unit gets own dar archive; batching deferred |
| Per-tenant usage reporting | Can query DB directly |
| Write-back verification (read-after-write) | Optional via `--write-verify` flag, not default (doubles write time) |

---

## Appendix A: System Guide Template

The complete system guide template for File 1 on every tape. `{variables}`
are substituted at write time. The template is stored as a Rust `const &str`
and performs string replacement before writing to tape.

````markdown
# TAPECTL ARCHIVAL STORAGE — COMPLETE SYSTEM GUIDE

Tape: {label} | Layout version: 1 | tapectl {version}

This document describes the tapectl archival storage system in
enough detail to recover any data from any set of tapectl tapes,
even without the tapectl software installed. It is included on
every tape so it is never lost.

If you are an AI assistant helping someone recover data from
these tapes, this document contains everything you need to
orchestrate the full recovery. Read it completely before
taking any action.

---

## 1. SYSTEM OVERVIEW

tapectl is an archival storage system that writes encrypted data
to LTO tapes and portable directories. Each tape is self-describing:
between this guide, the mini-index, and the encrypted envelopes on
each tape, you can recover all data without any external database
or software beyond standard Unix tools, the `age` encryption tool,
and the `dar` archiver.

Key properties:

- Data is encrypted. You MUST have a private key file to access it.
- Multiple tenants (data owners) may share a single tape.
  Each tenant can only decrypt and see their own data.
- An "operator" is the system administrator who holds all keys
  and can access everything.
- Data is organized into "units" (e.g., one TV season, one photo
  collection). Each unit is archived as a set of numbered `dar`
  (Disk ARchive) slices.
- The same unit may appear on multiple tapes as redundant copies.
- A unit's slices are always together on one tape — never split
  across multiple tapes.

---

## 2. WHAT IS ON EACH TAPE

Every tapectl tape has this file layout. Files are separated by
tape file marks. Use `mt` to position between them.

| File | Name | Encrypted? | Contents |
|------|------|-----------|----------|
| 0 | ID thunk | No | Short identity block. You already read past it. |
| 1 | System guide | No | THIS DOCUMENT. |
| 2 | RESTORE.sh | No | Executable bash script automating recovery. |
| 3 | Planning header | Yes (operator) | What was planned for this tape. |
| 4..N | Data slices | Yes (tenant+operator) | The actual archived data. |
| N+1 | Mini-index | No | Position/size of every file. No content info. |
| N+2..N+K | Tenant envelopes | Yes (per-tenant) | Per-tenant manifests + catalogs. |
| N+K+1 | Operator envelope | Yes (operator) | Full manifest for all tenants. |
| N+K+2 | Operator backup | Yes (operator) | Identical copy of operator envelope. |

The exact file numbers for this tape are listed in Section 11.

---

## 3. TOOLS REQUIRED

You need four tools. All are free, open-source, and available
for Linux, macOS, and most Unix systems.

### mt-st (tape positioning)

Sends commands to the tape drive: rewind, seek, status.

    Install:  apt install mt-st        (Debian/Ubuntu)
              yum install mt-st        (RHEL/CentOS)
    Test:     mt -f /dev/nst0 status

### dd (data extraction)

Reads raw data from the tape device. Already on every Unix system.

    Test:     dd if=/dev/nst0 bs=64k count=1 > /dev/null

### age (decryption)

Decrypts the encrypted files on this tape.

    Install options:
      - age (Go reference):  https://github.com/FiloSottile/age
      - rage (Rust):         https://github.com/str4d/rage
      - System package:      apt install age

    A private key file looks like this (one line):
      AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ

    To decrypt a file:
      age -d -i /path/to/key.file < encrypted_file > decrypted_file

### dar (archive extraction)

Extracts files from the decrypted archive slices.

    Install:  apt install dar          (version 2.6+)
    Source:   https://github.com/Edrusb/DAR

    Useful commands:
      dar -l basename                  List all files in the archive
      dar -l basename -T xml           List in structured XML format
      dar -x basename -R /output/      Extract everything
      dar -x basename -R /out/ -g path Extract one file
      dar -t basename                  Test archive integrity

---

## 4. UNDERSTANDING ENCRYPTION KEYS

All data on these tapes is encrypted with `age` (X25519 keys).
Each encrypted file has multiple recipients — typically the data
owner's key AND the operator's key.

### Key types

- **Tenant key:** Belongs to one data owner. Decrypts only that
  tenant's envelopes and data slices.
- **Operator key:** The system administrator's key. Decrypts
  everything on every tape.

### Where to find keys

Search these locations in order:

1. `~/.tapectl/keys/` directory (if tapectl was ever installed)
   - Files named `{tenant}-{alias}.age.key` (private)
   - Files named `{tenant}-{alias}.age.pub` (public — not useful for decryption)
2. A password manager, secrets vault, or encrypted notes
3. A USB drive, SD card, or paper backup stored in a safe
4. Ask the person who administered the tapectl system

### Identifying a key file

A private key file contains exactly one line starting with:

    AGE-SECRET-KEY-1

To see the corresponding public key (starts with `age1`):

    age-keygen -y /path/to/key.file

### If you have NO key

Without a private key, you can read:
- This guide (File 1)
- The mini-index — shows all file positions and sizes (File N+1)
- RESTORE.sh — the recovery script (File 2)

You CANNOT decrypt any data or envelopes. The encryption uses
X25519 + ChaCha20-Poly1305. There is no backdoor, no master
password, and no way to recover data without the correct key.

---

## 5. SINGLE-TAPE RECOVERY

Follow these steps to recover data from one tape.

### Step 1: Read the mini-index

The mini-index tells you the position and size of every file
on the tape. It requires no decryption.

```bash
TAPE=/dev/nst0
mt -f $TAPE rewind
mt -f $TAPE fsf {mini_index_pos}
dd if=$TAPE bs=1M > mini-index.txt
cat mini-index.txt
```

### Step 2: Find your envelope

The tenant envelopes are after the mini-index. Each is encrypted
to one tenant. Try your key against each one until one decrypts.

```bash
mt -f $TAPE rewind
mt -f $TAPE fsf {first_envelope_pos}

for i in $(seq 1 {num_envelopes}); do
    dd if=$TAPE bs=1M > envelope_$i.age 2>/dev/null
    if age -d -i /path/to/your.key < envelope_$i.age > envelope_$i.tar 2>/dev/null; then
        echo "SUCCESS: Envelope $i is yours"
        mkdir -p my_envelope
        tar xf envelope_$i.tar -C my_envelope/
        break
    else
        echo "Envelope $i: not yours (expected, trying next)"
    fi
done
```

### Step 3: Read your manifest

Your decrypted envelope contains:

    my_envelope/
    ├── MANIFEST.toml     Structured list of your data on this tape
    ├── RECOVERY.md       Step-by-step instructions specific to your data
    └── catalogs/         dar catalog files for browsing archives
        └── *.1.dar

Read RECOVERY.md for instructions tailored to your specific data.
Or continue with the general procedure below.

### Step 4: Extract and decrypt data slices

MANIFEST.toml lists your units and their tape positions. For
each unit, extract its slices:

```bash
# Example: unit has slices at tape files 4 through 15
mt -f $TAPE rewind
mt -f $TAPE fsf 4

# Read slices sequentially (do NOT rewind between them)
for n in $(seq 1 12); do
    dd if=$TAPE bs=1M > "unit_v1.$n.dar.age"
done

# Decrypt all slices
for f in *.dar.age; do
    age -d -i /path/to/your.key < "$f" > "${f%.age}"
done
```

### Step 5: Restore with dar

```bash
# List contents
dar -l unit_v1

# Extract everything
mkdir -p restored/
dar -x unit_v1 -R restored/

# Or extract one file
dar -x unit_v1 -R restored/ -g "season01/s01e03.mkv"
```

dar automatically reassembles the numbered slices. They must be
in the same directory, named `unit_v1.1.dar`, `unit_v1.2.dar`, etc.

---

## 6. MULTI-TAPE RECOVERY

If you have multiple tapectl tapes, build a complete inventory
before restoring anything. Tapes can be processed in any order.

### Phase 1: Inventory each tape

For each tape:

```bash
mt -f $TAPE rewind
dd if=$TAPE bs=64k > id_header.txt

# Note the label from id_header.txt
# Note the mini_index and first_envelope positions from [layout]

mt -f $TAPE rewind
mt -f $TAPE fsf {mini_index_pos}
dd if=$TAPE bs=1M > {label}_mini-index.txt

mt -f $TAPE rewind
mt -f $TAPE fsf {first_envelope_pos}
# Try each envelope with your key (see Step 2 above)
# Save successful result as {label}_manifest/
```

### Phase 2: Build cross-tape inventory

Each MANIFEST.toml lists units with:
- Unit name and UUID (unique identifier)
- Snapshot version number
- Slice tape positions

Combine all manifests to determine:
- Which units exist across all tapes
- Which tapes have copies of which units
- The latest snapshot version of each unit

A unit on multiple tapes is an intentional redundant copy.
You only need to restore from ONE tape per unit.

### Phase 3: Restore

For each unit you want:
1. Pick any tape with the latest version of that unit
2. Insert that tape
3. Follow the single-tape procedure (Section 5)
4. Verify SHA-256 checksums from MANIFEST.toml

### Important multi-tape notes

- A unit's slices are ALWAYS on one tape. You never need two
  tapes for one unit.
- If a tape is unreadable, check other tapes for copies of the
  same unit (matching UUID, same or newer version).
- Insert tapes in any order. There is no required sequence.
- The operator envelope on any tape contains ALL tenants' data
  catalogs for that tape.

---

## 7. DAR ARCHIVE FORMAT

dar (Disk ARchive) is the archive format inside the encrypted
slices.

- Archives split into numbered slices: `name.1.dar`, `name.2.dar`
- All slices must be in the same directory for extraction
- dar preserves: permissions, ownership, timestamps, xattrs,
  ACLs, symlinks
- dar has its own catalog system for listing files without
  reading all data slices

### dar catalog files

Each envelope includes dar catalog files in `catalogs/`. Use
them to list archive contents without reading data from tape:

```bash
dar -l catalogs/unit_basename -T xml
```

---

## 8. ENCRYPTION FORMAT

Files are encrypted with age v1 (https://age-encryption.org).

- Recipient type: X25519
- Payload cipher: ChaCha20-Poly1305 in 64KB streaming chunks
- Multi-recipient: each file decryptable by ANY recipient key
- Header: ~200 bytes per recipient; reveals nothing about
  plaintext or other recipients

The encryption wraps dar archive slices:

    original files → dar slices → age encryption

To reverse:

    age decryption → dar slices → original files

---

## 9. LAYOUT VERSION HISTORY

**Version 1** (current):

Files 0-2 are always plaintext. File 3 is always encrypted to
operator. Files 4..N are data. File N+1 is plaintext mini-index.
Files after the mini-index are encrypted envelopes. Last two
files are dual operator envelopes.

The `[layout]` section in File 0 provides exact positions.

If a future layout version exists, the tape's File 0 will state
the new version number, and that tape's system guide (File 1)
will describe the new layout.

---

## 10. ABOUT TAPECTL

tapectl is an open-source Rust CLI tool for managing long-term
archival storage.

    Repository:  https://github.com/{owner}/tapectl
    Written in:  Rust
    License:     {license}

If tapectl is available, use it instead of manual recovery:

```bash
tapectl restore --unit "name" --from {label} --to /output/
```

This guide exists for when tapectl is unavailable.

---

## 11. THIS TAPE

| Property | Value |
|----------|-------|
| Label | {label} |
| Media | {media_type} |
| Capacity (MAM) | {mam_capacity_mb} MB |
| tapectl version | {version} |
| Layout version | 1 |
| Written | {created_date} |
| Data slices | files {data_start} through {data_end} ({num_data_slices} slices) |
| Mini-index | file {mini_index_pos} |
| Tenant envelopes | files {first_envelope_pos} through {last_envelope_pos} ({num_envelopes} envelopes) |
| Operator envelope | file {op_envelope_pos} |
| Operator backup | file {op_backup_pos} |
| Total tape files | {total_files} |
````

---

## Appendix B: On-Tape Format Specifications (Layout Version 1)

**These formats are frozen once the first layout_version=1 tape is written.**
Any breaking change requires incrementing layout_version and implementing
backward-compatible readers for all prior versions.

### B.1 Layout Version 1 Contract

Layout version 1 guarantees:

1. Files are separated by tape file marks (MTWEOFI/MTWEOF)
2. File 0 is always the ID thunk (plaintext, prose+TOML hybrid)
3. File 1 is always the system guide (plaintext markdown)
4. File 2 is always RESTORE.sh (plaintext bash)
5. File 3 is always the planning header (age-encrypted TOML)
6. Files 4 through N are encrypted data slices (age-encrypted dar slices)
7. File N+1 is always the mini-index (plaintext, prose+TOML hybrid)
8. Files N+2 through N+K are tenant envelopes (age-encrypted tar)
9. File N+K+1 is the operator envelope (age-encrypted tar)
10. File N+K+2 is the operator envelope backup (age-encrypted tar, identical to N+K+1)
11. The ID thunk `[layout]` section provides the exact file number for each zone
12. All plaintext files use UTF-8 encoding
13. All TOML sections use TOML v1.0
14. All encrypted files use the age v1 format (X25519 recipients)
15. All tar archives inside envelopes use POSIX ustar format
16. File marks between data slices use MTWEOFI (immediate, no flush)
17. File marks after the last operator envelope use MTWEOF (synchronous flush)

### B.2 RESTORE.sh Specification

**Format:** Bash script. `#!/usr/bin/env bash` shebang. `set -euo pipefail`.

**Substituted constants** (embedded at top of script, no content metadata):

```bash
LABEL="{label}"
LAYOUT_VERSION=1
TAPECTL_VERSION="{version}"
DATA_START={data_start}
DATA_END={data_end}
MINI_INDEX_POS={mini_index_pos}
FIRST_ENVELOPE={first_envelope_pos}
NUM_ENVELOPES={num_envelopes}
OP_ENVELOPE={op_envelope_pos}
TOTAL_FILES={total_files}
```

**Required subcommands:**

| Subcommand | Requires Key | Description |
|-----------|-------------|-------------|
| `--info` | No | Reads ID thunk + mini-index, prints tape summary |
| `--find-envelope --key FILE` | Yes | Trial-decrypts each envelope, extracts matching one |
| `--extract-slice --position N --output FILE` | No | Reads one tape file to disk |
| `--decrypt --key FILE --input FILE --output FILE` | Yes | Decrypts one age file |
| `--restore --key FILE --manifest DIR [--unit NAME] [--to DIR]` | Yes | Full restore from decrypted manifest |
| `--help` | No | Prints usage with examples |

**Exit codes:** 0=success, 1=usage error, 2=tape error, 3=decryption error,
4=dar error, 5=verification error.

### B.3 Planning Header Schema

**Format:** age-encrypted TOML. Encrypted to operator key(s) only.

```toml
[planning_header]
schema_version = 1
tape_label = "{label}"
tapectl_version = "{version}"
planned_at = "{iso8601}"
status = "planned"

[capacity]
mam_maximum_mb = {integer}
mam_remaining_mb = {integer}
planned_data_bytes = {integer}
planned_slices = {integer}
planned_manifest_reserve_bytes = {integer}
estimated_fill_percent = {float}

[[planned_writes]]
unit_name = "{name}"
unit_uuid = "{uuid}"
tenant = "{tenant_name}"
snapshot_version = {integer}
stage_set_id = {integer}
num_slices = {integer}
total_encrypted_bytes = {integer}
```

### B.4 Mini-Index Schema

**Format:** Plaintext UTF-8. Prose header followed by TOML.

**Contains ZERO content metadata.**

```toml
[mini_index]
schema_version = 1
label = "{label}"
layout_version = 1
written_at = "{iso8601}"

[[files]]
position = 0
type = "id_thunk"
size_bytes = {integer}

[[files]]
position = 1
type = "system_guide"
size_bytes = {integer}

[[files]]
position = 2
type = "restore_script"
size_bytes = {integer}

[[files]]
position = 3
type = "planning_header"
size_bytes = {integer}

[[files]]
position = 4
type = "data_slice"
size_bytes = {integer}

# ... one entry per data slice, sequential positions ...

[[files]]
position = {N+1}
type = "mini_index"
size_bytes = {integer}

[[files]]
position = {N+2}
type = "tenant_envelope"
size_bytes = {integer}

# ... one entry per tenant envelope ...

[[files]]
position = {N+K+1}
type = "operator_envelope"
size_bytes = {integer}

[[files]]
position = {N+K+2}
type = "operator_envelope"
size_bytes = {integer}
```

### B.5 Tenant Envelope Format

**Outer format:** age-encrypted file. Recipients: tenant's active key(s) +
operator's active key(s).

**Inner format:** POSIX ustar tar archive.

**Tar contents:**
```
MANIFEST.toml
RECOVERY.md
catalogs/
catalogs/{uuid}_v{version}.1.dar
```

#### MANIFEST.toml Schema

```toml
[manifest]
schema_version = 1
tape_label = "{label}"
tapectl_version = "{version}"
written_at = "{iso8601}"
tenant = "{tenant_name}"

[[units]]
name = "{unit_name}"
uuid = "{unit_uuid}"
snapshot_version = {integer}
stage_set_id = {integer}
dar_version = "{dar_version_string}"
dar_command = "{full_dar_command_line}"
catalog_file = "catalogs/{uuid}_v{version}.1.dar"

[[units.slices]]
number = {integer}
tape_position = {integer}
size_bytes = {integer}
encrypted_bytes = {integer}
sha256_plain = "{hex}"
sha256_encrypted = "{hex}"

[[units.files]]
path = "{relative_path}"
size_bytes = {integer}
sha256 = "{hex}"
modified_at = "{iso8601}"
is_directory = {boolean}
```

### B.6 Operator Envelope Format

Identical tar structure to tenant envelopes, with these differences:

1. `MANIFEST.toml` contains ALL tenants' data (merged god-view)
2. `RECOVERY.md` covers all tenants
3. `catalogs/` contains dar catalogs for ALL units on this tape
4. Additional file: `catalog.db` — portable SQLite subset for this tape

---

## Appendix C: Write Pipeline State Machine

### C.1 Write States

```
planned → in_progress → completed
                      → interrupted (Ctrl+C or ENOSPC with successful metadata)
                      → failed (unrecoverable error)
                      → aborted (detected on startup as orphaned)
```

### C.2 Detailed Write Sequence

```
STEP  ACTION                           DB STATE                      ON FAILURE
─────────────────────────────────────────────────────────────────────────────────
 0    Create write record               write.status = 'planned'      —
 1    Open /dev/nst0                    —                             → failed
      Set variable block mode
      Disable hardware compression
 2    Read File 0, verify label         —                             → failed
      matches write.volume.label                                      "Wrong tape"
 3    Query MAM capacity                volume.mam_capacity_bytes     → warn, continue
      Store in volume record            volume.mam_remaining_at_start  (use nominal)
 4    Verify capacity sufficient        —                             → failed
      for planned slices + reserve                                    "Insufficient space"
 5    MTEOM (seek to end of data)       —                             → failed
      Verify position matches expected
      file count from DB
 6    Write File 0: ID thunk            —                             → failed
      Write File 1: System guide
      Write File 2: RESTORE.sh
      Write File 3: Planning header
      (each followed by MTWEOFI)
 7    write.status = 'in_progress'      write.status = 'in_progress'
      write.started_at = now()
 8    FOR EACH staged slice:
      8a  Verify slice checksum on disk —                             → failed
      8b  Check SIGINT flag             —                             → goto METADATA
      8c  write() slice to tape fd      —                             ENOSPC → goto METADATA
                                                                      EIO → failed
      8d  MTWEOFI (file mark)           —
      8e  Create write_position record  wp.status = 'written'
          wp.written_at = now()
 9    METADATA (normal or recovery):
      9a  Write mini-index (MTWEOFI)    —                             ENOSPC → goto RECOVERY
      9b  FOR EACH tenant with data:
          Generate + encrypt envelope
          Write to tape (MTWEOFI)       —                             ENOSPC → goto RECOVERY
      9c  Generate + encrypt operator envelope
          Write to tape (MTWEOF)        —                             ENOSPC → goto RECOVERY
      9d  Write operator backup (MTWEOF) —
10    write.status = 'completed'        write.status = 'completed'
      write.completed_at = now()
      volume.has_manifest = 1
      volume.bytes_written += total
      snapshot.status = 'current' (if first write)
      Collect sg_logs health data
      Write receipt

RECOVERY (ENOSPC during metadata):
  R1  If incomplete slice exists:
      Seek to start of incomplete slice position
      write.eot_recovery = 'overwrite_incomplete'
      Retry METADATA from step 9a
  R2  If R1 also hits ENOSPC:
      Seek to start of LAST COMPLETE slice
      Mark that slice's write_position as 'sacrificed'
      write.eot_recovery = 'sacrifice_last_slice'
      write.sacrificed_slice_id = last_slice_id
      Retry METADATA from step 9a
  R3  If R2 also hits ENOSPC (should be impossible — 2.4GB freed):
      write.status = 'failed'
      write.notes = "CRITICAL: could not write metadata even after sacrificing slice"
      Log critical error

INTERRUPT (SIGINT detected at step 8b):
  I1  Stop writing slices
  I2  write.eot_recovery = 'normal'
  I3  Goto METADATA (step 9)
  I4  write.status = 'interrupted' (instead of 'completed')
```

### C.3 Resume After Interruption

`tapectl volume write LABEL` (same label as interrupted write):

1. Detect interrupted write for this volume in DB
2. Read tape: verify ID block, count files via MTIOCGET
3. Compare tape file count against DB write_positions
4. Identify which slices were written and which remain
5. Present summary: "N of M slices written. Resume? [Y/n]"
6. MTEOM to position after last file mark
7. Continue write from next unwritten slice
8. Write fresh metadata (replaces old metadata if any)

---

## Appendix D: Append Semantics

### D.1 What Append Means

Appending adds new data to a tape that already has data from a previous
write session. The old data slices remain intact. New slices and fresh
metadata are written after the old metadata.

### D.2 Append Tape Layout

```
BEFORE APPEND:
[F0:ID][F1:Guide][F2:Script][F3:Plan][S1][S2][S3][Mini][Env1][Env2][Op][Op2]

AFTER APPEND:
[F0:ID][F1:Guide][F2:Script][F3:Plan][S1][S2][S3][OLD-Mini][OLD-Env...][OLD-Op...]
                                                   ↑ dead space (still readable)
[S4][S5][NEW-Mini][NEW-Env1][NEW-Env2][NEW-Env3][NEW-Op][NEW-Op2]
```

### D.3 Append Rules

1. **Files 0-2 (ID, Guide, Script) are NOT rewritten.** Immutable after first write.
2. **File 3 (Planning header) is NOT rewritten.** New planning header embedded
   in new operator envelope instead.
3. **Old metadata becomes dead space.** Still physically readable but superseded.
4. **New mini-index covers ALL files** (old + new data slices + new metadata).
5. **New tenant envelopes cover ALL data** (old + new writes).
6. **New operator envelopes cover ALL data** (complete god-view).
7. The new metadata is **larger** because it covers more data.
   `manifest_reserve` must account for this growth.

### D.4 Append Capacity Calculation

```
available_for_new_slices = mam_remaining_bytes
                         - new_manifest_reserve   # covers ALL data on tape
                         - enospc_buffer
```

The new `manifest_reserve` scales with the total number of tenants and units
on the tape (old + new), not just the newly appended data.

### D.5 Append Implementation

```
1. Open tape, verify ID block
2. Query MAM remaining capacity
3. Read old mini-index to enumerate existing files
4. MTEOM (positions after old final file mark)
5. Verify MTIOCGET position matches expected
6. Write new data slices (Files N+1..M)
7. Write new mini-index (covers files 0..M+metadata)
8. Write new envelopes (cover ALL writes on this tape)
9. Write new dual operator envelopes
10. Update volume record (bytes_written, num_data_files, last_write)
```

### D.6 Dead Space Accounting

`volume.bytes_written` includes dead metadata from previous sessions.
Conservative — capacity formula counts dead space as consumed.
Dead space per append typically < 100 MB. Even 10 appends waste < 1 GB
on a 2.5 TB tape.

---

## Appendix E: Event Logging Specification

### E.1 Granularity

Events are logged at **entity-operation level**, not column level. Each
event records one logical operation with a JSON `details` field capturing
the specific changes.

### E.2 When to Log

| Operation | entity_type | action | details |
|-----------|------------|--------|---------|
| Unit registered | unit | created | `{"name": "...", "tenant": "...", "path": "..."}` |
| Unit status change | unit | status_changed | `{"old": "active", "new": "tape_only"}` |
| Unit path changed | unit | path_changed | `{"old": "/old/path", "new": "/new/path"}` |
| Snapshot created | snapshot | created | `{"unit": "...", "version": N, "file_count": N}` |
| Snapshot status change | snapshot | status_changed | `{"old": "created", "new": "staged"}` |
| Stage set created | stage_set | created | `{"snapshot_id": N, "dar_version": "...", "num_slices": N}` |
| Stage set cleaned | stage_set | cleaned | `{"freed_bytes": N}` |
| Write started | write | started | `{"volume": "L6-0015", "stage_set_id": N}` |
| Write completed | write | completed | `{"slices_written": N, "bytes": N, "eot_recovery": null}` |
| Write interrupted | write | interrupted | `{"slices_written": N, "slices_remaining": N}` |
| Volume initialized | volume | created | `{"label": "...", "backend": "lto"}` |
| Volume moved | volume | moved | `{"from": "home-rack", "to": "parents-house"}` |
| Volume retired | volume | retired | `{"affected_units": [...]}` |
| Cartridge registered | cartridge | created | `{"barcode": "...", "media_type": "..."}` |
| Cartridge erased | cartridge | erased | `{"previous_volume": "...", "load_count": N}` |
| Cartridge retired | cartridge | retired | `{"reason": "...", "total_bytes_lifetime": N}` |
| Key generated | key | created | `{"tenant": "...", "alias": "...", "type": "primary"}` |
| Key deactivated | key | deactivated | `{"alias": "..."}` |
| Tenant added | tenant | created | `{"name": "..."}` |
| Tenant reassigned | tenant | reassigned | `{"units_moved": N, "target": "..."}` |
| Snapshot reclaimable | snapshot | marked_reclaimable | `{"version": N, "superseded_by": N}` |
| Compaction read | volume | compact_read | `{"slices_read": N, "bytes": N}` |
| Compaction finished | volume | compact_finish | `{"freed_cartridge": "..."}` |
| DB backup | system | db_backup | `{"path": "...", "size_bytes": N}` |

### E.3 What NOT to Log

- Read-only operations (list, search, status queries)
- Failed validation checks (user errors, not state changes)
- Individual file entries during snapshot create (too verbose; log at snapshot level)

---

## Appendix F: Typical Session Workflow

A realistic evening session demonstrating the natural flow:

```bash
# Register new content
tapectl unit init-bulk /media/tv/new-show/ --tenant mike --tag tv
tapectl snapshot create --dirty
tapectl stage create --unstaged
# (go eat dinner while dar runs)

# Write to two tapes
tapectl volume write L6-0015
# (swap tape)
tapectl volume write L6-0016

# Clean up and track
tapectl staging clean
tapectl volume move L6-0016 --to parents-house

# Check compliance
tapectl audit
```

Re-staging workflow (two weeks later, need another copy):

```bash
tapectl volume plan --copies 3 --copy-c-location safe-deposit
# Plan reports which snapshots need staging
tapectl stage create "tv/new-show/s01"
tapectl volume write L6-0020
tapectl staging clean
```

---

## Appendix G: DB Backup Strategy

`tapectl db backup` includes keys + catalogs + DB, encrypted with a
passphrase. The backup should be stored in multiple locations:

1. On a separate physical device (USB drive in a safe)
2. On a remote location (S3, remote server, email to self)
3. On every tape as part of the volume envelope (already designed)

The volume envelopes already contain a DB subset + catalogs. Combined
with key backups stored separately, this provides tape-only disaster
recovery without needing the DB backup file. The `tapectl db backup`
command is for convenience/completeness, not as the sole recovery path.

---

*End of tapectl Design Document v4.0*
