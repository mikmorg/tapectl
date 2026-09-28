# tapectl Operator Guide

This is the manual for running an installed tapectl: registering what you want
archived, writing it to tape, keeping enough copies in enough places, checking
that they are still readable, and getting data back. It is organised by task,
and each section shows the command and, where it helps you recognise success,
the output a real session printed.

**Where else to look.** Installing tapectl on a machine is
[install.md](install.md) — this guide assumes an install already exists. The
vocabulary (unit, snapshot, stage set, volume, cartridge, copy, escrow
recipient, …) is explained in [concepts.md](concepts.md); every
`config.toml` key is in [configuration.md](configuration.md); keys, the escrow
secret, the Heir Kit and recovery are gathered in
[keys-and-recovery.md](keys-and-recovery.md); error messages and what to do
about them are in [troubleshooting.md](troubleshooting.md). For a first guided
pass through the whole cycle, read [walkthrough.md](walkthrough.md). Every flag
of every command is in the [command reference](cli/README.md). The index of all
documentation is [docs/README.md](README.md).

> [!NOTE]
> Commands are written `tapectl …`. An install made with
> `scripts/first-run.sh` runs tapectl as a dedicated service user, `tapectl`,
> so on such a machine type the same command as `tapectl-op …` (the installed
> wrapper) or `sudo -u tapectl -H tapectl …`. Examples use
> `TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst` for the drive; `ls -l /dev/tape/by-id/`
> shows yours. `/dev/nstN` numbers are not stable across reboots, so never use
> them in scripts.

## Contents

- [Initial Setup](#initial-setup)
  - [The service user](#the-service-user)
  - [Initialize tapectl](#initialize-tapectl)
  - [Configure](#configure)
  - [Working on a different archive](#working-on-a-different-archive)
- [Day-to-Day Operations](#day-to-day-operations)
  - [A typical write session](#a-typical-write-session)
  - [Register units](#register-units)
  - [Archive to tape](#archive-to-tape)
  - [A quiet host while the tape runs](#a-quiet-host-while-the-tape-runs)
  - [Check what's pending](#check-whats-pending)
  - [Restore](#restore)
  - [Search the catalog](#search-the-catalog)
- [Safety Operations](#safety-operations)
- [Policy and Compliance](#policy-and-compliance)
- [Warehouse Copies (Cold Cloud)](#warehouse-copies-cold-cloud)
- [Cadence](#cadence)
- [Compaction](#compaction)
- [Cartridge Tracking](#cartridge-tracking)
- [Key Management](#key-management)
- [Database Operations](#database-operations)
- [Disaster Recovery](#disaster-recovery)
- [Multi-Tenant Setup](#multi-tenant-setup)
- [Testing with a virtual tape library](#testing-with-a-virtual-tape-library)

## Initial Setup

> [!IMPORTANT]
> Install with `scripts/first-run.sh`. It does everything in this section
> interactively, in order, explains each step, detects what is already done,
> and rehearses on a test cartridge before your first real write.
> [install.md](install.md) is its runbook: what each step creates on the host,
> how to resume, re-run, move to a new host and uninstall. The text below
> summarises what the result looks like, for reference.

### The service user

A production install runs tapectl as a dedicated nologin account, `tapectl`,
not as your login. The reason is what the tapectl home holds — the operator
private key, every tenant private key, the catalog — and the fact that tapectl
finds that home purely from `$HOME`: under its own account those files share a
home with nothing else, and the systemd timers get the fixed `User=` and
`HOME=` they need. Tenants are key domains, not Unix accounts; one service
user reads every tenant's data and encrypts each to its own key.

`first-run.sh` creates the account (home `/var/lib/tapectl`, so the tapectl
home is `/var/lib/tapectl/.tapectl`), adds it to the group that owns the drive
nodes, gives it the staging directory, and installs the wrapper
`/usr/local/bin/tapectl-op`, which is simply
`exec sudo -u tapectl -H /usr/local/bin/tapectl "$@"`. A restore destination
must be writable by `tapectl`; restore itself needs no root (dar runs with
`-O`), and restored files are owned by the service user.
`first-run.sh --no-service-user` keeps a run-as-yourself layout for a
single-user machine.

The service user needs **read** on every tree it archives and **write** on
each unit's top directory (for the `.tapectl-unit.toml` dotfile). The installer
grants this for the trees you name then; for a tree you add later, grant it
yourself with POSIX ACLs (package `acl`):

```bash
sudo setfacl -R -m u:tapectl:rX /data/alice/photos      # read the whole tree
sudo setfacl -R -d -m u:tapectl:rX /data/alice/photos   # files created later inherit it
sudo setfacl -m u:tapectl:rwX /data/alice/photos        # write the dotfile at the top
sudo setfacl -m u:tapectl:x /data /data/alice           # traverse each ancestor
```

### Initialize tapectl

`first-run.sh` step 7 runs this for you; by hand it is:

```bash
tapectl init --operator mike
```

`--operator` names the **operator tenant**, a label in the catalog, not a Unix
account (it defaults to the name of the user running `init`). `init` creates
that tenant with a primary and a backup key, so do not `tenant add` it
afterwards — that fails because the name is taken. It also creates the
database, the config file, the `keys/` directory, and the permanent **escrow
recipient**, whose secret it prints exactly once:

```text
================================================================================
  ESCROW IDENTITY GENERATED -- THIS SECRET IS SHOWN EXACTLY ONCE, RIGHT NOW
================================================================================
  ...
  SECRET -- transcribe this line:

    AGE-SECRET-KEY-1…(redacted)

  Public key (already saved to disk and the database -- safe to keep there):

    age1zczqp0emrmylxcetrrz5k8xu5j097dlptp4eysje0vgu496mgc5qaq0nmp
================================================================================

tapectl initialized at ~/.tapectl
  operator: mike
  database: ~/.tapectl/tapectl.db
  config:   ~/.tapectl/config.toml
  escrow:   age1zczqp0emrmylxcetrrz5k8xu5j097dlptp4eysje0vgu496mgc5qaq0nmp
            (the SECRET was printed above — transcribe it now onto paper; ADR-0005)
  dar:      dar (found at /usr/bin/dar)
```

> [!WARNING]
> The escrow secret is stored nowhere on disk. Copy it onto paper before you
> do anything else, and keep that paper in two independent places. Why, and
> what it unlocks, is in [keys-and-recovery.md](keys-and-recovery.md).

On a **reinstall**, do not let `init` mint a new escrow identity: adopt the
original with `tapectl init --escrow-public-key age1…` (see
[Disaster Recovery](#disaster-recovery)).

### Configure

The config file is `~/.tapectl/config.toml` (under the service user's home on
a production install). Register the drive with `backend add` rather than by
editing the file — `first-run.sh` step 8 does this, reading the generation from
the drive's own identification:

```bash
tapectl backend add --name lto6 \
    --device-tape /dev/tape/by-id/scsi-<SERIAL>-nst \
    --device-sg /dev/sgN --generation LTO-6
```

A drive declares only **what generation it is** (ADR-0010). It does not
declare a media type or a capacity: the generation of each cartridge is
detected from the medium at `volume init`, and that tape's capacity follows
from it. `capacity_override` exists for virtual drives and test harnesses
only. There is no `backend edit`; to change a backend, edit its
`[[backends.lto]]` table and run `tapectl config check`.

A minimal hand-written config looks like this (every key, its default and what
it does are in [configuration.md](configuration.md)):

```toml
[dar]
binary = "/usr/bin/dar"    # Path to dar binary

[[backends.lto]]
name = "lto-primary"
device_tape = "/dev/tape/by-id/scsi-XXXXXXXX-nst"   # by-id: /dev/nstN moves across reboots
device_sg = "/dev/sg1"      # also moves; tapectl re-checks that it names the same drive
generation = "LTO-6"        # what the DRIVE is, not what you feed it
# capacity_override = "2400M"   # virtual drives and test harnesses only

[staging]
directory = "/mnt/staging"  # Needs space for dar + encrypted slices

[defaults]
slice_size = "10G"
min_copies_for_tape_only = 2
min_locations_for_tape_only = 2

[discovery]
watch_roots = ["/media/tv", "/media/movies"]
```

Unknown keys are errors, everywhere. Validate with:

```bash
tapectl config check
```

```text
config: valid
dar: 2.7.13 at '/usr/bin/dar' (meets minimum 2.6)
staging: '/srv/staging' exists and is writable
host check (defaults, no [host_check] table): units none; processes cargo, rustc, docker, Runner.Worker; max load 1.00/CPU; min available 2048 MiB; max memory pressure 10.00%; max I/O pressure 10.00% — `tapectl host check` runs it
```

With exactly one drive configured, every `--device` flag may be left out: it
resolves to that drive. With more than one, `--device` is required on every
command that touches a tape.

### Working on a different archive

Everything lives under `~/.tapectl` by default. To operate on a different one
— a copy on an external disk, a restored catalog, a scratch archive for a
drill — pass `--home` (or export `TAPECTL_HOME`):

```bash
tapectl --home /mnt/usb/archive report copies
TAPECTL_HOME=/mnt/usb/archive tapectl audit
```

`--config` points at a config *file*. On its own it **also** relocates the
whole home to that file's parent directory — database, keys, catalogs,
receipts — which is surprising, and it warns when you do it. It still works,
because scripts and test harnesses have long relied on it for isolation, and
silently repointing them at the real archive would be worse than the
surprise. Use `--home` to choose the archive and `--config` only to name a
file inside it.

## Day-to-Day Operations

### A typical write session

Tape is write-once in tapectl: each `volume write` fills one freshly
initialised cartridge in a single session and seals it, and nothing is ever
appended to a sealed tape. So work comes in batches: snapshot and stage what
changed, then write the batch to as many cartridges as your policy wants
copies (two, by default). This is the whole sequence, copy-pasteable. The
output shown is from a real session with four small units and a policy of two
copies.

```bash
TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst

# 0. Is the host quiet, and what changed?
tapectl host check
tapectl report dirty

# 1. Snapshot (fast metadata walk) and stage (dar + encrypt) each unit
tapectl snapshot create family/letters
tapectl stage create family/letters

# 2. What will go on tape, and how many cartridges?
tapectl volume plan --copies 2

# 3. First copy: load a blank cartridge
tapectl volume init L8-0001 --device "$TAPE"
tapectl volume write L8-0001 --device "$TAPE"
tapectl volume verify L8-0001 --device "$TAPE"
tapectl volume move L8-0001 --to home-rack

# 4. Second copy: swap in another blank cartridge
tapectl volume init L8-0002 --device "$TAPE"
tapectl volume write L8-0002 --device "$TAPE"
tapectl volume verify L8-0002 --device "$TAPE"
tapectl volume move L8-0002 --to offsite

# 5. Check, release staging, refresh the Heir Kit
tapectl audit
tapectl report copies
tapectl staging clean
tapectl key escrow-kit --out ~/heir-kit
```

What success looks like at each step:

```text
$ tapectl snapshot create family/letters
snapshot created: family/letters v1 (2 files, 224 B)

$ tapectl stage create family/letters
staged: family/letters (1 slices, 1.1 KiB dar, 1.7 KiB encrypted)

$ tapectl volume plan --copies 2
volume write plan (2 copy/copies):
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  family/letters v1: 1 slices, 1.7 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB x 2 = 3.4 MiB
estimated tapes: 1 (at 92% usable capacity)

$ tapectl volume init L8-0001 --device "$TAPE"
cartridge E01001L8_1775794348 auto-registered from MAM (barcode = medium serial)
volume "L8-0001" initialized (id=1)

$ tapectl volume write L8-0001 --device "$TAPE"
about to write to volume "L8-0001":
  family/letters v1: 1 slices, 1.7 KiB
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB
volume "L8-0001" write completed

$ tapectl volume verify L8-0001 --device "$TAPE"
verify L8-0001 (full tier): 13 checked, 13 passed, 0 failed

$ tapectl volume move L8-0001 --to home-rack
volume "L8-0001" moved to "home-rack"
  cartridge "E01001L8_1775794348" moved with it
```

`volume write` ends by reading the whole tape back against its front index
(the write's own full verification appears in `volume info`'s history), so the
separate `volume verify` is a second, independent read-back. Here `audit`
after the first copy correctly reports that every unit is one copy short —
that is the reminder to write the second:

```text
$ tapectl audit
VIOLATIONS (4):
  [copy_count] family/letters: has 1 copies, needs 2
  [copy_count] family/photos/2019-italy: has 1 copies, needs 2
  [copy_count] family/photos/2020-garden: has 1 copies, needs 2
  [copy_count] work/invoices-2024: has 1 copies, needs 2
WARNINGS (2):
  [compaction_candidate] volume:L8-0001: utilization 49% < 50% threshold
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 4 violations, 2 warnings (exit 2)
```

And after the second copy is written and moved offsite:

```text
$ tapectl audit
WARNINGS (3):
  [compaction_candidate] volume:L8-0001: utilization 49% < 50% threshold
  [compaction_candidate] volume:L8-0002: utilization 49% < 50% threshold
  [escrow_kit_missing] archive: 2 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 0 violations, 3 warnings (exit 1)

$ tapectl report copies
  family/letters: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  family/photos/2019-italy: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  family/photos/2020-garden: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  work/invoices-2024: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]

$ tapectl staging clean
cleaned 4 stage set(s), 4 files removed, 2.1 MiB freed
  sessions: 2 reclaimed, 0 retained, 0 orphaned; 4 lockfiles reclaimed

$ tapectl key escrow-kit --out ~/heir-kit
heir kit written to ~/heir-kit
  COVER.txt        the printable cover sheet (print this)
  escrow-kit.html  same content with a QR, for a browser's print dialog
  catalog.db.age   encrypted catalog, 668082 bytes, covering 2 sealed volume(s)
  ...
```

(The compaction warnings are expected for tiny test data; a full cartridge
does not trip them.)

If you archive whole folders of similar things — one unit per subfolder of a
media root — a [Collection](cli/collection.md) (`collection sync`,
`collection plan`, `collection run`) does steps 1–4 for you, and
`quick-archive` registers, snapshots, stages and writes a single directory
onto a volume you have already initialised.

### Register units

A **unit** is one directory tracked as a whole. Its name is derived from its
path, with a leading `/media/` or `/mnt/` dropped (`/media/tv/breaking-bad/s01`
becomes `tv/breaking-bad/s01`); `--name` overrides it.

```bash
# Single directory
tapectl unit init /media/tv/breaking-bad/s01 --tenant mike --tag tv

# Every immediate subdirectory becomes its own unit (s01, s02, …)
tapectl unit init-bulk /media/tv/breaking-bad --tenant mike --tag tv

# Re-register units whose .tapectl-unit.toml dotfiles already exist under watch_roots
tapectl unit discover
```

```text
$ tapectl unit init-bulk /media/family/photos --tenant family --tag photos
  ok: /media/family/photos/2020-garden (id=2)
  ok: /media/family/photos/2019-italy (id=3)
2 units created, 0 skipped
```

`unit init` writes a `.tapectl-unit.toml` dotfile into the directory, which is
why the service user needs write on each unit's top directory. `unit discover`
does not invent new units; it finds directories that already carry a dotfile,
registering any the catalog does not know and updating the path of any that
moved (for example after the catalog was lost, or on a new host).

### Archive to tape

`tapectl init` creates the permanent escrow recipient (ADR-0005) for you and
prints its secret once — **transcribe that secret onto paper then, and store
it in two independent places** (it is stored nowhere on disk). The Heir Kit
you generate later prints only the *public* half; its cover sheet has a box
marked "WRITE IT HERE" for this secret, and without the secret the kit opens
nothing. The escrow recipient must exist **before the first `stage create`**,
because slices are encrypted at stage time and an escrow registered afterwards
cannot open them; `stage create` and `volume write` both refuse without one. If
you initialized with `--no-escrow` (to adopt an existing identity), register
it now with `tapectl key generate --escrow` or
`tapectl key import --escrow <age1...>` before staging.

```bash
# Step 1: Snapshot (fast directory walk)
tapectl snapshot create tv/breaking-bad/s01

# Step 2: Stage (dar archive + encrypt — needs staging disk space)
tapectl stage create tv/breaking-bad/s01

# Step 3: Write to tape (a blank or erased cartridge)
tapectl volume init L6-0001 --device "$TAPE"
tapectl volume write L6-0001 --device "$TAPE"

# Step 4: Verify
tapectl volume verify L6-0001 --device "$TAPE"
```

**`volume write` writes everything still staged, not just what you staged in
this sitting** — and it says so before it touches the drive:

```text
about to write to volume "L6-0001":
  tv/breaking-bad/s01 v1: 3 slices, 42.0 MiB
  photos/2019 v2: 1 slices, 8.0 MiB

total: 4 slices, 50.0 MiB
```

That is by design, not a bug: staging happens once and a stage set stays
`staged` until `staging clean` releases it, which is what lets you write the
same data to a second and third cartridge without re-archiving it (ADR-0006).
The consequence is that a stage set left over from last week rides along on
today's tape. **Read the list.** This is write-once media — a tape that
received four units when you meant one cannot be un-written, and under ADR-0003
it cannot even be overwritten without a real erase.

The announcement is display only. It does not prompt, and there is no flag to
select a subset: if the list is not what you want, stop, run `staging clean` to
release what is already safely on tape, and start again. (`volume write` may
still ask one question — the quiet-host check below — which `--yes` answers.)

`staging clean` releases every unit that has met its policy's `min_copies` and
**retains** the ones that have not, naming them (ADR-0012). So a unit still
short of its second copy keeps its staged bytes — the release cannot quietly
discard the cheap route to a copy your own policy requires — while everything
already safely on tape is freed. `--unit` (and `--version`) narrow what is
considered. `--force` releases the retained ones too, and is wider than the
gate it overrides: it also drops staged data for sets never written to any
tape.

### A quiet host while the tape runs

An LTO-6 drive streams at up to 160 MB/s and *stops and restarts* whenever the
host feeds it slower than about 54 MB/s. Every restart costs tape as well as
time: on a real HP LTO-6 a bursty feed used 1.48 bytes of tape per byte of
data, a steady one 1.00
([measurement](runs/2026-09-23-lto6-capacity-measurement.md)). `volume write`
and `volume verify` each move every byte of the tape through this machine, for
as long as the data takes — hours for a full cartridge.

So, before a write or a verify, make the host quiet:

- Pause anything that competes for CPU, memory or the staging disk: CI runners
  and their systemd timers, container builds, other backups, test suites.
- Do not start heavy disk I/O on the filesystem that holds staging.
- Nothing else touches the drive. tapectl itself takes no lock on the device;
  the repository's scripts (`first-run.sh` step 13 and the test harnesses) take
  `/tmp/tapectl-tape.lock` and refuse a second user, but a command you type by
  hand does not check it.
- Memory matters too: a process killed for memory pressure mid-write costs the
  cartridge its session (a clean abort to an unsealed tape, but the time is
  gone). Keep a few GB free.

The rule is checked, not only stated (ADR-0012). `tapectl host check` reports
the load average, available memory, memory and I/O pressure
(`/proc/pressure/*`, `full avg60`), and any contender process or systemd unit,
each against its limit; it exits 0 when the host is quiet and 1 when anything
tripped, and runs on a machine not yet `init`ed:

```text
$ tapectl host check
load average       3.78 over 16 CPUs = 0.24/CPU             max 1.00/CPU
available memory   7709 MiB                                 min 2048 MiB
memory pressure    0.01% full avg60                         max 10.00%
I/O pressure       0.23% full avg60                         max 10.00%
processes          cargo, rustc, docker, Runner.Worker      contender_processes
units              (none listed)                            contender_units
host check: quiet — nothing listed above is running or over its limit
```

`volume write` runs the same check after its own fact checks and before it
touches the drive (so do `volume compact-write`, `collection run` and
`quick-archive`, which write through it): when anything trips it prints each
finding — what, what was measured, the limit — and asks. `--yes` answers the
question (the findings are still printed); a session with no terminal and no
`--yes` declines, as every ADR-0008 Tier-2 question does, rather than hang. It
never refuses on its own account: a busy host is a warning you may accept, not
a fact that stops the write.

The limits live in `[host_check]` in config.toml (`init` writes the table
commented out, every key at its default):

| key | default | trips when |
|---|---|---|
| `contender_units` | `[]` | a listed systemd unit is active (a timer is active while armed) |
| `contender_processes` | `["cargo", "rustc", "docker", "Runner.Worker"]` | a process of that exact name (`/proc/<pid>/comm`) runs |
| `max_load_per_cpu` | `1.0` | 1-minute load / CPU count exceeds it |
| `min_available_mb` | `2048` | `MemAvailable` is below it (0 = off) |
| `max_memory_pressure_pct` | `10.0` | memory `full avg60` exceeds it |
| `max_io_pressure_pct` | `10.0` | I/O `full avg60` exceeds it |

The default processes are builds and CI jobs by the name they run under while
working — not the daemons that idle beside them (`dockerd`, `buildkitd`, the
Actions runner's `Runner.Listener`), which would trip every check on a normal
host. Unit names are host-specific, so none are listed by default. If this
host runs timers that should not fire during a write, list them so
`volume write` checks them too:

```toml
[host_check]
contender_units = ["nightly-ci.timer", "photo-sync.timer"]
```

and pause them for the write with `sudo systemctl stop <unit>…` (`start` them
again afterwards). `tapectl host check --unit <unit>` checks a unit once
without editing the config; `first-run.sh` step 13 does exactly that for the
host profile's `CONTENDER_UNITS` before its WRITE confirmation.

The write itself needs no supervision once it is streaming; the drive's
counters afterwards (`report health`, which compares the tape the drive
actually used against the bytes written) say whether the feed held.

### Check what's pending

```bash
tapectl stage list --status staged
tapectl volume plan --copies 2
tapectl report pending
```

`volume plan` estimates against the drive's own generation; pass
`--generation LTO-5` to count cartridges of another generation that drive can
write.

### Restore

`restore` takes everything as flags: `--unit`, the volume to read `--from`,
and the destination `--to` (plus `--file` for a single file). `--device` may
be left out when only one drive is configured.

```bash
# Full unit
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE"

# Single file (path within the unit)
tapectl restore file --file 1998-letter-to-mum.txt --unit family/letters \
  --from L8-0002 --to /tmp/restore --device "$TAPE"

# Dry run
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE" --dry-run
```

```text
$ tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE" --dry-run
would restore "family/letters" v1 from L8-0002 (1 slices) to /tmp/restore/unit

$ tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE"
2026-09-28T21:44:10.997985Z  WARN tapectl::dar::restore: restoring as a non-root user: restored files will be owned by the invoking user, not their archived owners
restored "family/letters" v1 from L8-0002 (1 slices) to /tmp/restore/unit

$ diff -r /media/family/letters /tmp/restore/unit && echo identical
identical
```

The `WARN` line is expected when not running as root (which includes the
service user): the files come back, but owned by the restoring user rather
than their archived owners. `--version N` restores an older snapshot; by default the newest version
of the unit on that volume is used. `catalog locate` (below) tells you which
volumes to restore `--from`.

### Search the catalog

```bash
tapectl catalog search "episode01"
tapectl catalog ls family/letters
tapectl catalog locate family/letters
tapectl catalog stats
```

`catalog locate` is the one to know: every copy of a unit, where it is, and
whether it can be relied on.

```text
$ tapectl catalog locate family/letters
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| Volume  | Status | Condition | Location  | Snapshot | Slices | Written             | Serviceable | Warehouse | Escrow | Verified |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0001 | sealed | ok        | home-rack | 1        | 1      | 2026-09-28 21:44:05 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0002 | sealed | ok        | offsite   | 1        | 1      | 2026-09-28 21:44:09 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
```

"Verified" is the catalog's record of the last *passed* `volume verify` of that
copy, not a check made just now.

## Safety Operations

### Locations and Movement

```bash
tapectl location add home-rack --description "Home server rack"
tapectl location add parents-house --description "Offsite backup"
tapectl volume move L6-0001 --to parents-house
```

A location is either a `shelf` (the default: somewhere a cartridge sits) or a
`warehouse` (cold cloud storage — see below). Moving a volume moves its
cartridge with it, and `cartridge move` does the same from the other end.

### Copy Management

```bash
# Check copy counts
tapectl report copies
tapectl report fire-risk
```

```text
$ tapectl report fire-risk
fire-risk: all units meet their resolved minimum copy requirements
```

The normal way to make another copy is to write the still-staged data to
another cartridge (`volume init` + `volume write`, as in
[A typical write session](#a-typical-write-session)). Once staging has been
cleaned, copy from an existing tape instead: read its encrypted slices back
into staging, then write them to a new cartridge with the full self-describing
layout.

```bash
# Read slices from tape into staging (source cartridge loaded)
tapectl volume read-slices --from L6-0001 --unit tv/breaking-bad/s01 --device "$TAPE"
# Swap in a blank cartridge, then write
tapectl volume init L6-0002 --device "$TAPE"
tapectl volume write L6-0002 --device "$TAPE"
```

### Mark Tape-Only

When local disk copies are no longer needed:

```bash
# Check the local files still match what was staged
tapectl unit check-integrity tv/breaking-bad/s01

# Then mark it tape-only (enforces min_copies_for_tape_only and min_locations_for_tape_only)
tapectl unit mark-tape-only tv/breaking-bad/s01
```

`mark-tape-only` refuses while the unit has fewer copies or locations than the
tape-only minimums (2 and 2 by default); `--force` overrides that, and should
not be needed.

### Retire a Volume

```bash
# Shows impact analysis: which units lose copies
tapectl volume retire L6-0001
```

See [Before moving a tape](#before-moving-a-tape) for what the impact analysis
shows.

## Policy and Compliance

### Archive Sets

An archive set is a named policy (minimum copies, required locations, verify
interval, …) that units can be assigned to (`unit init --archive-set`). Policy
resolves in three layers: the unit's dotfile, then its archive set, then
`[defaults]` in config.toml.

```bash
# Create a policy template
tapectl archive-set create critical-media \
  --min-copies 3 \
  --required-locations "home-rack,parents-house" \
  --verify-interval-days 180

# Import the archive sets defined in config.toml
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

Exit codes: 0 = clean, 1 = warnings, 2 = violations. The audit is advisory: it
changes the exit code and nothing else, and never blocks a command.
`--action-plan` adds the command that fixes each finding:

```text
$ tapectl audit --action-plan
VIOLATIONS (4):
  [copy_count] family/letters: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
  ...
WARNINGS (2):
  [compaction_candidate] volume:L8-0001: utilization 49% < 50% threshold
    fix: tapectl volume compact-read L8-0001
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
    fix: tapectl key escrow-kit --out <dir>
audit: 4 violations, 2 warnings (exit 2)
```

### Reports

```bash
tapectl report summary
tapectl report fire-risk
tapectl report copies --unit family/letters
tapectl report tape-only
tapectl report capacity --per-volume
tapectl report compaction-candidates
tapectl report events --days 30
```

```text
$ tapectl report summary
tapectl summary
  Tenants:    3
  Units:      4 active
  Snapshots:  4
  Volumes:    2 active
  Writes:     8 completed
  Total data: 7.0 MiB on tape
  Pending:    4 stage set(s) awaiting write
```

The full list of reports is in the [command reference](cli/report.md).

## Warehouse Copies (Cold Cloud)

A **warehouse** is a location kind (ADR-0006) that holds cold cloud storage —
S3 Glacier / Deep Archive and equivalents — rather than physical cartridges.
It sits alongside your shelves in the same location list, and a copy recorded
there counts toward `min_copies` and toward distinct-location counts exactly
like a tape does.

**tapectl does not upload anything.** The scope was settled deliberately: you
move the bytes yourself with the documented external procedure (`rclone` or
`aws-cli` against the sealed volume), and then you *record* that copy in the
catalog so every derivation — `audit`, `report fire-risk`, `report copies`,
the retire and mark-tape-only gates — can reason about it. There is no
upload command, no polling, and no credentials anywhere in the config.

### Creating a warehouse location

The endpoint or prefix goes in the description. There is no separate URI
field, on purpose.

```bash
tapectl location add glacier --kind warehouse \
  --description "s3://my-archive-bucket/tapectl"

tapectl location list
tapectl location info glacier
```

### The deposit procedure

tapectl does not run any of step 2 for you — it produces the bytes and records
the result.

> [!NOTE]
> Every `tapectl`, `age`, `dar`, `tr` and `sha256sum` invocation in this
> section and the next was run against a real dumped volume, and the outputs
> quoted are the real ones. The `rclone` and `aws` lines were **not** run when
> this was written — they are transcribed from vendor documentation and are
> the one part of this procedure to re-check against your own installed
> version before you rely on it. Do a dry run against a scratch bucket before
> your first real deposit; `rclone copy --dry-run` will tell you if a flag has
> moved.

**1. Dump the sealed volume to a directory.** `restore raw-volume` reads a
tape using only what is on the tape itself, verifies every file against the
front index as it goes, and needs no database:

```bash
tapectl restore raw-volume --device "$TAPE" --to /staging/MHVTLR3 \
  --from MHVTLR3
```

`--from` is a wrong-tape guard against the tape's own reported label, not a
catalog lookup. Expect output like `dumped 23 files … verified: 21
mismatched: 0 unverifiable: 2`. **`mismatched` must be 0.** The two
unverifiable files are always the front index and the seal marker — neither
can carry its own hash — so `2` is the correct number, not a warning.

Files land named `{position:04}_{type}.bin`, e.g.:

```text
0000_id_thunk.bin           0006_operator_envelope.bin
0001_system_guide.bin       0007_operator_envelope_backup.bin
0002_restore_sh.bin         0008_data_slice.bin  …  0021_data_slice.bin
0003_front_index.bin        0022_seal_marker.bin
0004_tenant_envelope.bin
0005_tenant_envelope.bin
```

**2. Upload, splitting hot from cold.** ADR-0006's zone split is the whole
point: metadata at instant-access, slices at deep-archive. Everything that is
*not* a `data_slice` is metadata and must stay instantly readable — an
operator who cold-stores the front index cannot even enumerate what they
deposited without paying for a restore request first, which defeats the split.

```bash
# Metadata zone -> instant access. Small: a few hundred KB in total.
rclone copy /staging/MHVTLR3 s3:my-archive-bucket/MHVTLR3/ \
  --exclude '*_data_slice.bin' --s3-storage-class STANDARD

# Slices -> deep archive. This is the volume's bulk.
rclone copy /staging/MHVTLR3 s3:my-archive-bucket/MHVTLR3/ \
  --include '*_data_slice.bin' --s3-storage-class DEEP_ARCHIVE
```

The `aws-cli` equivalent is `aws s3 cp --storage-class …` over the same two
file sets. Either way, **verify before you record**: `rclone check
/staging/MHVTLR3 s3:my-archive-bucket/MHVTLR3/ --checksum` compares hashes
rather than sizes. Record the deposit only after that passes.

**3. Record it**, per the next section.

### Recording a deposit

Copy the sealed volume's bytes out first, by your own procedure, then:

```bash
tapectl volume deposit add L6-0003 --to glacier \
  --receipt <provider-object-version-id> \
  --storage-class DEEP_ARCHIVE \
  --notes "rclone copy, 2026-01-02"

tapectl volume deposit list
tapectl volume deposit list --volume L6-0003
```

`deposit add` refuses two things and nothing else: a location that is not a
warehouse, and a volume that is not `sealed` (unsealed bytes are not final, so
there is nothing durable to have deposited). There is deliberately **no
checksum field** — tapectl did not perform the copy, so a checksum you typed
in would be a claim about a claim. What gets recorded is what is actually
attestable: which volume, which warehouse, when, and the provider's receipt.

### When a deposit is gone

Nothing tells tapectl that a cloud object was deleted — a lapsed bill or a
provider lifecycle rule removes it silently, and the row would keep counting
as a copy at the two gates that decide whether local data may be deleted. So
when you find a deposit is gone, un-record it:

```bash
tapectl volume deposit remove L6-0003 --from glacier
```

It errors rather than shrugging if no such deposit was recorded, so a typo in
the label cannot look like a successful correction.

### Getting the bytes back

Assume the tapes are gone and this warehouse copy is all that is left. That is
the only scenario this path exists for, so it is written for someone who has
tapectl's key files and nothing else — an heir, or you after a fire.

**1. Issue a restore request and wait.** Deep-archive objects cannot be read
until the provider stages them, and that wait is measured in **hours**, not
minutes — standard retrieval from Deep Archive is on the order of 12 hours,
bulk up to 48. Nothing you can do shortens it, and the metadata zone is at
instant access precisely so you can read the front index and decide *which*
slices to pay to thaw before you start that clock.

```bash
# Metadata is already instant — fetch it first and read it.
rclone copy s3:my-archive-bucket/MHVTLR3/ /recover/MHVTLR3/ \
  --exclude '*_data_slice.bin'

# Then thaw the slices and wait.
aws s3api restore-object --bucket my-archive-bucket \
  --key MHVTLR3/0008_data_slice.bin \
  --restore-request Days=7,GlacierJobParameters={Tier=Standard}
# ... repeat per slice, then poll:
aws s3api head-object --bucket my-archive-bucket \
  --key MHVTLR3/0008_data_slice.bin --query Restore
```

Once `Restore` reports `ongoing-request="false"`, download normally.

**2. Read the front index.** It is plain text, but it is dumped as a **full
tape block**, so it has a large tail of NUL padding — strip it before reading:

```bash
tr -d '\0' < /recover/MHVTLR3/0003_front_index.bin | less
```

Content files (envelopes, slices) are dumped at their exact length and need no
stripping. Only the front index and seal marker carry padding, because they
are the two files that cannot carry their own hash.

**3. Open an envelope by trial decryption.** Try each key you hold against
each envelope; the one that works is yours. A tenant key opens only that
tenant's envelope, and the operator key opens every one:

```bash
age -d -i ~/.tapectl/keys/YOURKEY.age.key \
  < /recover/MHVTLR3/0004_tenant_envelope.bin | tar -xv
```

You get `MANIFEST.toml`, `RECOVERY.md`, and the dar catalogs.

**4. Map slices using MANIFEST.toml — not the filename order.** Each unit's
`[[units.slices]]` block gives both `number` (dar's slice number) and
`tape_position` (which dumped file holds it). **These are not the same, and
slices do not start at position 0** — on a real volume they typically start
after the envelopes. Decrypt each slice into a working directory named by its
`number`:

```toml
[[units.slices]]
number = 1
tape_position = 8
sha256_plain = "0c507128…"
```

```bash
mkdir -p /recover/dar
age -d -i ~/.tapectl/keys/YOURKEY.age.key \
  < /recover/MHVTLR3/0008_data_slice.bin > /recover/dar/restore.1.dar
# verify against sha256_plain from the manifest:
sha256sum /recover/dar/restore.1.dar
```

**5. Extract.** dar takes the base name, with no `.N.dar` suffix:

```bash
dar -x /recover/dar/restore -R /destination -O -Q
```

This whole path was verified end-to-end against a real dumped volume: the
decrypted slice matched its manifest `sha256_plain`, and the extraction was
`diff -r`-identical to the original source tree.

### Asking for warehouse copies by policy

`warehouse_copies` resolves through the usual three layers — unit dotfile
`[policy]` > archive set > `[defaults]` in `config.toml` — and defaults to 0,
so an all-tape fleet never sees a warehouse finding.

```bash
tapectl archive-set create irreplaceable --min-copies 2 --warehouse-copies 1
tapectl archive-set edit irreplaceable --warehouse-copies 2
```

`tapectl audit` then reports a `warehouse_copies` VIOLATION for any unit with
fewer recorded deposits than its policy asks for, with the `volume deposit
add` command as its action. Like every other audit finding it is advisory: it
changes the exit code and nothing else.

### The honest caveats

Read these before you treat a warehouse copy as equivalent to a tape.

**It is never re-verified.** Tape evidence comes from physically loading the
cartridge and running `volume verify`, and it refreshes every time you do.
Warehouse evidence is the deposit receipt plus the provider's attestation, and
it ages without refresh — re-verification would mean paying to retrieve the
whole volume, which realistically never happens. tapectl says so out loud
wherever coverage is consumed:

```text
coverage for unit "photos" rests on a warehouse deposit of L6-0003 at glacier
(2026-01-02) — never re-verified, and warehouse copies do not refresh
```

`report copies` and `report fire-risk` likewise print how many of a unit's
copies are deposits rather than folding them into one number.

**It dies weeks after payment stops.** ADR-0006 states it plainly: a warehouse
copy dies weeks after payment stops; tapes are the durable line. A card
expiring, an account lapsing, or a billing dispute silently deletes every copy
you have there, on a timescale of weeks. A cartridge in a drawer does not care
whether you paid anyone this month. Treat warehouse copies as the extra leg
the irreplaceable core earns — never as the primary line, and never as a
reason to retire a tape.

**You cannot read it today.** A tape is slow; a deep-archive object is
*unavailable* until you ask for it and wait hours (12 for standard retrieval,
up to 48 for bulk). If someone needs a file back this afternoon, a warehouse
copy cannot give it to them and a cartridge can. Budget the wait into any
recovery plan that starts from the warehouse, and thaw only the slices the
manifest says you need — see "Getting the bytes back" above.

> [!NOTE]
> **The Heir Kit carries these caveats.** The two above — the billing
> fragility and the retrieval wait — are exactly what an heir needs, because
> they will meet this copy with no context at all. The printed cover sheet
> says both, in plain language, and only when the catalog actually records a
> deposit (a tape-only archive gets no cloud paragraph). See
> [The Heir Kit](#the-heir-kit) below.

**A deposit stops counting when its source volume does.** Deposits are gated
on the source volume still being an eligible copy — `sealed`, and not
quarantined by a failed verify (its `status` and its `observed_condition` are
separate fields) — so quarantining or retiring the cartridge also removes its
deposit from every count, even though the cloud object itself is unaffected.
That is the conservative reading, chosen so a deposit can never be the thing
that keeps a unit looking covered after its tape went bad.

## Cadence

Everything below is a **manual rhythm you run**. tapectl schedules nothing and
has no daemon or listener, and that is permanent: every cost of a server exists
to serve multi-machine access this system does not have. The read-only
advisory half *can* be put on a systemd timer — see
[Scheduling the advisory half](#scheduling-the-advisory-half) below — but that
is a wrapper around the same manual commands, not a daemon. `volume write`
stays manual forever, because it needs a human and a physically-present
cartridge.

The two operations that need no tape in the drive are the ones worth doing
often, because they cost nothing but attention:

```bash
tapectl audit               # 0 = clean, 1 = warnings, 2 = violations
tapectl report verify-status
```

### Weekly — cheap, no tape

Run `tapectl audit`. It runs eleven compliance checks, including copy count,
location presence, and **verification age against each unit's resolved
`verify_interval_days`**. That last check is what produces your "what is
overdue" list — you do not track it yourself. Per ADR-0004 it is advisory: it
warns, it never blocks, and a stale volume still counts as a copy.

`tapectl report dirty` and `tapectl report pending` are the companion glance —
what has drifted since its last snapshot, and what is staged but not yet on
tape.

### Scheduling the advisory half

The weekly glance is the one part of the cadence a machine can do for you,
because it is read-only and needs no tape in the drive. A `first-run.sh`
install already has it: step 14 runs `scripts/install-systemd.sh`, which
installs two timers from `contrib/systemd/` — a weekly audit and a daily
catalog backup — plus the `tapectl-op` wrapper. To install or re-render them on
their own:

```bash
scripts/install-systemd.sh --dry-run                        # print the plan, change nothing
scripts/install-systemd.sh --backup-dir /mnt/backup/tapectl # install (or re-render) for this host
```

Re-run the script rather than editing the installed units by hand;
[install.md §7](install.md#7-the-timers-and-the-operator-wrapper) lists its
options (`--user`, `--home` for a run-as-yourself install, and so on), the
schedules, and the backup retention. To check them:

```bash
systemctl list-timers --all 'tapectl-*'
sudo systemctl start tapectl-audit.service   # run once now to check it works
journalctl -u tapectl-audit.service -n 50
```

The audit wrapper runs `tapectl audit` followed by `tapectl report
verify-status`, and its exit status is `audit`'s:

| exit | meaning | unit result |
|---|---|---|
| 0 | clean | success |
| 1 | warnings only | **success** (`SuccessExitStatus=1`) |
| 2 | violations | failure |

Warnings are not a failure on purpose. `audit` warns for ordinary drift — an
overdue verification, a unit one copy short — and ADR-0004 is explicit that the
audit advises and never blocks. A timer that alerts on exit 1 would turn the
advisory audit into a blocking one by the back door.

`report verify-status` always exits 0; it runs for the journal record, not as a
check. The verification-age *check* with real exit codes is one of `audit`'s
checks, so nothing is lost.

Set `TAPECTL_HEALTHCHECK_URL` in the service to ping a healthchecks.io-style
endpoint (`/start` before, bare URL on 0 or 1, `/fail` on 2). It is off unless
set, and a missing `curl` or a failed ping never changes the run's own result.

Two things the timers do **not** change:

- **They never write.** No tape command is scheduled, ever. The services set
  `PrivateDevices=true` so they cannot reach `/dev/nst*` even by mistake.
- **They are safe to fire during other work, with one cosmetic caveat.**
  Opening the database runs the startup sweep, which marks an `in_progress`
  write session `interrupted` — so an audit or backup landing in the middle of
  a `volume write` produces a spurious "recovered orphaned write sessions"
  event. The session stays fully resumable and revalidates on resume, so
  nothing is lost, but prefer a schedule outside your usual write window to
  keep the event log honest.

### Monthly — verify a rotating slice of the library

Do **not** try to verify every volume every month. The rotation exists to bound
drive and tape wear, which is the same reason snapraid scrubs ~8% of an array
per pass rather than all of it. tapectl has no percentage selector and computes
no rotation for you — this is a human procedure driven by one report:

```bash
tapectl report verify-status                     # verification recency, oldest first
tapectl volume verify L6-0003 --device "$TAPE"   # --full is the default
```

Pick the N oldest-evidence volumes such that **every volume gets one full pass
within its `verify_interval_days`** — if you hold 24 volumes on a 12-month
interval, that is roughly 2 per month. `verify_interval_days` resolves through
the usual three layers (dotfile > archive set > defaults) and is set with
`tapectl archive-set edit <name> --verify-interval-days N`.

Two tiers, and the distinction matters:

| Tier | Cost | What it proves |
|---|---|---|
| `volume verify --full` (default) | Reads and hashes every content file | Media still returns the exact bytes the front index recorded |
| `volume verify --quick` | Seal binding + front index self-consistency only | Tape is still *navigable* — nothing about content integrity |

`--quick` is a triage tool for a tape you are about to move or a suspicion you
want to rule out fast. It is not a substitute for a full pass, and a `--quick`
run should not reset your sense of when that volume was really verified.

### Annually — the heir-path restore drill

The drill that matters is not "can tapectl restore this" — it is **can someone
who is not you, without this database and without this binary, get the data
back**. Run it from real media once a year.

The procedure already exists in
[`docs/lto6-validation-checklist.md`](lto6-validation-checklist.md) — see its
*Raw-recovery drill* section. Follow it there rather than a second copy here;
two drifting checklists are a failure waiting to happen. The drill's
essentials: load a real tape, pull `RESTORE.sh` off the plaintext front zone
with `mt` + `dd`, and run it using **only** `mt`, `dd`, `age`, `dar`, and
`sha256sum` — no `tapectl` — against nothing but the printed key material.

Nothing automated performs a drill; schedule it yourself, on real hardware.

While you are there, confirm your off-tape recovery inputs are current:

```bash
tapectl db backup --to /path/to/backup        # add --include-keys only if the
                                              # destination is treated as secret
tapectl key list --tenant <name>              # find the aliases, then
tapectl key export <alias>                    # public half, for the record
```

### The Heir Kit

`key escrow-kit` generates the kit (ADR-0005 / ADR-0009). What it is for, and
how an heir uses it, is in [keys-and-recovery.md](keys-and-recovery.md); this
is the operator's routine.

```bash
tapectl key escrow-kit --out ~/heir-kit
```

It writes three files:

| file | what to do with it |
|---|---|
| `COVER.txt` | **Print this.** The plain-text cover sheet. It carries the escrow *identity* (the public half, `age1…`, in retypable Bech32) and a boxed hand-fill area for the escrow *secret* (`AGE-SECRET-KEY-1…`), which `init` printed once and nothing on the machine holds. The printed `age1…` decrypts nothing by itself; the sheet says so. It is the artifact with the decades-scale claim — readable with `cat` when no browser exists. |
| `escrow-kit.html` | Same content with the identity as a QR, captioned as the identity. Open it and use the browser's print dialog. Self-contained: it renders with no network. |
| `catalog.db.age` | The whole catalog, encrypted to the escrow recipient. Put it on the media that travels with the paper. |

**The command stops at the files. The rest is yours, and the kit is worth
nothing until you do it:**

1. print `COVER.txt` (and/or the HTML page);
2. **copy the escrow secret by hand into the box marked "WRITE IT HERE"** —
   the kit prints only the public half; without the secret it opens nothing.
   Write in CAPITALS (age rejects a lowercased secret), then check the pair
   as the sheet describes: put the secret alone on one line of a file and
   `age-keygen -y <file>` must print exactly the `age1…` on the sheet;
3. seal into a **tamper-evident envelope**;
4. store copies in **at least two independent failure domains** — not two
   shelves in one building;
5. paper in a UL-350 safe; Class-125 if stored together with tape.

**Re-run it after every write session.** A kit made before newer tapes were
written still opens the older ones and silently misses the new — which is the
failure mode ADR-0005 names explicitly. You are not expected to remember:
`audit` warns (`escrow_kit_stale`, or `escrow_kit_missing` if you have sealed
tapes and no kit at all) whenever volumes were sealed after the last
generation. It is advisory and never blocks — exit 1, never 2.

The bundle deliberately contains the **whole** `tapectl.db`, not the filtered
subset that rides on tape: without `locations` and `cartridges` an heir would
learn what the archive holds but not which cartridge to fetch or where it is.
It is safe to escrow because the database stores no private keys — only public
halves and fingerprints; every secret is a file under `keys/`.

### Before moving a tape

`volume move` records a location change; it does not inspect the cartridge.
Check coverage **before** the tape leaves, not after:

```bash
tapectl report copies --unit <name>    # does anything depend on this tape alone?
tapectl report verify-status --volume <label>
tapectl volume move <label> --to <location>
tapectl cartridge info <barcode>       # physical cartridge, tracked separately
```

`--to` must be a **shelf** location. A warehouse destination is refused:
a volume's location records where to go to *fetch* the cartridge, and the
answer can never be an S3 bucket. To record that a copy of a volume's bytes
was uploaded to cold storage, use `volume deposit add` instead — a deposit is a
separate fact, because a cartridge has one location while a volume can carry
several deposits and a deposit never moves.

`volume retire` shows an impact analysis of which units lose copies, and — for
every affected unit that still retains coverage after the retirement — which
volume that remaining coverage rests on and how old its last passing
verification is (ADR-0004 Tier 1). This appears in the plain-text impact
analysis, in `--json` (an `evidence` array plus an `evidence_summary` string
per affected unit), and again in the Tier-2 consent prompt when some *other*
unit in the same retirement is genuinely at risk. It is advisory and never
blocks — a stale or missing verification never stops the retirement, it just
tells you what you're trusting. `report verify-status` before the fact remains
the right way to check coverage across the fleet, not just at the one volume
you're about to retire.

## Compaction

When tapes become underutilized (snapshots superseded and marked reclaimable),
copy their still-live slices to a new cartridge and retire the old one. The
destination must be a volume already initialised with `volume init`.

```bash
# Check candidates
tapectl report compaction-candidates

# Mark old snapshots as reclaimable (enforced preconditions)
tapectl snapshot mark-reclaimable tv/breaking-bad/s01 --version 1

# Three-step compaction, on one drive
tapectl volume compact-read L6-0001 --device "$TAPE"          # source loaded
# (swap in a blank cartridge)
tapectl volume init L6-0010 --device "$TAPE"
tapectl volume compact-write --destination L6-0010 --device "$TAPE"
tapectl volume compact-finish L6-0001                         # retires the source

# Or all three in one flow
tapectl volume compact L6-0001 --device "$TAPE"
```

`volume compact` reads the source, then prompts "Insert destination tape and
enter volume label" — load the already-initialised destination there and type
its label. `--to <label>` skips the prompt (for a non-interactive run), which
also means there is no pause to swap cartridges: use it only when the
destination is already the tape the drive will write. Step 3 (retiring the
source) asks for consent; `--force` or `--yes` answers it, and if it declines,
the destination is already written and sealed —
`volume compact-finish <source> --force` completes the job.

## Cartridge Tracking

**A cartridge is known by the serial its chip reports. A barcode is a sticker**
(ADR-0012). The serial is burned into the cartridge's memory chip at the
factory, tapectl reads it through the drive, and it never changes. The barcode
is whatever label you choose to put on the shell, and you can change it any time
without changing which cartridge it is.

That ordering is what makes the rest simple: **do not register a cartridge
before writing to it.** `volume init` reads the chip and registers the cartridge
itself.

### The sequence, end to end

Load a new tape and initialise a volume on it. You register nothing first:

```bash
tapectl volume init L8-0001 --device "$TAPE"
```

`volume init` reads the chip, finds no cartridge registered under that serial,
and registers one — using the serial itself as a placeholder barcode, because
that is the only identifier the tape carries:

```text
cartridge E01001L8_1775794348 auto-registered from MAM (barcode = medium serial)
volume "L8-0001" initialized (id=1)
```

Now put a sticker on the shell and tell the catalog what it says. Before or
after the first write — it makes no difference:

```bash
tapectl cartridge relabel E01001L8_1775794348 L8-0001
# cartridge "E01001L8_1775794348" relabelled to "L8-0001"
```

One cartridge row, with your barcode on it and the chip serial underneath as its
identity. The volume stays bound throughout: relabelling changes the label, not
the cartridge.

> [!WARNING]
> **Do not `cartridge register --barcode <sticker>` first and then write.**
> `volume init` matches on the chip serial, finds no row carrying it, and
> registers a *second* cartridge. You end up with this:
>
> ```text
> | Barcode             | Type  | Status    | Location | Loads   | Volume  |
> | E01001L8_1775794348 | LTO-8 | in_use    |          | 3       | L6-0009 |
> | L6-0009             | LTO-8 | available |          | unknown |         |
> ```
>
> Two rows for one physical tape — and the barcode you chose is on the one the
> catalog is *not* using. Let `volume init` register it, then relabel.

### When the drive reads no serial

Some drives — and some virtual libraries — report no medium serial at all. Then
tapectl cannot tell which cartridge is loaded, and it will not guess:

```bash
tapectl volume init L6-0002 --device "$TAPE"
# refused: this drive reports no medium serial, so tapectl cannot tell which
# physical cartridge is loaded. Name it: ... --cartridge <barcode>
```

Register the cartridge yourself and name it. On this path the barcode you give
**is** the recorded identity, and the tape records that fact — File 0 carries
`cartridge_identity_source = "operator"` rather than `"mam"`, so anyone reading
the tape later (you, or an heir with no catalog) can tell a chip-verified serial
from a label somebody typed:

```bash
tapectl cartridge register --barcode L6-0002 --generation LTO-6
tapectl volume init L6-0002 --device "$TAPE" --cartridge L6-0002
```

One safeguard on this path: if the barcode you name is still carrying a live
volume, tapectl refuses rather than displacing it. With no serial it cannot tell
whether that cartridge was erased or whether you have loaded a different tape
wearing its sticker. Say the bytes are gone first — `volume retire <volume>` or
`cartridge mark-erased <barcode>` — and run it again. There is no `--force`;
this is something tapectl cannot know, not a risk for you to accept.

### Everything else

```bash
tapectl cartridge list                      # barcode, generation, status, location, volume
tapectl cartridge list --location offsite-vault
tapectl cartridge info L6-0001
tapectl cartridge edit L6-0001 --generation LTO-5  # the registration was wrong about the medium
tapectl cartridge relabel L6-0001 L6-0001-B  # the sticker changed; identity did not
tapectl cartridge move L6-0001 --to offsite-vault   # the cartridge and every volume on it
tapectl cartridge retire L6-0001            # worn out or too many errors: never write it again
tapectl cartridge unretire L6-0001          # you were wrong about the medium; undo the retire
tapectl cartridge mark-erased L6-0001       # after a physical erase
```

```text
$ tapectl cartridge list
+---------------------+-------+--------+-----------+-------+---------+
| Barcode             | Type  | Status | Location  | Loads | Volume  |
+---------------------+-------+--------+-----------+-------+---------+
| E01001L8_1775794348 | LTO-8 | in_use | home-rack | 541   | L8-0001 |
+---------------------+-------+--------+-----------+-------+---------+
```

**`unretire` and `mark-erased` are different statements, and the difference
matters.** `cartridge retire` is a claim about the *medium*: this plastic is
worn out, never write it again. If that claim was wrong, `cartridge unretire`
withdraws it and puts the cartridge and its volumes back the way they were,
reading their prior statuses out of the audit trail. It is a Tier-1 correction
(ADR-0008): no prompt, no `--force`, because correcting a claim destroys
nothing.

`cartridge mark-erased` says something else entirely — *the bytes are gone* —
and marks every volume on the cartridge `erased` to match. Using it to undo a
mistaken retire would cost you the catalog's record that those tapes held data,
which is why the refusal you get when you try to write a retired cartridge
names `unretire`.

Reach for `mark-erased` when you have actually erased the tape. Reach for
`unretire` when you simply changed your mind.

**`cartridge edit --generation` is the third correction in that family**, and
the same logic places it: it is a claim about *what medium this is*. If a
cartridge was registered as LTO-6 when the plastic is LTO-5, a real
`volume init` reads the density code, disagrees with the row, and refuses; the
refusal names this repair. It is Tier 1 for the same reason as `unretire`:
correcting a fact destroys nothing, so there is no prompt and no `--force`, and
it works on every status including `retired_permanent`.

It edits the cartridge row and nothing else. A volume already written to that
cartridge keeps the capacity it was planned against — ADR-0010 decides capacity
once, at `volume init`, and stores it on the volume. If the corrected generation
now disagrees with a volume still mounted on the cartridge, the command says so
and changes nothing about that volume; the warning is there so you know the
tape was planned against a figure you have just called wrong.

Capacity follows the correction **only when you never chose it yourself**. If
the stored figure is exactly the old generation's table value, it was a default
and gets re-defaulted to the new generation's; if it differs — you passed
`--capacity`, or a virtual drive's `capacity_override` was baked in — it is left
alone. Either way the command tells you which happened and why, so you never
have to infer it.

If the catalog was rebuilt since the retirement, the events rows that recorded
the prior statuses may be gone. `unretire` then restores the cartridge to
`available`, leaves the volumes exactly as they are, and tells you which ones it
could not restore — an honest partial restore rather than a guess.

**A cartridge's place is a location, never a status** (ADR-0011). "Offsite" is
a location you named with `location add`, and `cartridge move` puts the
cartridge and all its volumes there in one step, so the shelf and the catalog
cannot drift apart. `volume move` does the same from the other end.

**Mixing LTO generations.** An LTO-6 drive writes LTO-5 and LTO-6 media, so one
drive can hold both. Declare the drive once as `generation = "LTO-6"`; tapectl
reads each cartridge's own generation from the medium at `volume init` and
plans that tape against that generation's capacity — 1.5 TB for LTO-5, 2.5 TB
for LTO-6 (decimal terabytes, as printed on the cartridge). There is nothing to
change between tapes and nothing to remember. If you load media the drive
cannot write, `volume init` refuses before touching the tape, and no `--force`
overrides it.

`first-run.sh` fills `generation` in from the drive's own INQUIRY product
identification — never from whatever cartridge happens to be loaded, which is a
fact about that tape and not about the drive. If it is ever wrong (you moved
the config to a different drive, or declared it by hand), the refusal above
names the `[[backends.lto]]` block to fix: there is no `backend edit`, so edit
`generation` in config.toml and run `tapectl config check`.

## Key Management

Each tenant has its own age keys (a primary and a backup, created with the
tenant); the operator's keys and the escrow recipient are recipients of every
tape as well. Private keys are files under `~/.tapectl/keys/`; the database
holds only public halves and fingerprints. The full story — rotation, what
each key opens, backing keys up, the escrow secret — is in
[keys-and-recovery.md](keys-and-recovery.md).

```bash
tapectl key list --tenant mike
tapectl key generate --tenant mike --alias 2026-primary
tapectl key rotate --tenant mike
tapectl key export mike-primary > mike-primary.age.pub
```

```text
$ tapectl key list --tenant family
+----------------+---------+--------+--------+-----------------------------+---------------------+
| Alias          | Type    | Active | Escrow | Fingerprint                 | Created             |
+----------------+---------+--------+--------+-----------------------------+---------------------+
| family-backup  | backup  | yes    |        | age1p39zlwg4wf57xv2kk7dj... | 2026-09-28 21:43:58 |
+----------------+---------+--------+--------+-----------------------------+---------------------+
| family-primary | primary | yes    |        | age1guqq2ueeq3x6mddm4qwm... | 2026-09-28 21:43:58 |
+----------------+---------+--------+--------+-----------------------------+---------------------+
```

Old keys are never deleted — only deactivated. Restore tries all known keys.
`key rotate` refuses until an escrow recipient is registered, and never touches
the escrow key itself.

## Database Operations

```bash
tapectl db backup --to /backup/tapectl.db
tapectl db fsck --repair
tapectl db stats
tapectl db export > tapectl-catalog.json   # every table, as JSON
```

```text
$ tapectl db backup --to /tmp/tour/backup/tapectl.db
database backed up to /tmp/tour/backup/tapectl.db (private keys not included — pass --include-keys to copy them)
```

A `first-run.sh` install already backs the catalog up daily to a second disk
([install.md §7](install.md#7-the-timers-and-the-operator-wrapper)); run the
backup service by hand after a write session too.

`db fsck` runs SQLite's `integrity_check` and `foreign_key_check` and reports
every violation it finds. `--repair` deletes rows whose foreign-key parent is
missing, closing the graph in one transaction, and logs what it deleted — if
anything is still dangling at commit the whole repair rolls back, so a failed
repair leaves the database exactly as it was.

Run it on a database you brought in with `db import` before you use it:
`db import` is a raw page copy and validates no foreign keys of its own.

## Disaster Recovery

Every tape is self-describing, so the database being gone costs you convenience,
not data. There are two ways back, and which one you take depends on what you
hold — not on how bad the loss is. This section is the procedure;
[keys-and-recovery.md](keys-and-recovery.md) explains which key opens what,
and [install.md §8](install.md#8-getting-the-catalog-back) covers the cheapest
case, restoring a catalog from a `db backup` copy.

> [!IMPORTANT]
> Every device below is written `$TAPE`. Find the drive by serial with
> `ls -l /dev/tape/by-id/` and set `TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst`
> first — `/dev/nstN` numbering is not stable across reboots on a host with
> more than one SCSI device, and a wrong device reads a different tape.

### If you hold a tenant key: restore directly, no catalog needed

This is the heir path. It needs `mt`, `dd`, `age`, `dar` and `sha256sum` — and
notably not `tapectl`.

1. Read the ID thunk (tape file 0), which says what the tape is and how to
   read the rest. With tapectl installed, `tapectl volume identify --device "$TAPE"`
   prints it; without:
   ```bash
   mt -f "$TAPE" setblk 524288 && mt -f "$TAPE" rewind
   dd if="$TAPE" bs=512k | tr -d '\0' | less
   ```
2. Extract RESTORE.sh from the tape (file position 2):
   ```bash
   mt -f "$TAPE" setblk 524288
   mt -f "$TAPE" rewind && mt -f "$TAPE" fsf 2
   dd if="$TAPE" bs=512k | tr -d '\0' > RESTORE.sh
   chmod +x RESTORE.sh
   ```
3. Use RESTORE.sh for guided recovery. It reads the drive named by
   `TAPE_DEVICE` (and falls back to `/dev/nst0` without it), so always set it:
   ```bash
   export TAPE_DEVICE="$TAPE"
   ./RESTORE.sh --info                                          # tape layout and seal verdict
   ./RESTORE.sh --verify                                        # keyless integrity check
   ./RESTORE.sh --find-envelope --key your.age.key              # find your data
   ./RESTORE.sh --restore --unit UNIT --key your.age.key --to /dest
   ```

   **Bring every key you hold, not just the current one.** `--key` may be
   repeated, and each key is tried on its own for the envelope and for every
   slice. A tape's envelope is sealed with the key that was active when the
   volume was *written*, while its slices were sealed when the data was
   *staged* — so a `key rotate` between those two moments leaves no single key
   that opens both, and the fix is to pass both.

### If you hold the operator or escrow key: rebuild the catalog

There are two sources — the heir kit's catalog bundle, and the tapes themselves
— and the procedure uses both, **in this order**. The order is not a preference:
`db import` replaces the *entire* live database, so importing the bundle after
rebuilding tapes would silently discard everything you just rebuilt.

Run these four steps in sequence on the rebuilt machine. (On a fresh
production host, `scripts/first-run.sh` step 7 asks for the original escrow
public key and does step 1 for you.)

**1. Register the original escrow identity.**

```bash
tapectl init --escrow-public-key age1…        # the ORIGINAL, from the kit's cover sheet
```

```text
$ tapectl --home ~/.tapectl-rebuilt init --operator mike --escrow-public-key age1zczqp0emrmylxcetrrz5k8xu5j097dlptp4eysje0vgu496mgc5qaq0nmp
tapectl initialized at ~/.tapectl-rebuilt
  operator: mike
  ...
  escrow:   adopted age1zczqp0emrmylxcetrrz5k8xu5j097dlptp4eysje0vgu496mgc5qaq0nmp (imported — its secret lives on the heir kit, not here)
```

Every escrow check compares against the escrow recipient this catalog has
*registered*. A plain `init` registers a brand-new one, and every tape was
encrypted to the old one — so until the original is registered, everything
reads `NO: encrypted without the current escrow recipient`, `volume write`
refuses to re-copy, and nothing can be attested. `audit` reports this
(`escrow_identity_mismatch`, naming the key). The equivalent two-step form
still works, if you prefer it:

```bash
tapectl init --no-escrow                      # do NOT let init mint a new escrow identity
tapectl key import --escrow age1…             # the ORIGINAL escrow public key, from the cover sheet
```

**If you already ran a plain `init`:** there is only ever one escrow identity
(ADR-0005) and no command replaces it — neither `key import --escrow` nor
`init --escrow-public-key` will adopt one while a (wrong) one is already
registered. The new home holds nothing yet, so remove it and start again. Do
this *before* restoring or rebuilding anything into it.

**2. Put the private keys back.** The database holds public keys and
fingerprints only; every private half is a file under `~/.tapectl/keys/`
(ADR-0009 — which is what makes the bundle safe to carry). `db import` restores
rows, not secrets, so copy the key files from your copy of the old home into
`~/.tapectl/keys/` now, or pass them explicitly with `--key` at each step
below. Skipping this leaves a catalog that describes data you cannot decrypt
with the tenants' own keys (the escrow secret still opens every tape).

**3. Import the heir kit's database.** `key escrow-kit` bundled the **whole**
`tapectl.db` at generation time (ADR-0009) — every row, every escrow receipt —
as `catalog.db.age`, encrypted to the escrow key. Decrypt it, then import:

```bash
age -d -i escrow.age.key -o catalog.db catalog.db.age
tapectl db import catalog.db
tapectl db fsck
```

`escrow.age.key` is a file you make now: the escrow *secret*
(`AGE-SECRET-KEY-1…`, 74 characters, CAPITALS) typed alone on one line, from
the hand-filled box on the kit's cover sheet — the kit itself prints only the
public half. Check it before using it: `age-keygen -y escrow.age.key` must print
exactly the `age1…` on the sheet; a miscopied character is refused with
`invalid checksum`. The same file is the `--key` for `catalog rebuild` and for
`RESTORE.sh`.

`db import` asks for confirmation first, because it **overwrites the entire
live database** with the file you name. That is what you want here and exactly
why this step precedes the rebuild. It prints `database imported from
catalog.db`.

**4. Rebuild every tape sealed since the kit was made.** `audit`'s
`escrow_kit_stale` check names exactly that set.

```bash
tapectl catalog rebuild --from-volume \
    --device "$TAPE" \
    --key ~/.tapectl/keys/operator-primary.age.key \
    --label VOL0001            # optional wrong-tape guard
```

```text
rebuilt from volume "L8-0002" (uuid a5379113-e90f-4e97-86c5-66368cb6c406), 3 envelope(s) opened
  inserted: 2 tenant(s), 4 unit(s), 4 snapshot(s), 4 stage set(s),
            4 slice(s), 4 write(s), 4 position(s), 11 file row(s), 1 volume
  cartridge "E01003L8_1775794348" registered from this tape's own identity and bound
  the slice hashes recorded are the tape's own claim; run `tapectl volume verify L8-0002` to check them
  escrow receipts: 4 stage set(s) carried theirs on the tape
```

Run it once per cartridge, in any order. It only ever **inserts what is
missing** and never edits a row it finds, so running it twice — or over a
catalog that is damaged rather than absent — is safe.

**How to tell it worked.** A row reconstructed from tape rather than recorded
at staging is marked `stage_sets.origin = 'rebuilt'` (the default is
`'staged'`) — the receipt was demonstrated, not recorded at staging. What that
means for escrow coverage, and the two ways to resolve it, is the
*Escrow coverage on rebuilt rows* note below.

What comes back, and from where:

| | source |
|---|---|
| volume identity, media, capacity | the ID thunk (tape file 0) |
| units, snapshots, slice map, plaintext hashes | each envelope's `MANIFEST.toml` |
| tenant ownership, escrow receipts, the per-file index | the operator envelope's `catalog.db` (tapes written after 2026-09-11); older tapes give ownership from the tenant envelopes and no receipt |
| which cartridge this is, and the volume bound to it | File 0's `[media]` table — the chip serial when `cartridge_identity_source = "mam"`, the barcode you typed when `"operator"` |

**Cartridge identity comes back too, but only as far as the tape can prove it.**
A rebuild registers the cartridge and records the mount, so `cartridge list`,
`cartridge move` and `cartridge retire` work on a recovered tape instead of
reporting "not found". What it will *not* do is guess: a File 0 that names a
barcode rather than a chip serial proves only that somebody typed that label,
so a rebuild refuses to displace a live volume on that evidence — retire the
volume, or `cartridge mark-erased` it, and run the rebuild again. A tape older
than this field (its File 0 has no `[media]` table) binds nothing and says so
in the report, and **re-running the rebuild will never change that**, on any
drive, however well it reads the medium serial: the rebuild trusts only what
File 0 records, and a sealed tape can never gain that table (ADR-0003). Record
the cartridge by hand instead — `cartridge register`, then `volume move` to
place the volume — or accept the volume as unbound and expect `audit`'s
location check to keep saying so.

**Escrow coverage on rebuilt rows.** A tape written after 2026-09-11 carries
each stage set's recipient list, so its rebuilt rows are covered like any
other. An older tape does not, and those rows show **`?`** in
`catalog locate` — *unknown*, not covered: `audit` warns, and `volume write`
still needs `--allow-missing-escrow` to re-copy them. Two ways to resolve it:

- **Attest.** Run the rebuild again with the escrow key itself:
  `catalog rebuild --from-volume --device "$TAPE" --key <escrow secret key>`.
  If that key is the one this catalog has registered, the command decrypts one
  slice *header* per stage set — a few hundred bytes, not the slice — and
  records the coverage it just proved. Rows it cannot open stay `?`.
- **Re-stage** the unit to a new tape, which records a fresh receipt.

Rebuilt units come back as `active`, so `audit` sees them. Expect it to start
reporting the truth immediately — a single cartridge is one copy, and if your
policy asks for two, that is a violation it should be telling you about:

```bash
tapectl audit                       # exit 2 on a one-copy rebuild is correct
tapectl volume verify VOL0001 --device "$TAPE" --full
```

Two things the rebuild deliberately does not do:

- **It does not verify the tape.** The hashes it records are the tape's own
  claim about itself. `volume verify` turns that into a checked claim.
- **It refuses a tenant key**, because a tenant key cannot open the operator
  envelope. That is not a restriction on the tenant: they do not need a catalog
  at all, and the refusal points them at RESTORE.sh above.

A tape written before the on-tape catalog existed carries no `catalog.db`. The
restore path still comes back whole; what you lose is `catalog ls`/`catalog
search` and each snapshot's original source path, and the command says so when
it happens.

## Multi-Tenant Setup

A tenant is a key domain: each has its own keys, and data staged for one
tenant is readable only with that tenant's keys (or the operator's and escrow
keys). The operator tenant already exists — `init` created it — so add only
the others:

```bash
tapectl tenant add alice --description "Alice's media"
tapectl tenant add bob --description "Bob's documents"

# Each tenant gets independent encryption keys
# Tenant A cannot see Tenant B's data on shared tapes
# Operator can always decrypt everything

# Move every unit of one tenant to another
tapectl tenant reassign alice --to bob
```

```text
$ tapectl tenant list
+--------+--------+----------+---------------------+---------------------------+
| Name   | Status | Operator | Created             | Description               |
+--------+--------+----------+---------------------+---------------------------+
| family | active |          | 2026-09-28 21:43:58 | family photos and letters |
+--------+--------+----------+---------------------+---------------------------+
| mike   | active | yes      | 2026-09-28 21:43:58 | System operator           |
+--------+--------+----------+---------------------+---------------------------+
| work   | active |          | 2026-09-28 21:43:58 | business records          |
+--------+--------+----------+---------------------+---------------------------+
```

Units of different tenants share tapes. Each tenant's envelope and slices on
the tape are encrypted to that tenant's keys (plus the operator and escrow
recipients), so nothing about one tenant's content is readable with another
tenant's key.

## Testing with a virtual tape library

[mhvtl](https://github.com/markh794/mhvtl) emulates a tape library and drives
in the kernel, which lets you rehearse every tapectl command — including
`volume write` and a full restore — without real hardware or real cartridges.
The project's own end-to-end tests use it. This section is about running it;
none of it is needed for production.

**Install.** mhvtl is not packaged by most distributions; build it from source:

```bash
sudo apt install lsscsi mt-st sg3-utils mtx
git clone https://github.com/markh794/mhvtl.git && cd mhvtl
make && sudo make install          # userspace daemons + systemd units
```

Register the kernel module with DKMS, or every kernel update silently removes
it (the module is gone, no `/dev/nst*` appears, and gated tests skip quietly):
copy `kernel/` and `include/` to `/usr/src/mhvtl-<ver>/` with a `dkms.conf`,
then `dkms add`, `dkms build` and `dkms install -m mhvtl -v <ver>`.

**After any kernel update**, confirm with `dkms status`. If the virtual drives
are missing, check `lsmod | grep mhvtl`, then `systemctl start mhvtl.target`.
SCSI enumeration can shuffle across module reloads — find the changer with
`lsscsi -g` (look for `mediumx`) rather than assuming a `/dev/sgN`, and load a
cartridge the drive can write (an L6 tape for a TD6 drive:
`mtx -f <changer-sg> load <slot> <dte>`). The repository's
`scripts/mhvtl-device.sh --tape <by-id path> --ensure-media` does that
discovery and loading for you.

**Registering a virtual drive.** Use its by-id path like a real one, and give it
a `capacity_override` so tapectl plans against the small virtual tape rather
than the generation's real capacity:

```bash
tapectl backend add --name lto8 --device-tape /dev/tape/by-id/scsi-XYZZY_A1-nst \
    --device-sg /dev/sg5 --generation LTO-8 --capacity-override 2G
```

```text
backend "lto8" added to ~/.tapectl/config.toml (LTO-8, tape=/dev/tape/by-id/scsi-XYZZY_A1-nst, sg=/dev/sg5)
verify it with: tapectl config check
```

**Keep the virtual tapes off a small root partition.** mhvtl stores tape
images under `/opt/mhvtl` by default, and each grows to the library's
configured `CAPACITY`; a handful of full tapes can fill `/` and break the
machine. To move them to a larger filesystem (here `/data/mhvtl`):

```text
/data/mhvtl                         # actual tape images
/opt/mhvtl -> /data/mhvtl           # symlink (see "why a symlink" below)
/etc/mhvtl/mhvtl.conf               # MHVTL_HOME_PATH=/data/mhvtl
/etc/mhvtl/device.conf              # ' Home directory: /data/mhvtl' (in EVERY library stanza)
```

**Why a symlink is required, not just config.** `MHVTL_HOME_PATH` is a
**compile-time `#define`** in the mhvtl userspace binaries, not a runtime
setting — `strings /usr/bin/vtltape` contains the fully-formed message
`Unable to change directory to /opt/mhvtl`, and `vtltape` accepts no `-H`
override (unlike `mktape`). Each `vtltape` daemon `chdir()`s to that baked-in
path at startup and **exits 255 if it does not exist**, no matter what the
config files say. `vtllibrary` is unaffected (it does not chdir), so the
symptom is: libraries start, every tape daemon fails, no `/dev/nst*` appears.
Editing both config files is still necessary — that is what actually relocates
the media — but the symlink is what lets the daemons start.

**Verifying the relocation really took effect** (a clean `systemctl start` is
not sufficient evidence — the symlink means either path resolves):

```bash
# Load a tape, then confirm the daemon's open files are under the new path:
for p in $(pgrep -f 'vtltape -F'); do sudo ls -l /proc/$p/fd; done | grep mhvtl
# Expect: /data/mhvtl/<PCL>/data.0 — never /opt/mhvtl/...
```

**Reinstall/upgrade risk:** `make install` or a package upgrade may rewrite
`/etc/mhvtl/*.conf` back to `/opt/mhvtl` and could replace the symlink with a
real directory. After any mhvtl reinstall, re-check all three items above —
otherwise tapes silently start filling `/` again with no error until the disk
is full.

Note: loading a tape rewrites its `mam` and `mhvtl_data` files (mount counter,
last-mount timestamp) while `data.0`/`indx.0`/`meta.0` stay byte-identical —
expected, not corruption, when comparing tape trees before and after a mount.

> [!NOTE]
> **For the maintainers' machines.** The development VM's mhvtl setup, its
> `first-run.sh` rehearsal profile and the production host's profile and
> preparation script are in [`contrib/hosts/`](../contrib/hosts/)
> (`vm-desk1-mhvtl.profile`, `home2.profile`, `home2-prep.sh`). Running the
> gated test suites against mhvtl is described in the repository's
> `CLAUDE.md` ("Testing").
