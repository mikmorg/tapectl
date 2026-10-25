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
> wrapper) or `sudo -u tapectl -H tapectl …`. With one drive configured, tapectl
> finds it, so the examples leave `--device` out ([when it is
> needed](#configure)). Where a drive is named for `mt` or another tool, or in
> [Disaster Recovery](#disaster-recovery), the examples write it as
> `TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst`; `ls -l /dev/tape/by-id/` shows
> yours. `/dev/nstN` numbers are not stable across reboots, so never use them
> in scripts.

## Contents

- [Initial Setup](#initial-setup)
  - [The service user](#the-service-user)
  - [Initialize tapectl](#initialize-tapectl)
  - [Configure](#configure)
  - [Working on a different archive](#working-on-a-different-archive)
- [Day-to-Day Operations](#day-to-day-operations)
  - [A typical write session](#a-typical-write-session)
  - [Watching a long operation: progress and the session log](#watching-a-long-operation-progress-and-the-session-log)
  - [Watching from another account](#watching-from-another-account)
  - [Register units](#register-units)
  - [Archive to tape](#archive-to-tape)
  - [A quiet host while the tape runs](#a-quiet-host-while-the-tape-runs)
  - [Check what's pending](#check-whats-pending)
  - [Restore](#restore)
  - [Search the catalog](#search-the-catalog)
- [Safety Operations](#safety-operations)
- [Policy and Compliance](#policy-and-compliance)
  - [Scripting against tapectl](#scripting-against-tapectl)
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
`/usr/local/bin/tapectl-op`, which is
`sudo -u tapectl -H /usr/local/bin/tapectl "$@"` — except that a tape command
(`volume write`, `resume`, `verify`, `cartridge erase`, `collection run`, …)
runs at raised CPU and I/O priority, raised as root and then dropped to the
service user ([install.md §7](install.md#7-the-timers-and-the-operator-wrapper)).
A restore destination
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
account. It defaults to your login name, except under a system account — the
`tapectl` service user, or root — where `init` refuses without it rather than
name the operator after the account. `init` creates
that tenant with a primary and a backup key, so do not `tenant add` it
afterwards — that fails because the name is taken. It also creates the
database, the config file, the `keys/` directory, and the permanent **escrow
recipient**, whose secret it prints exactly once. The secret line below is a
placeholder; your terminal shows the whole secret there:

```text
================================================================================
  ESCROW IDENTITY GENERATED -- THIS SECRET IS SHOWN EXACTLY ONCE, RIGHT NOW
================================================================================
  ...
  SECRET -- transcribe this line:

    AGE-SECRET-KEY-1<58 more characters: your secret>

  Public key (already saved to disk and the database -- safe to keep there):

    age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7

================================================================================

tapectl initialized at ~/.tapectl
  operator: mike
  database: ~/.tapectl/tapectl.db
  config:   ~/.tapectl/config.toml
  escrow:   age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7
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
`[[backends.lto]]` table by hand or with `tapectl config set
'backends.lto[NAME].<key>' <value>`, then run `tapectl config check`.

A minimal hand-written config looks like this (every key, its default and what
it does are in [configuration.md](configuration.md)):

```toml
[dar]
binary = "/usr/bin/dar"    # Path to dar binary

[[backends.lto]]
name = "lto-primary"
device_tape = "/dev/tape/by-id/scsi-XXXXXXXX-nst"   # by-id: /dev/nstN moves across reboots
# device_sg = "/dev/sg1"    # optional: derived from device_tape when absent (#431)
generation = "LTO-6"        # what the DRIVE is, not what you feed it
# capacity_override = "2400M"   # virtual drives and test harnesses only
# fill_ceiling = 0.97           # measured values come from scripts/lto-profile.sh (docs/lto-profiling.md)
# post_warning_bytes = "99008M" # measured room past early warning; caps slice_size

[staging]
directory = "/mnt/staging"  # Peak need: one unit's encrypted slices (no plaintext is written here)

[defaults]
slice_size = "1G"
global_excludes = ["*.nfo", "Thumbs.db", ".DS_Store", "*.tmp"]   # keep: a [defaults] table without it excludes nothing
min_copies = 2              # copies every unit needs (an archive set may ask for more)
min_locations = 2           # distinct locations those copies must be in

[discovery]
watch_roots = ["/media/tv", "/media/movies"]
```

Unknown keys are errors, everywhere. That includes the two `[defaults]` keys
that were renamed on 2026-09-29: a config written earlier carries
`min_copies_for_tape_only` and `min_locations_for_tape_only`, and it will not
load until you rename them to `min_copies` and `min_locations` — the meaning
and the values are unchanged, and the refusal says exactly that. (On an install
made with `first-run.sh`, re-running it offers the rename at step 0.)

Validate with:

```bash
tapectl config check
```

```text
config: valid
dar: 2.7.13 at '/usr/bin/dar' (meets minimum 2.6)
staging: '/srv/staging' exists and is writable
host check (defaults, no [host_check] table): units none; processes cargo, rustc, docker, Runner.Worker; max load 1.00/CPU; min available 2048 MiB; max memory pressure 10.00%; max I/O pressure 10.00% — `tapectl host check` runs it
```

**When `--device` is needed.** With exactly one `[[backends.lto]]` configured,
every tapectl command that touches a tape finds the drive without `--device`,
so the examples in this guide, and in the other pages, leave it out. Pass it,
as the by-id path, when:

- **more than one drive is configured.** Every command that touches a tape
  then refuses without it (`multiple LTO backends configured … pass --device to
  select one`).
- **a read has no backend configured.** `volume identify`, `volume verify`,
  `restore`, `catalog rebuild` and `volume read-slices` take any path you give
  them with no `[[backends.lto]]` at all, so a rebuilt machine holding only keys
  can read its tapes before `backend add`. That is the heir and disaster-recovery
  path ([Disaster Recovery](#disaster-recovery) here, runbooks B to F in
  [keys-and-recovery.md](keys-and-recovery.md#recovery-runbooks), and §8 of
  [install.md](install.md)), and those pages name the drive every time. A write never runs without a backend.
- **you run something other than tapectl.** `mt`, `dd`, `fuser` and the
  `sg_*` tools always need their device, and so do the scripts in `scripts/` and
  the test harnesses (`first-run.sh --device`, `lifecycle-suite.sh --device`),
  which take their own flag.

### Working on a different archive

Everything lives under `~/.tapectl` by default. To operate on a different one
— a copy on an external disk, a restored catalog, a scratch archive for a
drill — pass `--home` (or export `TAPECTL_HOME`):

```bash
tapectl --home /mnt/usb/archive report copies
TAPECTL_HOME=/mnt/usb/archive tapectl audit
```

`--config` points at a config *file*. On its own it **also** relocates the
whole home to that file's parent directory — database, keys, catalogs, stage
reports — which is surprising, and it warns when you do it. It still works,
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
# 0. Is the host quiet, and what changed?
tapectl host check
tapectl report dirty

# 1. Snapshot (fast metadata walk) and stage (dar + encrypt) each unit
tapectl snapshot create family/letters
tapectl stage create family/letters

# 2. What will go on tape, and how many cartridges?
tapectl volume plan --copies 2

# 3. First copy: load a blank cartridge. One command initialises it, writes the
#    volume, reads every file back, and moves it to the shelf once it is confirmed
tapectl volume write L8-0001 --init --full-confirm --move-to home-rack

# 4. Second copy: swap in another blank cartridge. This write completes the
#    policy's copies, so it also releases the staged slices (after its full readback)
tapectl volume write L8-0002 --init --full-confirm --move-to offsite

# (the long form still works: volume init, volume write, volume verify --full,
#  volume move; `tapectl volume verify` with no label verifies
#  whichever volume is loaded)

# 5. Check, release staging, refresh the Heir Kit, back up the catalog
tapectl audit
tapectl report copies
tapectl staging clean
tapectl key escrow-kit --out ~/heir-kit
tapectl db backup --to /mnt/backup/tapectl.db   # a timer install: sudo systemctl start tapectl-backup.service
```

What success looks like at each step:

```text
$ tapectl snapshot create family/letters
snapshot created: family/letters v1 (2 files, 224 B)

$ tapectl stage create family/letters
staged: family/letters (1 slices, 1.1 KiB dar, 1.8 KiB encrypted)

$ tapectl volume plan --copies 2
volume write plan (2 copy/copies):
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  family/letters v1: 1 slices, 1.8 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB x 2 = 3.4 MiB
estimated tapes: 1 (at the 97% fill ceiling)
cross-volume index: about 1.1 KiB per tape, 2.2 KiB on it with the envelope backups (an estimate: all of the above on one tape, as the catalog stands now)

$ tapectl volume init L8-0001
cartridge E01001L8_1775794348 auto-registered from MAM (barcode = medium serial)
volume "L8-0001" initialized (id=1)
medium: LTO-8, 12.00 TB (the cartridge chip's medium density code 0x5e)

$ tapectl volume write L8-0001
about to write to volume "L8-0001":
  family/letters v1: 1 slices, 1.8 KiB
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB
volume "L8-0001" write completed

$ tapectl volume verify L8-0001
verify L8-0001 (full tier): 13 checked, 13 passed, 0 failed

$ tapectl volume move L8-0001 --to home-rack
volume "L8-0001" moved to "home-rack"
  cartridge "E01001L8_1775794348" moved with it
```

`volume write` ends with a quick confirm: it reads the front index and the
seal marker back and checks them against what it wrote, and a passing confirm
seals the volume (ADR-0012, 2026-10-06). It does not read the data back, so
the `volume verify` straight after it is the volume's first full readback —
every file hashed against the front index, the one check that spans host,
cable, drive and medium. Until one passes, `audit` names the volume
(`no_full_verify`) and each unit on it (`verify_age`, `last: never`), and
`report verify-status` lists the volume. `volume write
--full-confirm` does the full readback as part of the write instead (hours on
a full cartridge); `volume verify` afterwards is then a second, independent
read-back. Here `audit`
after the first copy correctly reports that every unit is one copy short —
that is the reminder to write the second:

```text
$ tapectl audit
VIOLATIONS (4):
  [copy_count] family/letters: has 1 copies, needs 2
  [copy_count] family/photos/2019-italy: has 1 copies, needs 2
  [copy_count] family/photos/2020-garden: has 1 copies, needs 2
  [copy_count] work/invoices-2024: has 1 copies, needs 2
WARNINGS (1):
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 4 violations, 1 warnings (exit 2)
```

And after the second copy is written and moved offsite:

```text
$ tapectl audit
WARNINGS (1):
  [escrow_kit_missing] archive: 2 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 0 violations, 1 warnings (exit 1)

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
  catalog.db.age   encrypted catalog, 672130 bytes, covering 2 sealed volume(s)
  tapectl-1.5.0    the program that made this kit, 41238016 bytes, sha256 3a7bd3e2… (named on the sheet)

still to do, and only you can do it:
  1. print COVER.txt (or the HTML page)
  2. copy the escrow SECRET (AGE-SECRET-KEY-1..., shown once by `tapectl init`)
     by hand into the box marked WRITE IT HERE -- the kit prints only the
     public half, and without the secret the sheet opens nothing
  3. seal it in a tamper-evident envelope
  4. store copies in at least TWO independent failure domains
```

(The remaining warning, `escrow_kit_missing`, is what step 5's
`key escrow-kit` clears. Take the catalog backup last, once the session is
over; [Database Operations](#database-operations) says why a mid-session one
is not enough.)

If you archive whole folders of similar things — one unit per subfolder of a
media root — a [Collection](cli/collection.md) (`collection sync`,
`collection plan`, `collection run`) does steps 1–4 for you, and
`quick-archive` registers, snapshots, stages and writes a single directory
onto a volume you have already initialised.

**`collection run` releases staging as soon as its copies are confirmed.**
Each copy's write ends with the default quick confirm (the front index and the
seal marker, not the data), and once every unit of the batch has the copies its
`min_copies` asks for, the run releases those units' staged slices straight
away (ADR-0012, 2026-10-07). So on this path no full readback of the data
precedes the release. A `volume verify` after the run comes only after the staged
slices are gone, and a copy it finds bad is rewritten from another copy (`volume
read-slices`), not from staging; `--full-confirm-first` (or `--full-confirm`) on
the run reads the data back while staging is still held.
When the data matters, have at least one copy read back in full before the
release. `collection run --full-confirm-first` reads the run's first copy back
in full and confirms the others quickly; `--full-confirm` reads every copy back
in full. Each full readback takes about as long as its write (~2.3 h on a full
LTO-6). The two flags cannot be given together (ADR-0012, 2026-10-07). A
`collection run` writes one copy today (it takes one `--label`), so the two
flags read back the same copy; `--full-confirm-first` says what you mean if
runs ever write more. A second copy written afterwards with `volume write`
gets the quick confirm unless you pass it `--full-confirm`.

`volume write` and `volume resume` release staging the same way (#440): once the
copy they seal brings every unit written to the volume to its `min_copies`, they
release those units' staged slices, and name any unit still short. Pass
`--keep-staging` to hold them, for example for a full `volume verify` first, and
run `staging clean` afterwards.

A collection archives only its units: the real folders at exactly
`unit_depth` below its root, and everything inside them. Anything else under
the root down to that depth is in no unit and never reaches a tape: a loose
file beside the unit folders (or, with `unit_depth` 2 or more, beside their
parent folders), a symlink there, and a symlink to a folder standing where a
unit folder would be (`collection sync` does not follow it). `collection sync`
and `collection status` name each one and exit 1, so a clean run means the
whole root is in units:

```text
collection "movies": 0 pending, 0 dirty, 0 missing, 0 under-copied
  OUTSIDE ANY UNIT (not archived): 2 entries under the root belong to no unit
    README.txt (file)
    Brazil.1985 (symlinked directory)
```

Move such a file into a unit folder, replace the symlink with the folder
itself, or list the name in the collection's `exclude` to say you meant to
leave it out.

### Staging a source that is in use

Staging reads each unit's files once, while dar archives them, and refuses the
whole stage if any file changes during that read (ADR-0012, 2026-10-06 item 4:
dar runs with `--retry-on-change 0`, so a file caught mid-change is never
archived half-changed). A failed stage starts over (#409): there is no resuming
it. A unit can be up to one cartridge's budget (#395), so one stage can take many hours, and
one file touched by anything in those hours costs the whole stage.

For a source nothing writes to (a finished photo year, a scanned archive) this
never happens. For a living one, take one of two precautions before `stage
create` or `collection run`:

- **Stage from a read-only snapshot of the filesystem.** Take the snapshot (LVM:
  `lvcreate --snapshot --name src-snap --size 20G vg/src` and mount it read-only;
  btrfs: `btrfs subvolume snapshot -r /srv/src /srv/.snap-src`; ZFS:
  `zfs snapshot pool/src@stage`, readable at `/pool/src/.zfs/snapshot/stage`).
  tapectl stages from each unit's registered path, so show it the snapshot AT
  that path in a private mount namespace, as root, while everything else keeps
  writing the live directory:

  ```bash
  sudo unshare --mount sh -c 'mount --bind -o ro /srv/.snap-src /srv/src &&
      sudo -u tapectl -H tapectl stage create <unit>'
  ```

  Then remove the snapshot. For `collection run`, the snapshot hook below does
  all of this for you. `stage create` on its own has no hook and keeps this recipe.
- **Or hold the source still** for the stage's duration: stop sync clients,
  downloads and anything that autosaves into it, and do not edit it.

`collection run` stages units in name order (several at once with `[staging]
jobs`), so the stage report's start and end lines say which folders are being
read at any moment.

#### A snapshot hook for `collection run`

Since 1.2.0 a `[[collections]]` table can name two commands of yours. `collection run`
runs `snapshot_command` once, before it snapshots or stages any unit. The command takes
a read-only snapshot of the collection's root and prints its path. The run then reads
each unit's folder inside that snapshot. When staging is over, it runs
`release_command` to drop the snapshot.

```toml
[[collections]]
name = "media"
root = "/srv/media"
tenant = "family"
snapshot_command = "/usr/local/bin/tapectl-snap-take"
release_command = "/usr/local/bin/tapectl-snap-drop"
```

The two commands are yours. tapectl runs each under `sh -c`, as the user tapectl runs
as, with its standard input closed. Both get these environment variables:

| Variable | Value |
|---|---|
| `TAPECTL_COLLECTION` | The collection's name. |
| `TAPECTL_COLLECTION_ROOT` | Its root, as a canonical path. |
| `TAPECTL_SNAPSHOT_PATH` | The snapshot's path. `release_command` only. |

`snapshot_command` must print exactly one non-empty line on **stdout**: the absolute
path of a read-only snapshot that mirrors the root's layout, so that `<root>/Alien.1979`
is `<that path>/Alien.1979`. Everything else the command says goes to stderr, which
tapectl shows. Many snapshot tools print to stdout (`btrfs subvolume snapshot` does),
so redirect them with `>&2`. The snapshot must not be inside the root. tapectl reads it
as the user it runs as, as it reads the live root.

Anything else refuses the run **before any unit is read**, and the refusal names the
command and what it said on stderr: a non-zero exit, no line or several, a relative
path, a path that cannot be read or is not a directory, a path inside the live root, or
a unit whose folder is not in the snapshot (see
[the refusals](troubleshooting.md#collection-run-snapshot-hook-refusals-120)).

What changes during the run, and what does not:

- The walk, the content match, the source check, the sha256 pass, `dar -c -R` and the
  check after dar all read the unit's folder in the snapshot. A file touched in the
  living source after the snapshot was taken is not the stage's business.
- Every path tapectl **records** stays the real one: `units.current_path`,
  `snapshots.source_path`, the file lists, and the `-R` in the stage set's `dar_command`,
  which MANIFEST.toml carries to tape. `dar_create_reports` keeps the command exactly as
  dar ran it, so it shows the snapshot's path.
- `release_command` needs `snapshot_command`; the key alone is refused. It runs when the
  run's staging is over, whether the staging worked or not, and **before any tape is
  touched**, so the snapshot is held for the staging and not for the hours of writing
  after it. It does not run when `snapshot_command` itself failed, because nothing was
  taken. A failure of `release_command` is a warning on stderr (`snapshot_release_warning`
  in `--json`) naming the snapshot, and the run goes on: remove the snapshot by hand.
- `collection run --json` gains `snapshot_path` and `snapshot_release_warning`.
  `collection run --dry-run` shows `snapshot_command` (and `--json` both commands) and
  runs neither. `collection plan`, `status` and `sync` never run either, and neither does
  `config check`, which only checks that the keys are well formed.
- A first Ctrl-C, SIGTERM or SIGHUP stops the staging at its next safe point, and
  `release_command` still runs. A second signal, which stops tapectl at once, or a
  SIGKILL means `release_command` never runs. The snapshot stays until you remove it.

If a script mounts or snapshots, it needs root, and the tapectl user is not root. Give it
a sudoers entry for exactly the commands it runs. For btrfs, with `/srv/media` a
subvolume:

```bash
#!/bin/sh
# /usr/local/bin/tapectl-snap-take
set -eu
sudo /usr/bin/btrfs subvolume snapshot -r /srv/media /srv/.snap-tapectl-media >&2
echo /srv/.snap-tapectl-media
```

```bash
#!/bin/sh
# /usr/local/bin/tapectl-snap-drop
set -eu
sudo /usr/bin/btrfs subvolume delete /srv/.snap-tapectl-media >&2
```

```text
# /etc/sudoers.d/tapectl-snapshot (edit it with visudo)
tapectl ALL=(root) NOPASSWD: /usr/bin/btrfs subvolume snapshot -r /srv/media /srv/.snap-tapectl-media, /usr/bin/btrfs subvolume delete /srv/.snap-tapectl-media
```

For LVM, make the snapshot, mount it read-only, and print the directory that mirrors
the root. If `/srv/media` is the mount point of the origin volume `vg/media`:

```bash
#!/bin/sh
# /usr/local/bin/tapectl-snap-take (create /mnt/tapectl-media once)
set -eu
sudo /usr/sbin/lvcreate --snapshot --name tapectl-media --size 20G vg/media >&2
sudo /usr/bin/mount -o ro /dev/vg/tapectl-media /mnt/tapectl-media >&2
echo /mnt/tapectl-media
```

The release script unmounts it and runs `lvremove -f vg/tapectl-media`, each through
`sudo`, with sudoers entries as above. An XFS snapshot also needs `-o ro,nouuid`,
because XFS refuses to mount a second filesystem that has the origin's UUID. Size the
snapshot for the changes the living source takes while the staging runs: a classic LVM
snapshot that fills up is invalidated.

Two things to get right:

- **The printed path must mirror the root, not the filesystem.** If the root is a
  subdirectory of the snapshotted filesystem (the root is `/srv/media/movies` and the
  filesystem is mounted at `/srv/media`), print the matching subdirectory inside the
  snapshot (`/mnt/tapectl-media/movies`).
- **ZFS: the `.zfs/snapshot` directory is inside the root and is refused.** With
  `pool/media` mounted at `/srv/media`, its snapshot is readable at
  `/srv/media/.zfs/snapshot/<name>`, and that path is under the live root. Clone the
  snapshot (`zfs clone -o readonly=on -o mountpoint=/mnt/tapectl-media
  pool/media@<name> pool/tapectl-media`) or bind-mount it read-only somewhere else, and
  print that path. The release destroys the clone and then the snapshot.

### Watching a long operation: progress and the session log

`stage create`, `volume write`, `volume resume`, `volume verify`,
`volume read-slices`, the compaction reads and writes, `restore`,
`quick-archive` and `collection run` each run as named **phases**, and show
where they are on stderr while they run:

- **On a terminal**, one status line is redrawn in place: the phase, bytes
  done and total, rate, ETA, the unit or file being worked on, and any wait in
  progress (a rewind, an `sg_logs` run, a busy catalog). A phase that took two
  seconds or more leaves a `done:` line behind.
- **When stderr is not a terminal** (a pipe, a log file, cron, systemd), the
  redraw becomes one plain `progress:` line every 30 seconds, plus a `done:`
  line for each phase that long. `TAPECTL_PROGRESS_INTERVAL=<seconds>` changes
  the interval.
- **`--quiet` (`-q`)** prints no progress at all. Nothing is ever printed on
  stdout, so `--json` output is exactly what it was.

The phases of a write are `contact-open` (the MAM read, the drive's identity,
the counters at the contact's open), `build`, `prewrite-check` (the full read
of every staged slice under `--prewrite-hash`, a size check otherwise),
`positioning` (opening the drive, the File 0 check, the rewind), `plan`,
`write`, `seal`, `confirm` (by default the front index and the seal marker
read back, a minute or two; under `--full-confirm` the readback of the whole
tape: one rewind, then every file in a single forward pass ending on the seal
marker) and `health-sweep`; a resume has `drive-open`, `revalidate`, `identify` (the File 0
and seal checks) and `positioning` in place of the build and pre-write steps,
and the log names each file as it is written. `stage create` has `check` (the source against
the snapshot, by metadata), `archive` (dar, slicing, encryption and the source's hashing in one
pass), `recheck` (every hashed file unchanged since), `catalog` and `finalize`.

Every such command also writes a **session log**, whether or not anything
was shown: `<home>/logs/<UTC start>-<command>-<label>-<pid>.log` (mode 0600,
in the same 0700 directory as the catalog — or 0640 in a group-readable
`logs/` with an [`[ops] group`](#watching-from-another-account)). Each line starts with a UTC
timestamp. It records each phase's start and end with its duration, bytes and
rate; a progress line every interval; every tapectl log event at INFO and
above (DEBUG under `--verbose`), whatever `[logging] level` lets through to
stderr; and any wait that lasted five seconds or more — when it passed five
seconds and when it ended — naming what was waited on:

```text
2026-09-30T14:02:11.402Z phase start: confirm (1.20 TiB)
2026-09-30T14:02:16.911Z wait start: tape rewind (5.0 s so far)
2026-09-30T14:03:41.118Z wait end: tape rewind after 1m 29s
2026-09-30T14:04:11.402Z progress: confirm 12.3 GiB of 1.20 TiB (1.0%), 145.2 MiB/s, ETA 2h 23m, elapsed 2m 00s — file 5 of 43
```

A phase that counts bytes and moves none for a minute, with no named wait in
progress, logs a `stall:` line, so a gap nothing thought to name still shows
up.

A write logs one line per file, with where its time went:

```text
2026-10-06T03:12:40.118Z wrote file 7 (data_slice): 9.77 GiB in 1m 05s; tape waited 0.3 s for data, queue full 58.1 s
```

`tape waited` is time the tape writer spent with nothing to write — the host
was behind the drive. `queue full` is time the staged-file reader spent with
all 256 MiB of buffers in use, waiting on whatever is downstream of it: the
hash or the drive. Read them together:

- `tape waited` near zero: the drive set the pace — the write streamed. Most
  of the file's time then shows as `queue full`.
- `tape waited` high and `queue full` high: the hash set the pace (the
  reader is ahead of it, the drive behind it) — the CPU is the limit.
- `tape waited` high and `queue full` near zero: reading the staged file set
  the pace — the staging disk is the limit.

For a closer look than one line per file, set `TAPECTL_PIPELINE_TRACE=1` for
the command. The write then also writes `<session>.pipeline.tsv` beside its
session log: one tab-separated row every 250 ms, under a header naming each
column. A row holds the bytes written, the queue split into read-but-unhashed
and hashed-but-unwritten, the drive's time per block, the waits on either side
of the queue, and each pipeline thread's CPU time, run delay and CPU. It also
holds the host's CPU shares and run delay, the staging disks' busy share and
latency, the load average and the process's swap. The column list and what
each means is `tapectl::pipeline_trace::COLUMNS` (and
`docs/research/2026-10-20-write-shaping.md` §9). Watching changes nothing the
write does; it is off unless asked for, and a trace that cannot start says so
once in the session log and the write goes on. Until the write session names
each staged file to it, the trace's `file` column numbers the files (`#1`,
`#2`, ...) and its disk columns are empty.

The durations are kept in the catalog too. `volume info` ends with the phase
timings of the last session recorded against the volume (write, resume,
verify, read or restore):

```text
Phase timings (volume write, session 20260930T120001Z-volume-write-L6-0001-4242):
    2026-09-30 12:00:01  contact-open       2.4 s
    2026-09-30 12:00:04  build              1.2 s
    2026-09-30 12:00:05  prewrite-check      18 ms
    2026-09-30 12:00:05  positioning       1m 32s
    2026-09-30 12:01:37  plan               40 ms
    2026-09-30 12:01:37  write             2h 26m    1.20 TiB  143.8 MiB/s
    ...
```

and each stage report (`<home>/stage-reports/`) ends with a `Phase timings:`
section for the stage it describes. `volume info --json` does not carry them;
they are in the `phase_timings` table (migration 028) for scripts.

Each session log now ends with how the command ended — `session result: ok`,
or `session result: failed — <the error>` — just before its `session end` line
(a `volume verify` that exits 2 or 3 logs `session exit with code N` instead).

**tapectl never prunes session logs or stage reports** (ADR-0012, 2026-10-07):
they are evidence — what each long command did and when, and the hash of every
slice as it was staged — so how long to keep them is your call. They grow
slowly. A session log starts at about 1.5 KB (a small `stage create` writes
1.6 KB) and adds about 20 KB per hour the command runs (one `progress:` line
every 30 seconds), plus about 130 bytes per file a write puts on tape and
about 100 bytes per slice a stage produces. A stage report is about 600 bytes
plus about 200 bytes per slice. At the default 1 GiB slice, a full LTO-6's
worth of data (about 2,300 slices) comes to roughly 0.3 MB of stage logs and
0.5 MB of stage reports, about 0.5 MB for each copy's write log, and under
0.1 MB for each full verify: about 2 MB for two copies, and around 100 MB a
year at a cartridge a week. Pruning `logs/` loses nothing the catalog needs,
but `tapectl status --last N` reads only the logs that are there; the phase
timings are kept in the catalog regardless.

### Watching from another account

`tapectl status` says what is running and how the last sessions ended, from
the session logs alone — it opens no catalog and no config:

```bash
tapectl status                       # what is running, and the last 5
tapectl status --last 20 --json
```

The home comes from the pointer `/etc/tapectl/home` (written by first-run step 7 and
`install-systemd.sh`) when the account has no `~/.tapectl` of its own; it is ignored as
root, and `--home` or `TAPECTL_HOME` override it.

```text
session logs: /srv/archive_meta/tapectl/.tapectl/logs
running: volume write L6-0002 (pid 81234), started 2026-10-06T03:12:40.118Z
  phase:    write
  progress: write 512.0 GiB of 1.20 TiB (41.7%), 145.2 MiB/s, ETA 1h 21m, elapsed 1h 00m — file 19 of 43 (at 2026-10-06T04:12:41.002Z)
  last log line at 2026-10-06T04:12:58.310Z
  log:      /srv/archive_meta/tapectl/.tapectl/logs/20261006T031240Z-volume-write-L6-0002-81234.log
recent:
  2026-10-05T19:02:11.904Z  stage create keepsake/1998-wedding  — ok
      check            2.1 s
      archive         41m 3s   88.2 GiB  36.6 MiB/s
      ...
```

A running session's progress line is at most one interval (30 s) old, whatever
its display. A session whose log has no end line and whose process is gone is
shown as ended with no end line — killed, crashed or power lost — and the phase
it stopped in; the next tapectl command recovers what it was doing. Another
account may not see the writer in `/proc` at all (`/proc` mounted with
`hidepid`, or the service in its own PID namespace), which looks the same as a
process that is gone: while such a session's log is still being written (its
last line under two minutes old) it is listed as running, marked "process not
visible from this account", and only a log quiet for longer reads as ended.

The logs belong to the user tapectl runs as. To let your own account (or an
agent helping you) run `status` without sudo, name a group in the home's
config.toml:

```toml
[ops]
group = "tapectl-ops"
```

Both the service user and your account must be members (`sudo usermod -aG
tapectl-ops tapectl`, and the same for yourself; it takes effect at the next
login). From then on every command run as the service user keeps the home
at 0710 and `logs/` at 2750 owned by the group, and writes each log 0640 — the
group can read the logs and open nothing else: every other entry of the home
(the catalog, the keys, config.toml, …) is closed to it on every command.
Until the service user is a member, the home and `logs/` stay 0700 rather
than open to whatever group they already have. `config check` (as the service user) says whether
the group, the memberships and the modes are as they should be;
[configuration.md](configuration.md#ops) has the details. `scripts/first-run.sh`
does all of it in step 7 when the host profile sets `OPS_GROUP` — on an existing
install, `scripts/first-run.sh --profile <profile> --from 7 --to 7`, once the
installed tapectl is 1.1.0 or later (`tapectl --version`): an older one refuses
`[ops]` as an unknown key, and step 7 would leave it in `config.toml`.

### Register units

A **unit** is one directory tracked as a whole. Its name is derived from its
path, with a leading `/media/` or `/mnt/` dropped (`/media/tv/breaking-bad/s01`
becomes `tv/breaking-bad/s01`); `--name` overrides it.

```bash
# Single directory
tapectl unit init /media/tv/breaking-bad/s01 --tenant family --tag tv

# Every immediate subdirectory becomes its own unit (s01, s02, …)
tapectl unit init-bulk /media/tv/breaking-bad --tenant family --tag tv

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

**Leaving files out.** Exclude patterns come from `[defaults] global_excludes`
in config.toml (`init` writes `*.nfo`, `Thumbs.db`, `.DS_Store` and `*.tmp`)
plus the `[excludes] patterns` list in a unit's dotfile. Matching ignores case.
A plain pattern such as `*.iso` matches a file's name, anywhere in the unit. A
pattern ending in `/`, such as `.cache/` or `node_modules/`, drops everything
inside every directory of that name, at any depth below the unit, from the
snapshot and from the dar archive, so none of it reaches tape (the directory
itself is kept, empty). A collection's own `exclude` list, which decides which
folders become units, ignores case too: a plain pattern there matches a
candidate folder's name, and a `name/` pattern skips every candidate below a
folder of that name. Every key is in [configuration.md](configuration.md).

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
tapectl volume init L6-0001
tapectl volume write L6-0001

# Step 4: Verify
tapectl volume verify L6-0001
```

**Staging space.** `stage create` writes only a unit's encrypted slices to
staging, never the plaintext archive: dar writes its archive to standard
output, and tapectl cuts and encrypts it in memory. At its peak staging holds
the unit's slices, about the unit's size. The source is read once: each file
is hashed within 128 MiB of dar reading it, so the second read comes from the
page cache (dar's own page-cache eviction is switched off while it archives, and
tapectl drops the source's pages itself once both have read them, issue #441).
`[staging] hash_threads` files are hashed at once (1 by default,
never more than the cores) when they fit in that 128 MiB, as many small files do;
more threads can speed a unit of many small files, and on a single spinning
disk they may instead read slower.

**Several units at once.** `stage create` takes several unit names, and
`--jobs N` (or `[staging] jobs` in `config.toml`) stages N of them at a time,
the largest first:

```bash
tapectl stage create family/letters family/photos family/video --jobs 2
```

Each unit prints a line as it starts and ends, and the `staged:` lines follow;
while several run there is no live progress line. With more than one at a time,
each unit also gets a session log of its own beside the command's
(`<UTC>-stage-create-<unit>-<pid>.log`, a `/` in the name written as `-`), holding that unit's phases, waits and
progress lines and how it ended, so no two units interleave in one file; the
command's log records each unit's start and end and names its log, and
`tapectl status` lists each unit as its own session.
Every name is checked before anything is staged. If one unit fails, no further
unit is started; the ones already running finish and stay staged, and the error
names what was staged, what failed and what was not started. `collection run
--jobs N` and first-run's staging step stage the same way. A unit is never
staged twice at once: a second `stage create` of a unit that is being staged
right now is refused.

Before reading anything, it compares the free space with the unit's size (plus
dar's records). If free space is below that, the unit may or may not fit
(compression, zero-filled and hard-linked files all make it smaller), so it
asks. The question is an ADR-0008 Tier-2 one: a terminal gets
`stage unit "…" — proceed? [y/N]`, `--yes` proceeds (the figures are still
printed), and a run with no terminal and no `--yes` refuses. That includes
`collection run`, `quick-archive` and anything you script. If a stage that
went ahead does run out of room, it stops and the partial slices are removed.

Each stage set also leaves a short stage report (unit, tenant, snapshot, and
every slice's size and hash) in `<home>/stage-reports/`. A home initialised
before 2026-09-29 kept these in `receipts/`; the first command run on it moves
the directory. Nothing prunes them; see
[how fast they and the session logs grow](#watching-a-long-operation-progress-and-the-session-log).

**`volume write` writes everything still staged, not just what you staged in
this sitting** — and it says so before it touches the drive:

```text
about to write to volume "L6-0001":
  tv/breaking-bad/s01 v1: 3 slices, 42.0 MiB
  photos/2019 v2: 1 slices, 8.0 MiB

total: 4 slices, 50.0 MiB
```

That is by design, not a bug: staging happens once and a stage set stays
`staged` until the write that completes its copies, or `staging clean`, releases it, which is what lets you write the
same data to a second and third cartridge without re-archiving it (ADR-0006).
The consequence is that a stage set left over from last week rides along on
today's tape. **Read the list.** This is write-once media — a tape that
received four units when you meant one cannot be un-written, and under ADR-0003
it cannot even be overwritten without a real erase.

The announcement is display only. It does not prompt, and there is no flag to
select a subset: if the list is not what you want, stop, run `staging clean` to
release what is already safely on tape, and start again. (`volume write` may
still ask one question — the quiet-host check below — which `--yes` answers.)

**Integrity while writing.** Every staged slice is hashed as it streams to the
drive, and a slice whose bytes no longer match what `stage create` recorded
stops the write before anything is sealed — the tape is left unsealed and
nothing counts as a copy. The check finishes before the slice's last block
reaches the drive, so a slice that fails it never gets its last block or its
end-of-file mark written. Reading the staged file, hashing it and writing the
tape run side by side on separate threads, with up to 256 MiB queued between
them, so the drive is not left waiting on the disk or the hash. Before the tape moves, `volume write` checks only that
each staged slice exists at its recorded size. `--prewrite-hash` adds a full
hash of every staged slice from disk first, so a rotted slice is refused before
any tape I/O, at the cost of one extra full read of the batch (hours for a
full LTO-6). It is off by default (ADR-0012, 2026-09-30) because the streaming
hash already keeps a bad slice off a sealed tape; `volume resume`,
`volume compact-write`, `volume compact`, `collection run` and `quick-archive`
take the same flag.

**The fill ceiling.** A write fills at most 97% of the cartridge's capacity by
default (`fill_ceiling` in the drive's `[[backends.lto]]` block; ADR-0012,
2026-10-06). A real end of tape is a clean abort that costs the whole session,
and usable capacity is not fixed: a worn cartridge loses some to the drive's
rewrites, an uneven feed loses more. A batch above the ceiling is refused before
the tape moves, with the overage named. `--fill-ceiling 0.99` (or `99%`) lifts it
for one write; `volume plan`, `collection plan` and `collection run` take the
same flag. Each completed write records how much of the capacity it used, as a
`write_capacity_used` event (`tapectl report events`): the data and the native
tape the drive used (page 0x0c), each as a share of the capacity. Those figures
are what the default is to be tuned from.

**Cartridge wear.** Before it writes, `volume write` prints the cartridge's wear
figures on stderr: the chip's own lifetime record (loads, initialisations,
lifetime MiB written and read, manufacture date, the last drives that loaded it)
and the drive's lifetime counters for this cartridge from the newest page 0x17
reading tapectl journalled (write and read retries, unrecovered errors,
beginning-of-medium passes). `tapectl cartridge info <barcode>` shows the same
block. There is no health score: until production data sets thresholds
(ADR-0012, 2026-10-06 item 11) these are figures to read, and the only
`WARNING: worn cartridge?` lines are things the hardware itself raised — TapeAlert
flags on the chip, a medium TapeAlert from the drive (`Media`, `Media life`,
`Nearing media life`, `Not data grade`) raised by any reading since the
cartridge's last passed full readback (not only the newest reading: whether page
0x2E clears on read is unsettled, so a later clean reading proves nothing; #467),
or an unrecovered error on page 0x17. The medium-flag line ends "…(page 0x2E),
last at <when> — and no full readback has passed since".
A warning never stops the write. If you see one, write that batch to another
cartridge and retire this one. A line saying `no reading recorded` means tapectl
has no figure, not that the figure is good.

`tapectl report cartridge-health` ranks every registered cartridge, worst
first, and says why: `ATTENTION` names each reason (a medium TapeAlert raised on
one of its contacts, an uncorrected error counted during one, a volume
quarantined on medium evidence, a rising read-error rate); `NO_EVIDENCE` is a
cartridge with no health reading and no verify that read it back — never
examined, not healthy; `CLEAN` is examined with nothing adverse. A drive or
cleaning TapeAlert raised while a cartridge was loaded is noted against it but
is the drive's, a quarantine from a write-session finding is noted, not counted,
and so is a volume whose last verify failed without proving the medium bad (the
drive could not read it — a verify that did prove it quarantined the volume). A
verify still running, or aborted, is not counted at all. The report ends with
what resolves to no registered cartridge — readings, medium TapeAlerts and
rising read-error trends from a contact that identified none, such as a tape
written before 2026-09-13 — by volume and contact, or `unattributed: none`; under
`--json` the cartridges are under `cartridges` and that block under
`unattributed`. `cartridge info` shows the last bind and the last contact of
any kind, and when each volume it carried was mounted and unmounted.

**The confirm after sealing.** By default the write reads back the front index
and the seal marker and checks the seal's binding and the index against what
it wrote; a passing confirm seals the volume (ADR-0012, 2026-10-06). The data
is not read back, so the volume has no full readback until `volume verify`
runs one — see [Monthly — verify a rotating slice of the library](#monthly--verify-a-rotating-slice-of-the-library): `audit`
names every such volume (`no_full_verify`) and `report verify-status` lists
it. `--full-confirm` makes the write read every file back and hash it, which
costs about as long as the write itself (~2.3 h on a full LTO-6); the same six
commands take it. A full readback that is interrupted (Ctrl-C, a dropped ssh
session, a reboot) keeps what it has read back clean: `tapectl volume resume
<label> --full-confirm` continues it from there rather than from the first
file — unless what it kept is older than the volume's `verify_interval_days`,
in which case it reads everything again (see
[the verify section](#monthly--verify-a-rotating-slice-of-the-library)).

`staging clean` releases every unit that has met its policy's `min_copies` and
**retains** the ones that have not, naming them (ADR-0012). So a unit still
short of its second copy keeps its staged bytes — the release cannot quietly
discard the cheap route to a copy your own policy requires — while everything
already safely on tape is freed. `--unit` (and `--version`) narrow what is
considered. `--force` releases the retained ones too, and is wider than the
gate it overrides: it also drops staged data for sets never written to any
tape.

It also clears what a killed run left behind (#465, #474): the dar catalogue
directory of a set that never finished staging, `<home>/tmp` entries whose newest
file is a day old, and `clone-*` and `compact-*` directories under staging that no
staged set references and that have been quiet for an hour. They are counted as
"leftovers of killed runs removed" (`--json`: `home_leftovers_reclaimed`).

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
- Nothing else touches the drive. Every tapectl command that contacts the
  drive holds its drive lock (`<home>/locks/drive-<node>.lock`) from its
  contact until it exits, so the [daily drive poll](#the-daily-drive-poll)
  skips rather than read the drive under it. The lock does not stop a second
  tapectl command: the kernel's `st` driver refuses a second open on its own
  ("Device or resource busy"), and a command that finds the lock held waits a
  minute and then proceeds without it. The repository's scripts
  (`first-run.sh` step 13 and the test harnesses) also take
  `/tmp/tapectl-tape.lock` and refuse a second user, but a command you type by
  hand does not check that one.
- Memory matters too: a process killed for memory pressure mid-write costs the
  cartridge its session (a clean abort to an unsealed tape, but the time is
  gone). Keep a few GB free. tapectl itself holds up to 256 MiB of tape blocks
  in flight while it writes or reads a tape, whatever the size of the data.

The rule is checked, not only stated (ADR-0012). `tapectl host check` reports
the load average, available memory, memory and I/O pressure
(`/proc/pressure/*`, `full avg60`), and any contender process or systemd unit,
each against its limit; it exits 0 when the host is quiet and 1 when anything
tripped, and runs on a machine not yet `init`ed:

```text
$ tapectl host check
load average       2.69 over 16 CPUs = 0.17/CPU             max 1.00/CPU
available memory   8071 MiB                                 min 2048 MiB
memory pressure    0.00% full avg60                         max 10.00%
I/O pressure       0.60% full avg60                         max 10.00%
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

The tape commands also run at a higher priority than the work they compete with
(#432): to `nice -10` and best-effort I/O level 0. The wrapper `tapectl-op`
raises a tape command as root and then drops to the service user;
`first-run.sh` step 13 raises its own shell once (`sudo renice`), and the
commands it starts inherit that; `tapectl-drive-poll.service` sets the same
with `Nice=` and `IOSchedulingClass=`. Neither replaces the quiet, and each
reaches less than its name suggests:
- **nice** ranks the tape command only among processes in its own cgroup. The
  kernel shares a CPU between cgroups by group weight, so a guest's vCPUs or a
  container in another cgroup are not outranked.
- **The I/O level** takes effect only on a disk whose I/O scheduler acts on
  priorities (`cat /sys/block/<dev>/queue/scheduler`). bfq does;
  mq-deadline does not before Linux 5.14.

On home2 as configured (measured 2026-10-20, #488), neither reaches what
competes with a write: `vm-desk1`'s vCPU threads run in libvirtd's cgroup, and
the staging disks use mq-deadline on a 4.19 kernel. The lever that does work
there is CPU affinity (pinning), a host decision that
`contrib/hosts/home2-prep.sh` §9 describes and never applies. To see what a
running write got, find its process (`pgrep -x tapectl`) and read
`ps -o pid,ni -p <pid>` and `ionice -p <pid>`.

The write itself needs no supervision once it is streaming; the drive's
counters afterwards say whether the feed held: `tapectl volume info <label>`
shows each write's feed ratio (native tape bytes per data byte sent, judged
against 1.05), and `tapectl report health` lists every recorded write, the
uneven feeds first. On an HP drive each line also gives the drive's own wear
count, `repositions per 100 MB` (its log page 0x34: "tape direction changes per
100 MB written. The higher the figure the more wear occurs"), and `tapectl report
events` shows it on the `write_feed_ratio` line. A write recorded before the figure
was kept still has the page's raw bytes in the catalog, so it is decoded for those too. The
drive reads it after the whole command, the confirm's rewinds included; it is
not yet known whether it averages since the cartridge was loaded or over the
last stretch written (`docs/research/2026-10-20-write-shaping.md` §6).

### Check what's pending

```bash
tapectl stage list --status staged
tapectl volume plan --copies 2
tapectl report pending
```

`stage list --status staged` is everything the next `volume write` would
carry. `report pending` is narrower: the stage sets whose version still owes a
copy, fewer copies than its unit's resolved `min_copies`, each with how many it
has of how many it needs. A stage set that already has all its copies is only
counted on a last line, as held in staging until `staging clean` releases it:

```text
$ tapectl report pending
no stage set owes a copy
4 more stage set(s) have every copy their own version needs and are only held in staging (`tapectl staging clean` decides which can be released)
```

`volume plan` estimates against the drive's own generation; pass
`--generation LTO-5` to count cartridges of another generation that drive can
write.

`--policy-aware` puts first the units a write would help: those `audit` finds
with too few copies, or with no copy at a required location. Each one is named
with what it is short of. `volume plan --policy-aware` only orders the list,
because `volume write` writes every staged set either way. `collection plan
--policy-aware` changes the batches: those units are packed into the first
batches, each group in name order. Batch numbers follow that order, so give
`collection run` the same flag to run the batch you reviewed:

```bash
tapectl volume plan --copies 2 --policy-aware
tapectl collection plan --policy-aware
tapectl collection run --collection movies --batch 0 --label L8-0003 --policy-aware
```

### Restore

`restore` takes everything as flags: `--unit`, the volume to read `--from`,
and the destination `--to` (plus `--file` for a single file).

```bash
# Full unit
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit

# Single file (path within the unit)
tapectl restore file --file 1998-letter-to-mum.txt --unit family/letters \
  --from L8-0002 --to /tmp/restore

# Dry run
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --dry-run

# A large unit whose slices are spooled: they wait on another disk
tapectl restore unit --unit family/letters --from L8-0002 --to /srv/restore/letters \
  --scratch /mnt/big

# Into a directory that already holds an older copy, replacing what collides
tapectl restore unit --unit family/letters --from L8-0002 --to /srv/restore/letters \
  --overwrite

# Spool the slices to disk even though the unit could stream into dar
tapectl restore unit --unit family/letters --from L8-0002 --to /srv/restore/letters \
  --spool

# Every unit on a volume, in one pass over the tape, each into /srv/restore/<unit>
tapectl restore volume L8-0002 --to /srv/restore

# Only some of them, still in one pass
tapectl restore volume L8-0002 --to /srv/restore --unit family/letters --unit mike/notes
```

```text
$ tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --dry-run
would restore "family/letters" v1 from L8-0002 (1 slices) to /tmp/restore/unit

$ tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit
2026-09-29T08:20:06.455931Z  WARN tapectl::dar::restore: restoring as a non-root user: restored files will be owned by the invoking user, not their archived owners
restored "family/letters" v1 from L8-0002 (1 slices) to /tmp/restore/unit

$ diff -r /media/family/letters /tmp/restore/unit && echo identical
identical
```

The `WARN` line is expected when not running as root (which includes the
service user): the files come back, but owned by the restoring user rather
than their archived owners. `--version N` restores an older snapshot; by default the newest version
of the unit on that volume is used. `catalog locate` (below) tells you which
volumes to restore `--from`.

`restore volume <label> --to DIR` restores several units from one volume in
one forward pass over the tape: the drive is opened and rewound once, and the
units are read in the order they lie on the tape, however they were named.
Each unit goes into its own directory, `DIR/<unit name>`, which must be empty
or new. Without `--unit` it restores every unit with written slices on that
volume; `--unit NAME`, repeated, names the set. Each unit's newest version on
the volume is restored, as `restore unit` does by default; for an older
version, use `restore unit --version`. Restoring a whole volume this way costs
one pass instead of a rewind and locate per unit, which is what makes it the
disaster-recovery path ([keys-and-recovery](keys-and-recovery.md#c-the-catalog-is-gone-and-you-hold-the-operator-or-escrow-key)).
One unit's failure does not stop the others: the report lists every unit in
tape order with how it ended, and the command fails when any unit did
(`--fail-fast` stops at the first failure instead). `--dry-run` lists the set
without opening the drive.

**When a slice or a file comes back damaged (1.3.0).** Copies are written from
one staging, so a slice damaged on one cartridge is read from another that holds
it: tapectl asks for that cartridge by label and chip serial, resumes the slice
where the first stopped, and keeps the second in the drive while it serves. With
no terminal it refuses by name instead and lists the holders. Next, if files are
still damaged or missing, they are restored from another copy of the same
version that came from a different staging (a different stage set), reading only
the slices that hold them, and each is checked against the sha256 recorded for
the version. Last, what no copy can give is written as zeros (age authenticates
every 64 KiB chunk, so a defect costs 64 KiB and the files in it, not the rest
of the slice), and the restore exits non-zero naming the slice, its byte ranges
and the files. Use `tapectl drive eject` at the prompt. Another version is never
a source. `--json` carries `substitutions` and `filled`. RESTORE.sh, on the heir
path, takes a damaged slice from a directory of slice files (`--slices-from`) or
a second tape (`--swap-tapes`), fills listed files from another tape of the same
unit and version (`--fill-from`), and checks every restored file against its
recorded sha256; it cannot read around a damaged chunk, because it runs the
`age` program.

Before it opens the drive, a restore checks everything it can without the
tape, and refuses with nothing read
([troubleshooting](troubleshooting.md#restoring)):

- **Space.** A unit whose isolated catalogue from `stage create` is still on
  disk (under `~/.tapectl/catalogs/`) **streams**: each slice is decrypted
  off the tape straight into dar through a named pipe, so it needs only about
  the unit's size free in `--to`, and the drive keeps reading while dar
  extracts. Otherwise — a stage set whose catalogue is not on disk (a catalog
  rebuilt from a tape that carried none), or dar older than 2.7.9 —
  the restore **spools**: every slice is decrypted to disk before dar
  extracts them, in a `.tapectl-restore-tmp` directory inside `--to`, or
  inside `--scratch DIR` when you name one, never in the system temp
  directory; with both on one disk a unit then needs about twice its size
  free. `--spool` makes a unit that could stream spool anyway (to get round
  a streaming problem, or to rehearse what a rebuilt catalog will do), at
  that larger size. `restore volume` checks the set as a whole: every
  restored unit, plus the largest unit's spooled slices, since the units
  run one after another. `restore file` needs the slices it reads plus the
  one file. Pick a
  `--to` with room: `/tmp`, used in these small examples, is often RAM or the
  root filesystem. `--no-space-check` skips the check, for a compressed or
  thin-provisioned filesystem that holds more than it reports free.
- **The destination.** `restore unit` wants an empty or new `--to`
  (`restore volume`, an empty or new `DIR/<unit name>` for each unit), and
  `restore file` refuses when a file of the same name is already there.
  `--overwrite` restores anyway and replaces what collides.
- **A key that is not in the home.** `--key FILE` (repeatable) on `restore unit`,
  `restore file`, `restore volume` and `catalog rebuild` adds an age identity file to
  the keys tried, for that command only (ADR-0012 item 107). It is never copied into
  the home, registered as a key, or logged. An heir holding only the escrow secret
  runs `catalog rebuild --from-volume --key escrow.key`, then `restore unit --unit U
  --key escrow.key --to DIR`, and gets chunk salvage and file fill, which `RESTORE.sh`
  cannot do. A file that is not an identity is refused by name before the drive is
  opened.
- **The file.** `restore file --file` must be a path the catalog recorded for
  that version, relative to the unit's root, as `catalog ls` and
  `catalog search` print it. A directory is refused: restore the unit. dar is
  asked for that one entry, and with the unit's isolated catalogue on disk
  only the slices that hold it, its directories and dar's own catalogue (the
  last slice) are read off the tape; without it, every slice.
- **A streamed restore that fails partway** — a slice that fails its
  checksum, a read error — has already let dar extract what came before it
  into `--to`. The error says so: empty `--to` (or restore again with
  `--overwrite`) before you trust it. A spooled restore extracts nothing until
  every slice has been read.
- **A leftover scratch directory.** A restore that was killed, or lost power,
  leaves its `.tapectl-restore-tmp` behind, and it may hold decrypted data. The
  next restore refuses and names it; remove it by hand.

### Search the catalog

```bash
tapectl catalog search "episode01"
tapectl catalog search "episode01" --all-versions
tapectl catalog ls family/letters
tapectl catalog locate family/letters
tapectl catalog stats
```

`catalog search` looks in each unit's newest version, the one `catalog ls`
lists, and prints its hits in unit and path order; `--all-versions` lists a
file once for every version that holds it. `catalog stats` adds up each
version's own totals: `Files` counts the non-directory entries and `Total` the
bytes of regular files, over every version that still has a file list (a
purged version has none). Releases up to 1.1.0 counted directories as files
and a symlink's target length as bytes, so a figure noted then can read higher.

For each version the catalog records every entry's path, kind (directory,
regular file, symlink or special file), size, modified time, sha256 once
staged, and a symlink's target; each path is stored once per unit, however
many versions hold it. Permissions, owners, ACLs, extended attributes, device
numbers and hard links are not in the catalog: dar's catalogue is their record,
on the home disk under `catalogs/` and in every envelope on tape, and
`dar -l` lists them.

`catalog locate` is the one to know: every copy of a unit, where it is, and
whether it can be relied on.

```text
$ tapectl catalog locate family/letters
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| Volume  | Status | Condition | Location  | Snapshot | Slices | Written             | Serviceable | Warehouse | Escrow | Verified |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0001 | sealed | ok        | home-rack | 1        | 1      | 2026-09-29 08:20:01 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0002 | sealed | ok        | offsite   | 1        | 1      | 2026-09-29 08:20:05 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+

note: "Verified" is this catalog's last-known record of each copy's most recent PASSED full readback (a full `volume verify`, or a write's full confirm; a quick one reads none of the copy's bytes and does not count) — not a check of the tape performed just now. "never" means no passed full readback is on record, not that the copy is bad; an aged value does not mean the tape has since failed. Re-run `tapectl volume verify <label>` to refresh it.
```

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
[A typical write session](#a-typical-write-session)).

Each further copy goes on a different cartridge, because a cartridge holds one
volume at a time. What stops a second copy landing on copy 1's cartridge is
`volume init`, not the write. The init's File 0 check refuses a tape that
carries a sealed volume, with or without `--force` (ADR-0003). Reusing that
cartridge means saying copy 1 is gone first: `volume retire`, then `cartridge
erase` ([Erasing a cartridge](#erasing-a-cartridge)), which erases the tape in
the drive, reads it back and records it. If the tape was erased some other way
and its chip serial identifies the cartridge, `volume init` finds it blank and
goes ahead. It records the displacement, marks
copy 1's volume `erased`, and warns naming every unit left without a copy
(ADR-0010, ADR-0012). By then copy 1 is already gone.

Before the tape moves, the write checks each version against the copies it
already has (ADR-0012, 2026-10-07). If a copy is already at the destination
volume's location, the write goes ahead with a warning naming that copy.
Copies kept in one place are lost together, and `audit` still reports any
required location with no copy. The write also refuses a destination that
shares its cartridge with a live copy, but only an inconsistent catalog can
reach that: tapectl never records two live volumes on one cartridge.
`collection run` makes the same check before it stages anything, for the
staged sets that will ride along, and `--dry-run` prints the warning.

Once staging has been
cleaned, copy from an existing tape instead: read its encrypted slices back
into staging, then write them to a new cartridge with the full self-describing
layout.

```bash
# Read slices from tape into staging (source cartridge loaded)
tapectl volume read-slices --from L6-0001 --unit tv/breaking-bad/s01
# Swap in a blank cartridge, then initialise it and write
tapectl volume write L6-0002 --init
```

`--version N` reads one version; without it, every version of the unit on the
tape except those marked reclaimable or purged. The staging directory is checked
for room first, and a read that does not finish removes what it wrote (#474).

### Copies by location

From 1.7.1 a unit can say how many copies it wants at each location, not only
how many in all ([ADR-0012](adr/0012-copies-are-identical-content-cartridges-are-known-by-serial.md)
items 148-160). These are its **copy targets**, the `copies_at` map. A copy *at*
a location is an eligible tape whose recorded location is that one (set by
`volume move` or `--move-to`), or a recorded warehouse deposit there. Like every
count, it is judged per Version: a unit is as covered at a place as its
least-covered current Version, and a spanning Version as its least-covered slice.

**Setting targets.** Register the locations first. A target that names a location
`location add` has not made leaves every unit that takes it unresolvable, and
`config check` cannot see that, because the names are checked against the catalog
when a unit's policy is resolved.

```bash
tapectl location add home-rack --description "Home server rack"
tapectl location add offsite --description "Fire safe at a relative's house"
```

Then set the map at whichever layer fits:
- **one unit**, in its dotfile, a `[policy.copies_at]` table with one
  `location = count` line each:
  ```toml
  [policy.copies_at]
  home-rack = 1
  offsite = 2
  ```
- **an archive set**, `tapectl archive-set edit critical-media --copies-at
  home-rack=1 --copies-at offsite=2` (or `copies_at` in its `[[archive_sets]]`
  table, then `archive-set sync`). Its `required_locations` already counts as one
  copy at each name, and `--copies-at` counts are laid over those names;
- **every unit**, `copies_at = { home-rack = 1, offsite = 1 }` in config.toml's
  `[defaults]`.

The most specific layer that sets the map wins whole: a dotfile that names only
`offsite` drops the `home-rack` its archive set named. The unit's **copy floor**
becomes the larger of `min_copies` and the map's sum, so the dotfile above
(`home-rack = 1`, `offsite = 2`) asks for 3 copies in all at the default
`min_copies` of 2. For a tape-only unit the floor and each target are multiplied
by `[compaction] tape_only_safety_multiplier`, as before: at 2x that unit needs 6
copies, 2 at `home-rack` and 4 at `offsite`. Every key is in
[configuration.md](configuration.md#defaults).

A unit's source can go (tape-only, or a disk that is not mounted), and its
dotfile with it. So tapectl keeps the dotfile's own map in the catalog each time
`snapshot create`, `stage create` or `collection sync`'s adoption of the dotfile
runs with the dotfile present, and uses that copy while the dotfile is absent.
`location rename` rewrites a renamed location in the catalog's maps and in every
dotfile it can reach; change config.toml's `copies_at` by hand.

**What each unit still needs.** `report needs` gives every unit with a target or
short of its copy floor: its copies against the floor, and at each location its
target, its copies there and how far short. `--all` lists every unit, `--json`
gives the same facts.

```text
$ tapectl report needs
needs per location (3 unit(s)):
  family/letters (active): 0 of 3 copies, 3 short
    home-rack: target 1, 0 copies, 1 short
    offsite: target 2, 0 copies, 2 short
  family/photos (active): 0 of 2 copies, 2 short
    home-rack: no target, 0 copies
    offsite: no target, 0 copies
  media/music (active): 0 of 2 copies, 2 short
    home-rack: no target, 0 copies
    offsite: no target, 0 copies
```

With `--location`, it shows one place and each unit's need there, in two groups
(item 158). Group 1 is a unit whose target there is unmet. Group 2 is a unit short
of its copy floor that has no target there and no copy there yet; a unit holding
all its copies at one place is never in group 2 for that place.

```text
$ tapectl report needs --location offsite
needs at offsite (3 unit(s)):
  family/letters (active): target 2, 0 copies here, 2 short; 0 of 3 copies, 3 short — group 1: its target here is unmet
  family/photos (active): no target, 0 copies here; 0 of 2 copies, 2 short — group 2: short of its copy floor, no target and no copy here
  media/music (active): no target, 0 copies here; 0 of 2 copies, 2 short — group 2: short of its copy floor, no target and no copy here
```

`audit`'s `location_presence` reports a location short of its target
([Audit](#audit)), and `volume plan --policy-aware` and `collection plan
--policy-aware` count such a unit as one that needs a copy.

**Writing a tape for a location.** Staging and writing stay two commands, and the
first one never moves a tape:

```bash
# 1. The cartridge that will go to offsite: initialise it (it stays initialised
#    and empty until step 3)
tapectl volume init L6-0007
# 2. Work out what offsite needs, pack it into L6-0007's room, and stage it
tapectl stage for-location offsite --volume L6-0007 --dry-run
tapectl stage for-location offsite --volume L6-0007
# 3. Write only what offsite needs; once sealed and confirmed, the tape is
#    recorded at offsite
tapectl volume write L6-0007 --for-location offsite
```

`stage for-location` judges each unit's need at the location by the two groups
above, from the version a write would put on tape: a new or changed unit's next
version holds no copy anywhere yet, so it is in group 1 or 2 whatever its older
version holds. It then packs the units into the volume's own room, its capacity at
the drive's fill ceiling less the end-of-tape reserve (and on to its plan bound
when the drive declares `post_warning_bytes`):
- what is already staged for the location goes first, and is never staged again
  (when it does not all fit, nothing more is planned, and a warning names it);
- then a span in progress that continues there, then whole units, then units
  that may span, and within each, group 1 before group 2. Within group 1 the
  largest shortfall at the location comes first (0 copies there before 1 short),
  the fewest copies in all breaking ties; within group 2, the fewest copies in
  all; then the name (item 152);
- next-fit: the first unit that does not fit closes the tape, and the rest are
  named as waiting for the next tape for that location.

`--dry-run` prints the plan and stages nothing:

```text
$ tapectl stage for-location offsite --volume L6-0007 --dry-run
stage for-location offsite --volume L6-0007: 3 unit(s), 1.5 MiB, against the volume's 2.21 TiB under its fill ceiling (dry run: nothing was snapshotted or staged)
  "family/letters" (new version): group 1: 0 of its target 2 at offsite, 0 copies in all; 512.0 KiB — to snapshot and stage (never archived)
  "family/photos" (new version): group 2: 0 of its copy floor 2, none at offsite; 512.0 KiB — to snapshot and stage (never archived)
  "media/music" (new version): group 2: 0 of its copy floor 2, none at offsite; 512.0 KiB — to snapshot and stage (never archived)
then write them: tapectl volume write L6-0007 --for-location offsite
```

Without it, the command snapshots and stages new and changed units as
`collection run` does, and stages an unchanged unit's version from its source. A
unit whose source is gone (tape-only) is read back from a tape that holds it: the
command prints the `volume read-slices --from <LABEL>` lines, grouped by tape in
the order to run them, and you run them with each tape loaded in turn before the
write. A unit whose source is not there is named and skipped. A span in progress
whose next slices continue a range already on a tape (they start after slice 1)
goes only onto a tape for the location of that range (item 159); for any other
location it is named as waiting. A warehouse location is refused: a copy there is
a recorded deposit of a sealed tape, so the refusal names `volume deposit add` and
the tapes that would meet its targets.

`volume write --for-location` writes only the staged sets that still need the
location (and a span that continues there), names the others, which stay staged,
and refuses before the drive is touched when nothing staged needs it. Once the
tape is sealed and confirmed it is moved to the location, as `--move-to` does; a
different `--move-to` is refused. Plain `volume write`, and `collection run`, take
every staged set and know nothing of where the tape goes.

Staging is released at each unit's copy floor, as after any write (item 154). If a
target is still unmet at that point because the copies landed elsewhere, the next
tape for that location stages the unit again, from its source or by
`read-slices`. When staging has room, `--keep-staging-for-locations` holds it until
every target of each unit written is met, and the output names the locations still
short:

```bash
tapectl volume write L6-0008 --for-location home-rack --keep-staging-for-locations
```

### Mark Tape-Only

When local disk copies are no longer needed:

```bash
# Check the local files still match what was staged
tapectl unit check-integrity tv/breaking-bad/s01

# Then mark it tape-only (checks its copies and locations against its policy)
tapectl unit mark-tape-only tv/breaking-bad/s01
```

`check-integrity` exits 0 whether or not the files match, so read what it
prints: a `BITROT`, `MISSING` or `SIZE_MISMATCH` line means they do not.
It compares the files against the checksums of one version, the newest one
that has been staged, and names it. A file you changed or deleted since an
older version is not reported. A version you have snapshotted but not yet
staged has no checksums, so the check uses the version before it and says so.

```text
$ tapectl unit check-integrity family/letters
integrity check for "family/letters" against v1's checksums:
  OK:            1
  SIZE_MISMATCH: 1
    a.txt — SIZE_MISMATCH
```

`mark-tape-only` checks the unit against the floors it will be held to once it
is tape-only. They come from its own resolved policy (dotfile > archive set >
`[defaults]`): its copy floor, the larger of `min_copies` (2 by default) and
the sum of its `copies_at` targets; copies in `[defaults] min_locations`
distinct locations (2 by default); and its target at every location of its
`copies_at` map, by name (an archive set's `required_locations` counts as one
copy at each; see [Copies by location](#copies-by-location)). A tape-only
unit's copy floors, the copy floor and each location's target, are multiplied
by `[compaction] tape_only_safety_multiplier` (2 by default; ADR-0012,
2026-10-12). The number of distinct locations is not. So at the defaults it
needs 4 copies in 2 locations, and 2 copies at each named location. It also checks that nothing changed on
disk since its last snapshot. A unit that meets all of it is marked with no
question asked.

A shortfall is a risk you may knowingly accept (ADR-0008 Tier 2), so it is
asked about rather than refused: a terminal shows each shortfall and asks
`mark unit "…" tape-only — proceed? [y/N]`, `--force` or the global `--yes`
confirms in advance, and a session with no terminal and neither flag refuses,
naming each one:

```text
$ tapectl unit mark-tape-only tv/show/s01 < /dev/null
error: mark unit "tv/show/s01" tape-only refused: non-interactive session with no confirmation given — refusing rather than assuming consent (re-run with --yes to proceed)
insufficient copies: 0 < 4 required by unit "tv/show/s01"'s policy (tape-only 2x)
insufficient locations: 0 < 2 required ([defaults] location floor)
(`--force` or `--yes` confirms this in advance)
```

Two things no flag passes: a unit that was never archived (no snapshot, so no
tape holds it; ADR-0008 Tier 3), and a unit whose policy cannot be resolved,
such as one with a malformed dotfile. The gate will not guess at a policy.

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
`[defaults]` in config.toml. Not every key exists in every layer. A dotfile's
`[policy]` table takes only `checksum_mode`, `compression`, `slice_size`,
`warehouse_copies`, `allow_spanning` and `copies_at`. `required_locations` exists
only on an archive set; from 1.7.1 every layer can name places, with a count each,
through `copies_at` ([Copies by location](#copies-by-location)). `verify_interval_days` exists on an archive set and in
`[defaults]`, where it is 365 days when the file does not say, so every unit has one.

```bash
# Create a policy template
tapectl archive-set create critical-media \
  --min-copies 3 \
  --required-locations "home-rack,parents-house" \
  --verify-interval-days 180

# Apply the [[archive_sets]] tables in config.toml
tapectl archive-set sync
```

`--required-locations` takes the names of registered locations (see
[Locations and Movement](#locations-and-movement)), and they are checked by
name everywhere: `audit`, `unit mark-tape-only` and `snapshot mark-reclaimable`
all want a copy *at each named place*, not merely copies in that many places.
From 1.7.1 `required_locations` is shorthand for a copy target of one at each
name, and the names count towards the copy floor: a set whose
`required_locations` name more places than its `min_copies` needs one copy per
name. `--copies-at NAME=N` (repeatable) sets counts other than one.
A name that is not registered is refused, dry run or not:

```text
$ tapectl archive-set create critical-media --min-copies 3 --required-locations "home-rack,parents-house" --verify-interval-days 180
error: --required-locations names "parents-house", which is not a registered location (registered locations: home-rack). Register a location first with `tapectl location add <name>`, or fix the spelling.
```

`archive-set sync` writes only the keys each `[[archive_sets]]` table in
config.toml names. For those keys config.toml wins, so an `archive-set edit`
of one is undone by the next sync; change the file instead. A key the table
leaves out keeps whatever `create` or `edit` set. A table whose
`required_locations` names an unregistered location stops the sync before any
set is written.

```text
$ tapectl archive-set sync
sync: 1 created, 0 updated, 1 unchanged from config.toml
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

Exit codes: 0 = clean, 1 = warnings, 2 = violations, and 2 means nothing
else: an audit that stops on an error exits 70 (75 when the catalog was busy).
The audit is advisory: it changes the exit code and nothing else, and never
blocks a command.
`--action-plan` adds the command that fixes each finding:

```text
$ tapectl audit --action-plan
VIOLATIONS (4):
  [copy_count] family/letters: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
  ...
WARNINGS (1):
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
    fix: tapectl key escrow-kit --out <dir>
audit: 4 violations, 1 warnings (exit 2)
```

`location_presence` compares a unit's copy targets (its resolved `copies_at`
map, an archive set's `required_locations` counted as one copy at each name),
by name, with where its copies actually are: a missing place reads
`no copy at required location(s) offsite (policy requires home-rack, offsite; copies are in 1 location(s))`,
and its fix ends in `tapectl volume move <OTHER-LABEL> --to offsite`. A target
above one names its count: `has 1 copy at required location offsite, needs 2
(policy requires 1 at home-rack, 2 at offsite)`. A `required_locations` name that
is not a registered location counts as missing; a `copies_at` name that is not one
makes the unit's policy unresolvable. A tape-only unit needs
`[compaction] tape_only_safety_multiplier` copies at each named place
(ADR-0012, 2026-10-07): at the default 2x, one copy at `offsite` reads
`has 1 copy at required location offsite, needs 2 (tape-only 2x) (policy requires offsite)`.
Its `copy_count` is multiplied the same way (ADR-0012, 2026-10-12): a
tape-only unit with `min_copies = 2` and two copies reads
`has 2 copies, needs 4 (tape-only 2x)`. `copy_count` judges the copy floor,
the larger of `min_copies` and the `copies_at` sum, so a unit whose targets
add up to more than its `min_copies` needs that many copies in all.
`location_count` is the places half: a tape-only unit whose current Version sits in
fewer places than `[defaults] min_locations` (2 by default, never multiplied) reads
`has copies in 1 location(s), needs 2 ([defaults] min_locations)`.
`snapshot mark-reclaimable` asks the same of the version that supersedes.

### Reports

```bash
tapectl report summary
tapectl report fire-risk
tapectl report copies --unit family/letters
tapectl report needs --location offsite
tapectl report tape-only
tapectl report capacity --per-volume
tapectl report compaction-candidates
tapectl report events --days 30
tapectl report failed-stages
```

```text
$ tapectl report summary
tapectl summary
  Tenants:    2 (the operator not counted)
  Units:      4 active
  Snapshots:  4
  Volumes:    2 holding data (unwritten, retired, erased and quarantined not counted)
  Writes:     8 completed
  Total data: 7.0 MiB on tape
  Staging:    4 stage set(s) held, none owe a copy of their own version (`tapectl staging clean` decides which can be released)
```

The `Staging` line appears only while something is staged. When a stage set's
version still has fewer copies than its unit needs, it reads
`N stage set(s) held, M still owe a copy`, and `report pending` lists them.

From 1.7.0 a unit that allows spanning may be held across cartridges, as
ranges (runs of its slices, one per tape). A range is not a copy on its own,
so when one exists the `Writes` line counts them apart:
`Writes:     1 completed (each a version whole), 2 range(s) (part of a version across tapes)`.
`report copies` lists a tape that holds a range of a unit's current version
among its tapes, and names it on a `ranges:` line under the unit, with the
slices it holds.

`report failed-stages` lists every `stage create` that ended without staging
its unit, and the time each cost. A failed stage restarts its unit from the
beginning ([why](troubleshooting.md#why-a-failed-stage-starts-over); ADR-0012
item 91), so this is the record to judge that cost by. It reads only what
the catalog already keeps: when the attempt started, and the last moment it
recorded (its phases, dar's report, its source check).

```text
$ tapectl report failed-stages
failed stages (2):
  <unit> v<n> (stage set <id>): started <time>, failed <time>: <h> hours lost
  <unit> v<n> (stage set <id>): started <time>, no end recorded (the process stopped before it could record one): hours lost unknown
total: <h> hours lost, not counting 1 stage(s) with no end recorded
```

An attempt that stopped before it recorded anything (a crash, a power loss)
has no end, and is left out of the total. With none recorded it prints
`no failed stages recorded`. `--json` gives `failed_stages` (each with
`unit`, `version`, `stage_set_id`, `started`, `failed` and `hours_lost`),
`count`, `total_hours_lost` and `without_recorded_end`.

`report needs` (1.7.1) gives each unit's copy targets, copies and shortfall per
location; it is described under [Copies by location](#copies-by-location).

The full list of reports is in the [command reference](cli/report.md).

### Scripting against tapectl

The global `--json` flag makes a command print machine-readable output on
stdout; log lines always go to stderr. Only the commands in the first seven
rows put a finding in their exit status:

| command | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| `audit` | clean | warnings only | violations, and only violations: an error exits 70 | — |
| `volume verify` | every checked file matched | — | the medium is proven bad: the volume is quarantined and no longer counts as a copy | inconclusive: a drive or transport failure, or any error, including a command line that does not parse |
| `db fsck` | clean | findings that are not corruption (orphaned rows, repaired or not) | the integrity check failed, or an error | — |
| `collection sync`, `status`, `plan`, `run` | every unit ran | a unit was refused (its dotfile), or `sync` could not register a folder (invalid name, missing tenant or archive set), or (`sync`, `status`) something under the root belongs to no unit; the rest ran | an error (`run`: the write family's codes, below) | — |
| `host check` | quiet | something tripped | an error | — |
| the write family: `volume write`, `volume resume`, `collection run`, `quick-archive`, `volume compact-write`, `volume compact` | sealed and confirmed | (`collection run` only) sealed, and a unit was refused | an error with the medium untouched: nothing was written | the confirm was inconclusive; more codes below |
| `config check` | the config loads | — | it does not, or an error | — |
| every other command | it ran, whatever it found | — | an error, including a usage error | — |

The write family goes past 3 (ADR-0012, 2026-10-07; the full table is in
[Troubleshooting](troubleshooting.md#the-write-family-0-2-3-4-5-6-or-75)):

| exit | the tape and the session | next |
|---|---|---|
| 0 | sealed and confirmed (a step after it that failed is a `warning:` line) | nothing, or the command the warning names |
| 2 | untouched: refused before anything was written, or a usage error | fix the error, run it again |
| 3 | sealed; the confirm reached no verdict | `volume resume <label>` |
| 4 | interrupted part-way (a signal, a drive or disk error) | `volume resume <label>` |
| 5 | aborted: cannot resume (end of tape, a slice changed) | `volume write <label>` again |
| 6 | the medium proven bad, quarantined | another cartridge |
| 75 | the catalog was busy, nothing written | run it again |

`collection run` exits with the worst outcome among its copies, in the order
6, 5, 4, 3, 2, 0. A wrapper that retries can therefore act on the code alone:
resume on 3 or 4, write again on 5, page someone on 6. Older builds exited 2
for all of these.

Every other command reports what it finds in its output only, and exits 0
whatever that is. `unit check-integrity` exits 0 when files no longer match,
and `report fire-risk` exits 0 with units at risk. A script reads those from
`--json`: `unit check-integrity` gives the counts `bitrot`, `missing` and
`size_mismatch` (all 0 when every file matches), and `report fire-risk` gives
`at_risk`.

```text
$ tapectl --json unit check-integrity family/letters; echo "exit=$?"
{"bitrot":0,"details":[{"actual":12,"expected":6,"path":"a.txt","status":"SIZE_MISMATCH"}],"missing":0,"newer_version_without_checksums":null,"ok":1,"size_mismatch":1,"unit":"family/letters","version":1}
exit=0
```

`--help` exits 0 everywhere. Older builds exited 2 from `volume verify` for a
bad medium and for every error alike. A script that retried a verify on 2 must
now retry on 3, and replace the cartridge on 2.

With no terminal on stdin, a command that would ask for confirmation (an
ADR-0008 Tier-2 question, such as `unit mark-tape-only` below policy, or
`stage create` when the staging directory may be too small) refuses and
exits 2 rather than assume consent. Pass the global `--yes`, or the command's
`--force` where it has one, to confirm in advance.

These `--json` fields changed incompatibly from older builds:

| command | before | now |
|---|---|---|
| `report summary` | `staged_pending` | removed; `stage_sets_held` (every stage set in staging) and `stage_sets_owing_a_copy` (those whose version is still short of copies) replace it |
| `report summary` | `tenants` counted the operator | `tenants` excludes the operator |
| `report pending` | a row per staged stage set | a row only per stage set that still owes a copy, with new `copies` and `min_copies` (`null` when the unit's policy cannot be resolved) |
| `report compaction-candidates` | `total_bytes` | `data_bytes`: live plus reclaimable archive data, without the fixed per-volume metadata; `utilization` is `live_bytes / data_bytes`, and `flagged` also needs `reclaimable_bytes > 0` |
| `key list` | the escrow row's `key_type` was `"primary"` | `"escrow"` (`is_escrow` is unchanged) |
| `config show` | `defaults.min_copies_for_tape_only`, `defaults.min_locations_for_tape_only` | `defaults.min_copies`, `defaults.min_locations`; it mirrors config.toml, and a fresh one no longer has empty `collections` and `archive_sets` |

Additions a parser can ignore: `db backup` gains `keys_dir` (`null` when no
keys were copied), `size_bytes` (the copy it wrote) and `keys_bytes` (the key
files it copied, 0 when none), `db stats` gains `journal_growth` (see
[The journals are never pruned](#the-journals-are-never-pruned)),
`archive-set sync` gains `unchanged`, `archive-set info`
gains `warehouse_copies`, `preserve_xattrs`, `preserve_acls`, `preserve_fsa`
and `dirty_on_metadata_change`, `key import --reactivate` prints
`alias`, `fingerprint` and `reactivated`, and `unit check-integrity` gains
`version` (the version it compared against) and
`newer_version_without_checksums` (`null` unless a newer version is not yet
staged).
`report verify-status` gains `started` beside `completed`, and its text line
prints both, each named. `volume info`'s `first_write` is when the volume's
first write began (it was the time that write finished, the same as
`last_write`), for volumes written before this change too.
`collection sync` and `collection status` gain `outside`: each entry under the
root that belongs to no unit, as `path` and `kind`.

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
tapectl restore raw-volume --to /staging/MHVTLR3 \
  --from MHVTLR3
```

`--from` is a wrong-tape guard against the tape's own reported label, not a
catalog lookup. Expect output like `dumped 25 files … verified: 23
mismatched: 0 unverifiable: 2`. **`mismatched` must be 0.** The two
unverifiable files are always the front index and the seal marker — neither
can carry its own hash — so `2` is the correct number, not a warning.

Files land named `{position:04}_{type}.bin`, e.g.:

```text
0000_id_thunk.bin           0006_operator_envelope.bin
0001_system_guide.bin       0007_operator_envelope_backup.bin
0002_restore_sh.bin         0008_tenant_envelope_backup.bin
0003_front_index.bin        0009_tenant_envelope_backup.bin
0004_tenant_envelope.bin    0010_data_slice.bin  …  0023_data_slice.bin
0005_tenant_envelope.bin    0024_seal_marker.bin
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

`deposit add` checks the request, not the copy. It refuses a location that is
not a warehouse, and a volume that is not an eligible copy: one that is not
`sealed` (unsealed bytes are not final, so there is nothing durable to have
deposited), or one that is `sealed` but that a failed verify quarantined (its
`observed_condition` is not `ok`). No flag passes the quarantine refusal,
because it is a fact the catalog recorded, not a risk to accept. A volume holds
one deposit per warehouse: to record a new receipt, `deposit remove` the old
one first. There is deliberately **no checksum field** — tapectl did not perform the copy, so a checksum you typed
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
location presence, and **verification age**. The verification-age check
covers every unit that has a copy. Since 1.2.0 each unit has an interval: its
archive set's `verify_interval_days`, else `[defaults] verify_interval_days`,
which is 365 days when the file does not say (`tapectl config check` prints the
value in force). A dotfile `[policy]` key makes the unit's policy unresolvable,
so it cannot set one. `audit` produces your "what is overdue" list, and you do
not track it yourself. `tapectl report verify-status` is the same list by
recency (see [Monthly](#monthly--verify-a-rotating-slice-of-the-library)).
To change the interval for a group of units, set it on the archive set they
belong to (`tapectl archive-set edit <name> --verify-interval-days 180`). A unit
joins an archive set when it is registered (`unit init --archive-set <name>`).
To change it for every unit, `tapectl config set defaults.verify_interval_days
180`. There is no way to switch the check off; a very large number is the
nearest thing. Per ADR-0004 the check is advisory: it warns, it never blocks,
and a stale volume still counts as a copy.

**A new unit warns at once.** A write's quick confirm reads back the front index
and the seal marker, not the data, so it is no full readback. A unit written that
way has a copy and no full readback, so `audit` warns `verify_age` with `(last:
never)` the moment the write ends, and `no_full_verify` for the volume. Both
clear when a full `tapectl volume verify` passes, or when the write used
`--full-confirm`. That is expected, not a fault: a quick-confirmed tape has not yet
proved its data reads.

`tapectl report dirty` and `tapectl report pending` are the companion glance —
what has drifted since its last snapshot, and which stage sets still owe a
copy.

### Scheduling the advisory half

The weekly glance is the one part of the cadence a machine can do for you,
because it is read-only and needs no tape in the drive. A `first-run.sh`
install already has it: step 14 runs `scripts/install-systemd.sh`, which
installs three timers from `contrib/systemd/` — a weekly audit, a daily
catalog backup and a [daily drive poll](#the-daily-drive-poll) — plus the
`tapectl-op` wrapper. To install or re-render them on their own:

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
| 70 | no verdict: the audit stopped on an error (its message is in the journal) | failure |
| 75 | catalog busy: no verdict this run | neither (no `/fail` ping) |
| anything else | no verdict: the audit did not complete (a crash, say) | failure |

Warnings are not a failure on purpose. `audit` warns for ordinary drift — an
overdue verification (see [Weekly](#weekly--cheap-no-tape); a unit just written with a
quick confirm shows one until its first full readback), a source changed since its last snapshot, a Heir Kit
older than the newest tape — and ADR-0004 is explicit that the audit advises
and never blocks. A timer that alerts on exit 1 would turn the advisory audit
into a blocking one by the back door. A unit short of its copies or of a
required location is a violation, not a warning, so it exits 2 and the timer
unit fails: that is the finding you want to hear about.

`report verify-status` always exits 0; it runs for the journal record, not as a
check. The verification-age *check*, with a real exit code, is `audit`'s, and
it covers every unit that has a copy (365 days unless the archive set or `[defaults]`
says otherwise). The verify-status listing in the journal is the record of what was
read, and when.

Set `TAPECTL_HEALTHCHECK_URL` in the service to ping a healthchecks.io-style
endpoint (`/start` before, bare URL on 0 or 1, `/fail` on 2 and on any code
but 75, no second ping on 75, catalog busy). It is off unless set, and a missing `curl` or a failed
ping never changes the run's own result.

Two things the timers do **not** change:

- **They never write.** No tape command is scheduled, ever. The audit and
  backup services set `PrivateDevices=true`, so they cannot reach any device
  at all; the drive poll's service may open the SCSI generic node
  (`/dev/sg*`) and nothing else, so it cannot reach `/dev/nst*` even by
  mistake.
- **They are safe to fire during other work.** Opening the database runs
  the startup sweep, which marks a crashed write session `interrupted` and a
  crashed verification `aborted`. It tells a crashed session from a live
  one by the volume's lock (`locks/volume-<id>.lock`, held by `volume init`,
  `write`, `resume` through confirm, and `verify`; the kernel releases it
  when the process dies), so a timer that fires during a `volume write`
  leaves the live session alone. The sweep reads first and writes only
  when it has something to recover, so the audit and the backup read the
  catalog even while another command holds its write lock. If that lock is
  held longer than a command will wait, the command exits **75** ("catalog
  busy", [Exit codes](troubleshooting.md#exit-codes)). The wrappers report
  that as "busy, not run" and do not ping `/fail`.

The same lock guards the destructive commands. `volume abort`, `volume
resume` and `staging clean` (with or without `--force`) refuse a volume or
stage set whose session is live, and `--force` does not override that: the
running command is a fact, not a risk judgement.

#### What is in the drive right now

`tapectl drive status` shows what is in the drive right now (issue #428):

```bash
tapectl drive status            # the only configured drive
tapectl drive status --json
```

It reads the st driver's status (`MTIOCGET`) through a non-blocking open, then the
drive's identity and, with a ready cartridge, the cartridge chip, then the catalog's
cartridge and volumes for that chip serial. It never moves the tape, takes no lock and
writes nothing: no contact, no journal. A drive held by a tapectl command, or open in
another process, is reported with the holder's pid and command and is sent no SCSI
command, so it is safe to run during a write. It exits 0 whenever the probe ran (an
empty drive is an answer) and 2 when the tape node cannot be read or no drive is
configured; `--json` still prints the report.

#### Ejecting the cartridge

`tapectl drive eject` unloads the cartridge and tells the catalog it left (issue #429):

```bash
tapectl drive eject                # the only configured drive
tapectl drive eject --dry-run      # check and say what it would eject; send nothing
```

It reads the cartridge chip's serial, rewinds and unloads (what `mt offline` sends), and
records one `drive eject` contact naming the cartridge and the drive, and one `ejected`
event on the cartridge (on the drive if the chip is not registered). It changes nothing
else: the cartridge's volumes stay bound to it, because an eject is not a displacement
(ADR-0012, 2026-10-15 item 61), and its place stays what `cartridge move` last said; the
output names the command for where it is going. It refuses while any tapectl command holds
the drive or a volume (a write, a verify, a restore, a poll): exit 75, naming the holder's
pid and command, nothing sent to the drive. Once it has the drive it holds the drive lock
until it exits. An empty drive is not an error (exit 0, nothing recorded); a cartridge in a
drive that is not ready is refused (exit 2). It needs a configured backend; without one
`mt -f <device> offline` still works and the catalog is not told.

#### Profiling a drive

To measure a new drive model, firmware or cartridge generation and get its `[[backends.lto]]`
block, use `scripts/lto-profile.sh` on a test cartridge (it erases it); see
[Profiling an LTO drive](lto-profiling.md). Stop `tapectl-drive-poll.timer` first; the script
refuses while it is active.

#### Cleaning the drive

tapectl drives no changer, so cleaning is you inserting the cleaning cartridge when the
drive asks (`drive status` shows it; `drive poll` and `audit` print the cleaning TapeAlert
as "clean the drive"). `tapectl drive cleaned` writes down that it happened (ADR-0012,
2026-10-15 item 60):

```bash
tapectl drive cleaning-cartridge add CLNU01   # once per cleaning cartridge; rated for 50 uses (--rated-uses N)
tapectl drive cleaned --cartridge CLNU01      # after each cleaning
tapectl drive cleaned                         # the cartridge in the drive, if its chip names a registered one
```

Each cleaning is one `cleaned` event on the drive, naming the cartridge. Each cleaning
cartridge's uses and each drive's last cleaning are counted from those events, and
`tapectl report health` shows them once a cleaning cartridge is registered or a cleaning
recorded. A cartridge within 5 uses of its rating is flagged NEAR ITS RATED LIFE; at or past
it, SPENT, and `drive cleaned` exits 1: a spent cartridge does not clean, so replace it. The
drive says so too: TapeAlert 22 reads "replace the cleaning cartridge".

#### The daily drive poll

Between a write and the next verify a drive can go months unwatched, and the
audit cannot look: it reads no device. `tapectl drive poll` is the one
scheduled command that touches the drive (ADR-0012, 2026-10-07 item 29), and
it only *reads*:

```bash
tapectl drive poll                                     # the only configured drive
tapectl drive poll --device /dev/tape/by-id/scsi-HUJ808A5L4-nst   # several drives: name the one to poll
```

It sends three kinds of command to the backend's `device_sg`, and nothing
else: LOG SENSE (every log page the drive lists, each read once), READ
ATTRIBUTE (the cartridge chip, which answers only when a cartridge is loaded)
and INQUIRY. It **never** opens the tape node, moves the tape, loads, ejects
or rewinds a cartridge, or writes to a medium — and tapectl drives no changer,
so a scheduled load is not something it could do. Everything it reads is
recorded verbatim as a contact of its own (`cartridge_contacts.operation =
'drive poll'`): each log page in the log-page journal, the chip read in the
MAM journal, and one reading in `report health`. With no cartridge loaded, or
one the catalog does not know, the reading is the drive's alone; with a known
cartridge loaded the contact names that cartridge, and `audit` follows it to
the volume on it.

The TapeAlert page (0x2E) may clear as it is read, so the poll runs only when
no tapectl command has the drive: it takes the drive lock without waiting, and
when a command holds it, it reads nothing and exits 75. A poll can therefore
never take the alerts a write's or a verify's own reading is owed, within one
tapectl home: the lock is `<home>/locks/drive-<node>.lock`, so a command run
against another home (a throwaway rehearsal home, `--home` elsewhere) holds a
different lock, and the poll does not see it. Stop the poll's timer for such
runs (`sudo systemctl stop tapectl-drive-poll.timer`), as handoff (f) does for
the step-12 rehearsal. Nor does it
read a page the catalog cannot record: it takes the catalog's write lock and
records its contact before the first page, and reads nothing when it cannot (75
for a catalog another command is writing, 2 for one that refuses writes); a
page it read but could not record is named, printed in full, and exits 2.
**Reading
page 0x2E by hand (`sg_logs --page=0x2e`) can clear it the same way** —
debugging with `sg_logs` can erase the evidence you are looking for; run
`tapectl drive poll` instead, which keeps it.

| exit | meaning | unit result |
|---|---|---|
| 0 | recorded; the drive reported nothing | success |
| 1 | recorded; the drive raised a TapeAlert or reported an unrecovered error (named) | **failure** |
| 2 | no reading: the sg node could not be read, no drive is configured, or the catalog refuses writes — or a reading the catalog could not record (named; the output is its only copy) | failure |
| 75 | a tapectl command holds the drive, or another is writing the catalog; nothing was read | neither (`SuccessExitStatus=75`, no ping) |

Unlike the audit's warnings, a raised TapeAlert *is* a failure here: it is a
hardware fact, not a policy finding, and this is the check that exists to go
red. Ping it through its own healthchecks.io-style check, separate from the
audit's: set `TAPECTL_DRIVE_HEALTHCHECK_URL` in `tapectl-drive-poll.service`
(`/start` before, bare URL on 0, `/fail` on 1, 2 and anything else but 75; the
poll's output is the ping's body, so the check's log names the flags; a cleaning
flag (TapeAlert 20-23) is printed as the action it asks for, and is still exit 1). The
audit sees the same alerts from the catalog: `tape_alert` warns (exit 1) for
each live volume whose cartridge raised one not before the volume's last
passed full verify started, and names the flags; the remedy is `tapectl volume verify
<label>`, which either passes (the warning goes) or quarantines the volume
(and the copy checks take over).

The timer runs daily at 06:30 (30 minutes' jitter, `Persistent=true`): a poll
costs seconds and no tape motion, and a drive idle for months is the one whose
first sign of trouble should not wait a week. The service runs as the service
user with `DevicePolicy=closed` and `DeviceAllow=char-sg rw` — every
`/dev/sg*`, whatever its number after a reboot, and no `/dev/nst*` — and it
needs that user in the group that owns the sg node (usually `tape`;
`first-run.sh` step 6 adds it). The audit's service keeps
`PrivateDevices=true`.

### Monthly — verify a rotating slice of the library

Do **not** try to verify every volume every month. The rotation exists to bound
drive and tape wear, which is the same reason snapraid scrubs ~8% of an array
per pass rather than all of it. tapectl has no percentage selector and computes
no rotation for you — this is a human procedure driven by one report:

```bash
tapectl report verify-status                     # verification recency, oldest first
tapectl volume verify L6-0003                     # --full is the default
```

The report lists every verification session, not one line per volume.
`never verified` lines come first, then sessions oldest first, so a volume
verified twice appears twice. A full readback is dated by when it *started*:
one that continued an interrupted readback started, as far as its oldest file
is concerned, when that file was read, and its `started` time says so. `audit`'s
`verify_age`, `volume list`'s VERIFIED column and the evidence lines the
destructive commands print date it the same way (ADR-0012, 2026-10-07). Judge
each volume by its newest line:

```text
$ tapectl report verify-status
  L8-0001: full passed, started 2026-09-28 23:14:34, completed 2026-09-28 23:14:34 (13/13/0 checked/passed/failed)
  L8-0001: full passed, started 2026-09-28 23:14:35, completed 2026-09-28 23:14:35 (13/13/0 checked/passed/failed)
  L8-0002: full passed, started 2026-09-28 23:14:39, completed 2026-09-28 23:14:39 (13/13/0 checked/passed/failed)
```

**First, every newly written volume.** A write's confirm reads back only the
front index and the seal marker unless you passed `--full-confirm`, so a new
volume has had none of its data read back. Its units show `verify_age` with
`(last: never)` in `audit` until it has one (a warning, and expected). Give it a full `volume verify`
soon after the write — pass `--keep-staging` to the write that completes a unit's
copies (otherwise that write releases staging), so the verify runs while
the staged slices can still rewrite a bad copy cheaply. `collection run` gives
no such window: it releases staging right after its quick confirms, so use its
`--full-confirm-first` (or `--full-confirm`, every copy) when the data matters (see
[A typical write session](#a-typical-write-session)). `report
verify-status` ends with the volumes still owed one:

```text
  sealed with no full readback recorded — run `tapectl volume verify <label>`:
    L8-0002
```

and `audit` warns about each (`no_full_verify`), and about each unit on it
(`verify_age`), until a full verify passes. A
`--quick` verify does not clear it. Until then the volume does count as a copy
— the quick confirm proved it is sealed and navigable — but `volume resume`
will not adopt an aborted session on it without a full verify on record.

Then the rotation: pick the N volumes whose newest pass is oldest, such that
**every volume gets one full pass within your verification interval**. If you hold 24 volumes on a
12-month interval, that is roughly 2 per month. The interval `audit` checks is
`verify_interval_days`: an archive set's (`tapectl archive-set edit <name>
--verify-interval-days N`), else `[defaults] verify_interval_days` (`tapectl
config set defaults.verify_interval_days 180`, #453), which is 365 days when the
file does not say (1.2.0). So `audit` always checks it, and `tapectl config check`
prints the value in force.

`audit` judges each volume on its own as well: `volume_verify_age` warns when a
volume's newest full readback is older than the shortest interval of the units
on it, and names the volume, its cartridge and its location (#454), so a fresh
home copy no longer hides an offsite copy nobody has read. Once a year, swap the
home and offsite copies and verify the one that came home, so no tape goes
unread for more than a year plus the interval.

Two tiers, and the distinction matters:

| Tier | Cost | What it proves |
|---|---|---|
| `volume verify --full` (default) | Reads and hashes every content file | Media still returns the exact bytes the front index recorded |
| `volume verify --quick` | Seal binding + front index self-consistency only | Tape is still *navigable* — nothing about content integrity |

`--quick` is a triage tool for a tape you are about to move or a suspicion you
want to rule out fast. It is not a substitute for a full pass, and a `--quick`
run should not reset your sense of when that volume was really verified.

The exit status says which of three things happened, so a script needs no
`--json` to tell them apart:

| exit | meaning | what to do |
|---|---|---|
| 0 | every checked file matched | nothing |
| 2 | the verify **proved the medium bad**: the volume is quarantined and no longer counts as a copy | write its content to another cartridge |
| 3 | **inconclusive**: a drive or transport failure, or an error before any verdict (no cartridge loaded, the wrong tape, an unknown label, a File 0 saying this tapectl cannot read the tape, a mistyped command line); the volume is untouched | check the drive, then verify again |

An empty drive is refused at once with `no cartridge loaded in <device>`, by
`volume verify` and by every other command that reads or writes a tape.

A full verify that stops part-way (Ctrl-C, a dropped ssh session, a reboot)
is recorded `aborted` with every file it had read back clean. Run the same
`volume verify` again and it continues: it reads File 0, File 3 and the seal
marker as always, skips the files already read back clean, and reads the
rest. It continues only the volume's latest readback (a write's interrupted
`--full-confirm` included), only while the front index on the tape is the
one those files were checked against, and only while the oldest of those reads
is within the volume's `verify_interval_days` (the shortest among the units on it:
an archive set's, else `[defaults]`'s, 365 days when absent; no limit only on a volume
no unit was written to); otherwise it reads
everything. A verify that continues is recorded as started when its oldest
carried read was made, not when you re-ran it — `report verify-status` and
`volume info` show that time (ADR-0012, 2026-10-07).

`volume verify` opens the drive read-only, so leave a sealed cartridge's
write-protect tab set: it verifies without sliding the tab. So does a
`volume resume` that only re-confirms a recorded seal.

#### Reading the corrected-error trend

A verify that passes can still be a warning. The drive corrects read errors
as it goes, and a cartridge whose surface is wearing makes it correct more of
them each year, long before a sha256 fails. So every completed full `volume verify`
records the read errors its drive corrected (log page 0x03: corrected with and
without delay, rereads, uncorrected), each per GiB the drive read, as a
`verify_read_errors` event naming its verification session. There is no
separate scrub command (ADR-0012, 2026-10-06): your rotating verifies are the
scrub.

`tapectl report health` shows the figures per cartridge, oldest verify first:

```text
$ tapectl report health
Corrected read errors per GiB, by cartridge, verify over verify:
  EW7VWMVKF6: 0.120 -> 0.450 -> 1.800 corrected/GiB over 3 verifies (uncorrected 0, 0, 0)
    ** RISING — corrected read errors per GiB rose from 0.450 to 1.800 between its last two verifies, ... **
```

A cartridge is flagged when its newest verify corrected more than
`read_error_rise_factor` times as many errors per GiB as the verify before it
**and** more than 1 corrected error per GiB, and `tapectl audit` warns on it
(`read_error_trend`). The factor lives in `config.toml` and is
**provisional**: 2.0 is a starting point, not a measured threshold, to be set
once home2's verifies show what a healthy cartridge looks like. The floor of 1
per GiB is fixed (ADR-0012, 2026-10-07): from a verify that corrected nothing,
any later figure is a rise past every factor, so without it a cartridge's first
corrected error would be an alarm. A cartridge going from 0 to 0.5 per GiB is
not flagged; one going from 0 to 2 per GiB is.

```toml
[health]
read_error_rise_factor = 2.0
```

`report health --json` carries the same figures: an object whose
`read_error_trends` holds one entry per cartridge (its verifies, oldest first,
in `points`, and `rising` set — with the line above as its `message` — when it
is flagged), beside the `read_error_rise_factor` and
`read_error_rise_floor_per_gib` it was judged against and the health readings
under `readings`. (Before the trends joined it, the document
was the readings array alone.)

How to read it:

- Only full verifies are recorded. `volume verify --quick` reads File 0, the
  front index and the seal — a few MB — and a rate over that is a sample of
  nothing, so a quick verify (before moving a tape, say) adds no point and
  cannot make a healthy cartridge look as if it is rising.
- A rate of `-` is a verify whose drive counted no bytes read on page 0x03
  (mhvtl always does this). It is skipped, never read as zero.
- The counters are the drive's since it last cleared the page, which the HP
  LTO-6 does when a cartridge is loaded. Verify a cartridge on a fresh load and
  the figure is that verify's readback.
- One verify's rate means little; the same cartridge's rates over time are the
  signal. A climb that shows on one drive and not on another is the drive, not
  the tape: compare across drives before you act (`report health` lists each
  reading's drive).
- Rising, with the data still verifying: copy its units to a fresh cartridge
  while it still reads (`volume read-slices --from <LABEL> --unit <UNIT>`, then
  `volume write` to the new one), then retire it. Once the cartridge is
  `retired_permanent` or `pending_erase` (or, for a trend shown as
  `volume:<label>`, the volume is retired or erased) it drops out of the trend
  and the warning clears: it will not be verified again, so its last two
  points would otherwise warn forever.

### Annually — the heir-path restore drill

The drill that matters is not "can tapectl restore this" — it is **can someone
who is not you, without this database and without this binary, get the data
back**. Run it from real media once a year.

The procedure already exists in
[`docs/lto6-validation-checklist.md`](lto6-validation-checklist.md) — see its
*Disaster recovery from the real tape* section (the "heir script alone (no
tapectl)" item). Follow it there rather than a second copy here;
two drifting checklists are a failure waiting to happen. The drill's
essentials: load a real tape, pull `RESTORE.sh` off the plaintext front zone
with `mt` + `dd`, and run it using **only** the tools the tape's own guide
lists — `mt`, `dd`, `age`, `dar`, `sha256sum`, `head`, `truncate`, `tar`, `mkfifo`
and `tee` — plus the shell tools any Linux system has, and no `tapectl`, against
nothing but the printed key material.

Nothing automated performs a drill; schedule it yourself, on real hardware.

While you are there, confirm your off-tape recovery inputs are current:

```bash
tapectl db backup --to /path/to/backup/tapectl.db   # add --include-keys only if the
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

It writes four files:

| file | what to do with it |
|---|---|
| `COVER.txt` | **Print this.** The plain-text cover sheet. It carries the escrow *identity* (the public half, `age1…`, in retypable Bech32) and a boxed hand-fill area for the escrow *secret* (`AGE-SECRET-KEY-1…`), which `init` printed once and nothing on the machine holds. The printed `age1…` decrypts nothing by itself; the sheet says so. It is the artifact with the decades-scale claim — readable with `cat` when no browser exists. |
| `escrow-kit.html` | Same content with the identity as a QR, captioned as the identity. Open it and use the browser's print dialog. Self-contained: it renders with no network. |
| `catalog.db.age` | The whole catalog, encrypted to the escrow recipient. Put it on the media that travels with the paper. |
| `tapectl-<version>` | A copy of the tapectl binary that ran the command (mode 0755), from 1.5.0. The cover sheet names it with its size and sha256 and says it is for Linux on one machine type; File 1 on every tape sends an heir there first. Put it on the same media as `catalog.db.age`. Run the installed production binary to make the kit, since the copy is whatever binary ran; after an upgrade, delete the older `tapectl-<oldversion>` the new kit leaves beside it. |

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

**Re-run it after every write session.** The escrow secret opens every tape
ever written, old and new, because the escrow recipient is on all of them.
What goes stale is `catalog.db.age`: it lists only the volumes that existed
when the kit was made, so an heir holding an old kit can decrypt a newer tape
but finds nothing in the catalog about what it holds or where it is kept.
They can still rebuild the catalog from that tape
(`catalog rebuild --from-volume`); a fresh kit spares them that. You are not
expected to remember: `audit` warns (`escrow_kit_stale`, or
`escrow_kit_missing` if you have sealed tapes and no kit at all) whenever
volumes were sealed after the last generation. It is advisory and never
blocks — exit 1, never 2.

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
tapectl report cartridge-health        # is this cartridge fit to be the copy that leaves?
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

# Mark old snapshots as reclaimable (checks the version that supersedes each)
tapectl snapshot mark-reclaimable tv/breaking-bad/s01 --version 1

# Three-step compaction, on one drive
tapectl volume compact-read L6-0001                           # source loaded
# (swap in a blank cartridge)
tapectl volume init L6-0010
tapectl volume compact-write --destination L6-0010
tapectl volume compact-finish L6-0001                         # retires the source

# Or all three in one flow, at a terminal: initialise the destination first
tapectl volume init L6-0010                                   # destination loaded
# (swap in the source)
tapectl volume compact L6-0001 --to L6-0010
```

A volume is a candidate when its live data falls below
`[compaction] utilization_threshold` (50% by default) of the archive data on
it: live data plus the data of reclaimable and purged snapshots, with the
fixed per-volume metadata left out. A volume with nothing reclaimable is never
a candidate however little it holds, so a freshly written tape never is. The
report prints a line for every sealed volume holding data (a quarantined one
is left out), marks a
candidate `*** CANDIDATE ***`, and ends with `no compaction candidates` when
none qualifies:

```text
$ tapectl report compaction-candidates
  L8-0001: live data is 100% of the archive data on this volume (1.7 MiB live, 0 B in reclaimable/purged snapshots)
  L8-0002: live data is 100% of the archive data on this volume (1.7 MiB live, 0 B in reclaimable/purged snapshots)
no compaction candidates (threshold: live data below 50% of the archive data on a volume)
```

`snapshot mark-reclaimable` releases an older version once a newer current
version supersedes it, and first checks that the superseding version is
covered on its own: `min_copies` copies, and a copy at each of its
`required_locations` by name (for a tape-only unit, every copy floor multiplied
by `[compaction] tape_only_safety_multiplier`, as everywhere a tape-only unit is
judged: the copies, and the copies at each named location — two at `offsite` at
2x; never the number of places, ADR-0012 2026-10-12). A shortfall goes through the same
Tier-2 consent as `mark-tape-only`: a terminal asks, `--force` or `--yes`
confirms in advance, and a session with no terminal and neither flag refuses,
naming it (for example
`superseding v2 has no copy at required location(s) offsite (policy requires home-rack, offsite)`).

`--force` accepts a shortfall; it does not skip the check. When a superseding
version exists, the unit's policy is resolved before any coverage is checked,
even under `--force`. A policy that cannot be resolved, such as a dotfile with an invalid
`[policy]` value or a unit whose archive set the catalog no longer has, is an
error rather than a shortfall. Fix what it names, then run the command again:

```text
$ tapectl snapshot mark-reclaimable tv/show/s01 --version 1 --force
error: unit "tv/show/s01" has an invalid [policy] compression in /media/tv/show/s01/.tapectl-unit.toml (invalid compression "bogus": accepted values are none, gzip, bzip2, lzo, xz, lzma, zstd, lz4)
```

A version that no current version supersedes is different. Marking it would
release the unit's only current version, which is not a shortfall to accept at
a prompt, so neither a terminal's `y` nor `--yes` passes it; only an explicit
`--force` does:

```text
$ tapectl snapshot mark-reclaimable tv/show/s01 --version 1
error: no superseding current snapshot exists for v1 — marking it reclaimable would release "tv/show/s01"'s only current version. That is not a shortfall a prompt or --yes can accept; only an explicit --force overrides it.
```

`volume compact` reads and writes through one drive, so it **always pauses
after step 1** for you to unload the source and load the destination, with or
without `--to`. With `--to` the pause asks only for the swap:

```text
Unload "L6-0001" from /dev/tape/by-id/scsi-<SERIAL>-nst and load the destination "L6-0010", then press Enter to write it (Ctrl-D stops here):
```

Without `--to` the same pause also asks for the destination's label. Because
someone has to make the swap, `volume compact` needs a terminal: without one
it refuses before reading anything and prints the three separate steps above,
which are the way to compact unattended. Ctrl-D at the pause stops before
step 2 with the source's live slices staged; the message names the
`compact-write` and `compact-finish` that finish the job.

Step 3 retires the source without asking in an ordinary compaction, because
the destination now carries every live slice the source did. It asks only
when retiring the source would leave some unit below its policy. `--force` or
`--yes` answers that question in advance (the swap still waits for you). If
you decline, the destination is already written and sealed, and
`volume compact-finish <source> --force` completes the job. Step 3 refuses
outright, and no flag passes it, when a live slice on the source has no copy
on any other volume, or when the source holds the last eligible copy of a live
version.

### Compacting in place: `volume rewrite`

`volume compact` needs a second cartridge. `volume rewrite` reclaims the space
on the cartridge a volume is already on, with one drive and one tape (ADR-0012
item 106). It reads the volume's live slices into staging, retires the volume,
erases its cartridge, initialises a new volume on the same cartridge, writes the
staged slices to it with the full confirm (every file read back), and only then
releases staging. It works when the volume holds the only copy of a unit.

```bash
tapectl report compaction-candidates --plan          # what to compact, grouped by what fits one tape
tapectl volume rewrite L6-0001 --label L6-0011 --dry-run
tapectl volume rewrite L6-0001 --label L6-0011       # L6-0001 loaded
tapectl volume rewrite L6-0001 --label L6-0011 --only-copy-on-staging L6-0001   # unattended
```

Before anything is read it checks that the source is sealed and trusted, that
`--label` is free, that the cartridge carries no other live volume, that the
drive can write its generation, that the live data fits one tape and that
staging has room. It asks the quiet-host question once and reads the chip
serial. After the read it re-reads every staged slice from the staging disk and
matches its hash, checks what the write would refuse (keys, escrow, a staged set
not whole), and builds the new volume's real layout from the staged slices (the
envelopes with their catalogues, the cross-volume index, `catalog.db`, the
backups, block padding), holding its total, plus a margin and the end-of-tape
reserve, to the cartridge's budget. A refusal at any of these changes nothing,
with the figures named.

Consent depends on what else holds the data:

- Every live slice has a copy on another sealed volume: the usual Tier-2
  prompt, and only when a version would fall below its policy (a terminal asks,
  `--yes` confirms in advance, no terminal and no `--yes` refuses).
- Some live slice has no copy anywhere else: the retire family's zero floor is
  amended for this command alone. The units and versions, the staging path and
  the window are shown, and the source's label must be typed at a terminal, or
  given as `--only-copy-on-staging L6-0001` for an unattended run. `--yes` does
  not give it, and the command has no `--force`. `volume retire`,
  `volume compact-finish`, `cartridge retire` and `cartridge mark-erased` keep
  the absolute floor.

**The window.** From the erase until the new volume seals and passes a full
readback, the staging disk holds the only copy of those units. The window is
named in the `rewrite_window_opened` and `rewrite_window_closed` events and in
the session log. If the command stops in it, the staged slices stay staged and
nothing releases them (`staging clean --force`, the release after
`volume write`/`volume resume`, and `collection run` all refuse). The error
names `tapectl volume rewrite --resume L6-0011`, which continues from where the
catalog says it stopped and lifts the hold once the new volume has passed the
full readback. `--with-staged` writes whatever else is staged beside the
rewrite's slices (without it the new volume carries the rewrite's slices and
nothing else); `--keep-staging` releases nothing afterwards.

`report compaction-candidates --plan [--generation LTO-N]` groups the flagged
volumes whose live data fits one destination tape (capacity times the fill
ceiling, less the end-of-tape reserve), largest reclaim first, and names the
command: `volume rewrite` for a group of one, or `compact-read` the others and
then `volume rewrite HOST --with-staged` for a group of several.

Exit status is the write family's: 4 means it stopped after the tape was
touched and `--resume` finishes it.

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
tapectl volume init L8-0001
```

`volume init` reads the chip, finds no cartridge registered under that serial,
and registers one — using the serial itself as a placeholder barcode, because
that is the only identifier the tape carries:

```text
cartridge E01001L8_1775794348 auto-registered from MAM (barcode = medium serial)
volume "L8-0001" initialized (id=1)
medium: LTO-8, 12.00 TB (the cartridge chip's medium density code 0x5e)
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
tapectl volume init L6-0002
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
tapectl volume init L6-0002 --cartridge L6-0002
```

One safeguard on this path: if the barcode you name is still carrying a live
volume, tapectl refuses rather than displacing it. With no serial it cannot tell
whether that cartridge was erased or whether you have loaded a different tape
wearing its sticker, so decide which it is. If you loaded the wrong tape, load
the cartridge you meant and run `volume init` again naming its barcode (or
without `--cartridge` when the drive reads a medium serial), and leave the named
cartridge alone. If it really is that cartridge and you are reusing it, say the
bytes are gone first — `volume retire <volume>` or `cartridge mark-erased
<barcode>` — and run it again. Both refuse while that volume holds the last copy
of any unit (Tier 3: no `--force`, and `--yes` does not reach it), so make that
copy on another cartridge first; the refusal prints the steps (#468). There is no
`--force` for the init itself: this is something tapectl cannot know, not a risk
for you to accept.

### Erasing a cartridge

`cartridge erase` overwrites the tape in the drive, carefully (ADR-0012,
2026-10-12). It is the way to reuse a cartridge whose volume is given up, or a
tape some other catalog wrote, and the way to wipe one before it leaves your
hands. In order, it:

1. **identifies the tape, read-only**, and prints what it is: the chip serial;
   File 0's label, uuid, layout version, the tapectl that wrote it and when;
   the seal and its front-index totals. A tape tapectl did not write is
   reported as such. A blank tape is reported blank, and the command stops
   there: there is nothing to erase.
2. **checks the catalog**, by File 0's uuid and by the chip serial. A volume
   the catalog still holds is refused, naming `volume retire`: retiring is the
   only way to give a volume up, and the retire is where its last-copy floor
   is enforced. `--dry-run` stops here and prints the label to confirm with.
3. **asks for the label** it just read from File 0 to be typed (for a tape
   tapectl did not write, its chip serial). Unattended, `--confirm-label`
   gives it and must match. `--yes` never does. If the chip serial could not
   be read (the MAM read on the drive's `device_sg` failed), it refuses here
   instead: nothing else proves afterwards which cartridge was erased.
4. **erases**, once the chip, read again with the drive held open, still
   reports the same serial (a cartridge swapped while you were asked is
   refused, untouched): by default a filemark at the start of the tape, in seconds;
   File 0 then reads EMPTY and nothing past it can be read. `--long` erases
   the whole medium, which takes hours on LTO.
5. **reads it back**: File 0 must be EMPTY (or blank, after `--long`) and the
   chip serial unchanged. Anything else is an error, and nothing is recorded.
6. **records it**: an event, the drive contact, and the cartridge. A cartridge
   the catalog did not know is registered by its chip serial as `available`; a
   known one has its retired volumes marked `erased` and becomes `available`,
   as `cartridge mark-erased` would do. A `retired_permanent` cartridge stays
   retired.

```bash
tapectl cartridge erase --dry-run                  # what is on it, and its label
tapectl cartridge erase                            # type the label when asked
tapectl cartridge erase --confirm-label L6-0001    # unattended
tapectl cartridge erase --long                     # the whole medium, hours
```

After either erase `volume init` needs no `--force`. After the default short erase
it reads File 0 as EMPTY and takes this catalog's record of the verified erase, with
nothing written to the cartridge since, as the proof (ADR-0012 item 57; it prints
`blank proven by this catalog's verified erase on <date>`). After `--long` the tape is
blank. A by-hand `mt weof 1` records nothing and still needs `--force`
([troubleshooting](troubleshooting.md#file-0-identifies-a-different-volume-or-is-empty)). The exit
codes are in [troubleshooting](troubleshooting.md#cartridge-erase-0-2-3-4-6-or-75).
Never degauss or bulk-erase an LTO cartridge: that destroys its factory servo
tracks.

### Everything else

```bash
tapectl cartridge list                      # barcode, generation, status, location, volume
tapectl location add offsite-vault -d "the bank's safe deposit box"   # a place must exist before anything moves there
tapectl cartridge list --location offsite-vault
tapectl cartridge info L6-0001
tapectl cartridge edit L6-0001 --generation LTO-5  # the registration was wrong about the medium
tapectl cartridge move L6-0001 --to offsite-vault   # the cartridge and every volume on it
tapectl cartridge retire L6-0001            # worn out or too many errors: never write it again
tapectl cartridge unretire L6-0001          # you were wrong about the medium; undo the retire
tapectl cartridge erase                     # erase, verify and record (above)
tapectl cartridge mark-erased L6-0001       # after erasing it some other way (mt erase; never a degausser)
tapectl cartridge relabel L6-0001 L6-0001-B  # the sticker changed; identity did not (later commands use the new barcode)
```

```text
$ tapectl cartridge list
+---------------------+-------+--------+-----------+-------+---------+
| Barcode             | Type  | Status | Location  | Loads | Volume  |
+---------------------+-------+--------+-----------+-------+---------+
| E01001L8_1775794348 | LTO-8 | in_use | home-rack | 564   | L8-0001 |
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
for LTO-6 (decimal terabytes, as printed on the cartridge). `volume init` says what it took the
cartridge for on its last line, for example `medium: LTO-5, 1.50 TB (the cartridge chip's
medium density code 0x58)`; the same line follows `volume write --init`, on stderr. `volume
list` shows the GENERATION and CAPACITY of every volume. There is nothing to
change between tapes and nothing to remember. If you load media the drive
cannot write, `volume init` refuses before touching the tape, and no `--force`
overrides it.

`first-run.sh` fills `generation` in from the drive's own INQUIRY product
identification — never from whatever cartridge happens to be loaded, which is a
fact about that tape and not about the drive. If it is ever wrong (you moved
the config to a different drive, or declared it by hand), the refusal above
names the `[[backends.lto]]` block to fix, and the command that corrects it:
`tapectl config set 'backends.lto[NAME].generation' LTO-N`, then `tapectl config check`.

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
| family-backup  | backup  | yes    |        | age1k83yzew0lqgc2ghf7ktc... | 2026-09-29 08:19:49 |
+----------------+---------+--------+--------+-----------------------------+---------------------+
| family-primary | primary | yes    |        | age194da4h246gzse65r0a3k... | 2026-09-29 08:19:49 |
+----------------+---------+--------+--------+-----------------------------+---------------------+
```

The operator's own list (`key list --tenant mike`) also shows the escrow
recipient, as type `escrow`, flagged `ESCROW (ADR-0005)`.

Old keys are never deleted — only deactivated. Restore tries every key file of
the unit's tenant, deactivated ones included, and the operator's; which files
are a tenant's comes from the catalog's key rows. A key file with no row goes
to every tenant whose name, followed by `-`, begins the file name. A catalog
built by `catalog rebuild` alone has no key rows, so there this applies to
every file: until `key import` registers the key again,
`family-old-laptop.age.key` is tried for both `family` and `family-old`.

`key rotate` refuses until an escrow recipient is registered, and never
touches the escrow key itself. To
make a deactivated key a recipient of new writes again, import its public key
file with `--reactivate`; it keeps the alias it was registered under:

```bash
tapectl key import --tenant family --reactivate ~/.tapectl/keys/family-primary.age.pub
```

## Database Operations

```bash
tapectl db backup --to /backup/tapectl.db
tapectl db fsck --repair
tapectl db stats
tapectl db export > tapectl-catalog.json   # every table, as JSON
```

```text
$ tapectl db backup --to /tmp/tour/backup/tapectl.db
database backed up to /tmp/tour/backup/tapectl.db (464.0 KiB); private keys not included — pass --include-keys to copy them
```

`--to` names the backup *file*, and its directory must already exist:
`db backup` creates no directories, and refuses a `--to` that is itself a
directory. `--include-keys` also copies the private key directory beside the
file, named for it with the extension replaced by `.keys`
(`--to /backup/tapectl.db` puts the keys in `/backup/tapectl.keys/`); treat that
directory as secret wherever it ends up.

A `first-run.sh` install already backs the catalog up daily to a second disk
([install.md §7](install.md#7-the-timers-and-the-operator-wrapper)). Run the
backup service by hand at the end of every write session too
(`sudo systemctl start tapectl-backup.service`). A backup taken mid-session,
after `volume init` and before `volume write`, restores that volume as
`initialized`, and rebuilding the catalog from the sealed tape attaches its
units to the row but does not seal it, so nothing on it counts as a copy until
a full `volume verify` and then `volume resume` adopt it (#360; see
[keys-and-recovery.md](keys-and-recovery.md), runbook D).

`db fsck` runs SQLite's `integrity_check` and `foreign_key_check` and reports
every violation it finds. `--repair` deletes rows whose foreign-key parent is
missing, closing the graph in one transaction, and logs what it deleted — if
anything is still dangling at commit the whole repair rolls back, so a failed
repair leaves the database exactly as it was.

Run it on a database you brought in with `db import` before you use it:
`db import` is a raw page copy and validates no foreign keys of its own.

### The journals are never pruned

The catalog keeps every reading it takes of a tape, a drive and a write:
`health_logs`, `verification_sessions` and `verification_results`, `events`,
`cartridge_contacts`, `mam_journal`, `log_page_journal`, `st_stats_journal`,
`mtget_journal`, `phase_timings`, `restores` and `dar_create_reports` only grow. Nothing removes their rows by age or size, and nothing
that does is to be added (ADR-0012, 2026-10-07, item 28). They are evidence: a
journal with rows missing looks complete and is not, and the earliest reading of
a cartridge is the only baseline its later readings have. The session logs and
stage reports under the home are kept the same way.

Two commands do delete journal rows, and only rows the catalog can no longer
refer to. `snapshot delete`, which deletes only a snapshot never written to tape
(with `--force` once it is staged), takes that snapshot's own
`verification_results` rows with the slices they point at. `db fsck --repair`
deletes any row, in any table, whose foreign-key parent is gone.

The cost is small. The largest journal is `verification_results`, one row of
about 250 bytes per slice per full verify: a full LTO-6 at the default 1 GiB
slice is about 2,300 slices, so one full verify of it adds about 0.6 MB, and a
hundred cartridges verified four times a year add about 230 MB a year. A
contact's health and chip readings are a few kilobytes. SQLite handles a
catalog of some gigabytes without trouble; what grows with it is the time
`db backup` takes, because it copies the whole file. `db backup` prints the
size of the copy it wrote (`size_bytes` in `--json`), so a backup log shows the
catalog growing.

`db stats` shows what each table weighs, largest first, then how fast each
journal grew over the trailing 90 days, so the growth can be seen rather than
guessed. On a fresh home every table is a page or two of empty structure, and
the only journal rows are the few events `init` wrote (three here, from
`init --no-escrow`):

```text
$ tapectl db stats
database: 464.0 KiB, 42 tables, 116 pages
table sizes (measured by SQLite's dbstat: the pages each table and its indexes occupy):
  units                           0 rows     24.0 KiB
  cartridges                      0 rows     20.0 KiB
  events                          3 rows     20.0 KiB
  ...
journal growth, trailing 90 days (rows exact; bytes derived: the bytes in use on each table's and its indexes' pages, shared among its rows by stored length):
  events                          3 rows        493 B  ~  175.7 KiB/year
  cartridge_contacts              0 rows          0 B  ~        0 B/year
  ...
note: the journals go back only 1.0 days (counted as at least one), so each /year figure is that span's pace scaled to a year
```

The table sizes are measured when tapectl's SQLite has the `dbstat` table,
which the build tapectl ships with does. Otherwise the line says `ESTIMATED`
and the sizes are the stored values' lengths alone, without page or index
overhead, so they read low. `--json` carries the same list as `table_sizes`,
with `size_method` saying which (`dbstat` or `estimate`).

The growth lines list every journal, fastest first. The row count is exact:
the rows whose date (`logged_at`, `captured_at`, `verified_at` and so on, one
column per journal) falls in the last 90 days. No SQLite figure sizes a row, so
the bytes are derived: the bytes in use on the table's and its indexes' pages
(page size less free space, so a near-empty table's empty pages are not charged
to its few rows), shared among its rows by their stored length; without
`dbstat` they are the rows' stored lengths alone, and the line says
`ESTIMATED`. The `/year` figure is that pace over 365 days. A catalog younger
than 90 days is paced over its own age, from its oldest journal row and never
under a day, and the `note:` line says so; a few days' pace scaled to a year is
a rough figure, steadier as the catalog ages. A `verification_sessions` row for
a readback continued from an interrupted one is dated from its oldest
checkpoint, so it can fall before the window although it was written today.
`--json` carries it as `journal_growth`: `size_method`, `trailing_days` (90),
`window_days` (the days actually covered) and `journals`, each with `name`,
`date_column`, `rows_in_window`, `bytes_in_window` and `bytes_per_year`.

## Disaster Recovery

Every tape is self-describing, so the database being gone costs you convenience,
not data. There are two ways back, and which one you take depends on what you
hold — not on how bad the loss is. This section is the procedure;
[keys-and-recovery.md](keys-and-recovery.md) explains which key opens what,
and [install.md §8](install.md#8-getting-the-catalog-back) covers the cheapest
case, restoring a catalog from a `db backup` copy.

What a power cut costs at each point of a write, why a write host wants a UPS
and a catalog backup after every write session, and what the on-tape hashes
do and do not prove (they detect damage, not a deliberate rewrite: the seal is
not a signature) are in
[design/threat-model.md](design/threat-model.md).

> [!IMPORTANT]
> Every device below is written `$TAPE`. Find the drive by serial with
> `ls -l /dev/tape/by-id/` and set `TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst`
> first — `/dev/nstN` numbering is not stable across reboots on a host with
> more than one SCSI device, and a wrong device reads a different tape.

### If you hold a tenant key: restore directly, no catalog needed

This is the heir path. It needs `mt`, `dd`, `age`, `dar`, `sha256sum`, `head`,
`truncate`, `tar`, `mkfifo` and `tee` — the list the tape's own guide gives — plus
the ordinary shell tools any Linux system already has (`bash`, `awk`, `sed`, `tr`,
`cut`, `grep`, `wc`, `mktemp`), which RESTORE.sh uses without checking for them.
Notably, it does not need `tapectl`. The RESTORE.sh of a tape written by 1.2.0 or
later checks, when a mode starts and before it reads the tape, only the tools that
mode runs: `--info` and `--verify` need `mt`, `dd`, `sha256sum` and `head`;
`--find-envelope` adds `age`, `truncate` and `tar`; `--list` adds `dar`;
`--restore --path` adds `tee`; a whole-unit `--restore` needs `tee` and `mkfifo` too.
An earlier tape's script checks every tool in every mode.

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

   On tapes written by tapectl 1.1.0 and later a restore streams each slice
   from the tape through age into dar, so `/dest` needs about the unit's size
   free and must be empty (`--overwrite` finishes a restore that stopped part
   way). `--all` restores every unit; `--list` shows a unit's files and
   `--path` restores one, from the dar catalogue in the envelope. The script
   checks space before it reads any slice. A 1.0.x tape's own script needs
   about twice the unit's size, plus one slice. The decrypted envelopes go to a
   work directory under `$TMPDIR` (default `/tmp`) on a 1.2.0 tape's script, which
   warns when that looks too small; set `TMPDIR` to move it.

   On a 1.2.0 tape, a restore with no `--unit` or `--all` that finds one unit in
   the first envelope your key opens restores it, and then says how many other
   envelopes the key opened and how many other units they list. An operator or
   escrow key opens every tenant's envelope, so `--list`, then `--all` into a
   new directory, is how you take all of it.

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

Run these five steps in sequence on the rebuilt machine. (On a fresh
production host, `scripts/first-run.sh` step 7 asks for the original escrow
public key and does step 1 for you.)

**1. Register the original escrow identity.**

```bash
tapectl init --operator mike --escrow-public-key age1…   # the ORIGINAL, from the kit's cover sheet
```

`--operator` is required when you run this as the `tapectl` service user or as
root; the recorded session below gives the same name the original home used.

```text
$ tapectl --home ~/.tapectl-rebuilt init --operator mike --escrow-public-key age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7
tapectl initialized at ~/.tapectl-rebuilt
  operator: mike
  ...
  escrow:   adopted age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7 (imported — its secret lives on the heir kit, not here)
```

Every escrow check compares against the escrow recipient this catalog has
*registered*. A plain `init` registers a brand-new one, and every tape was
encrypted to the old one. So until the original is registered, every stage set
is an escrow gap: `catalog locate` shows `NO` in its Escrow column, `audit`'s
`escrow_coverage` check reports each one as
`encrypted without the current escrow recipient — the current escrow key cannot recover it`,
`volume write` refuses to re-copy, and nothing can be attested. `audit` also
explains the cause once (`escrow_identity_mismatch`, naming the key). The
equivalent two-step form still works, if you prefer it:

```bash
tapectl init --operator mike --no-escrow      # do NOT let init mint a new escrow identity
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

**4. Rebuild every tape sealed since the kit was made.** Those are the tapes
the imported catalog does not list. `tapectl volume list` shows every volume
the kit's catalog knows, so each cartridge on your shelf whose label is
missing from it needs a rebuild. `audit` cannot find them for you: the
imported catalog has never heard of those tapes. If you are unsure about a
tape, rebuild it anyway: a tape the catalog already knows adds nothing (what a
rebuild does to rows it finds is below the example).

```bash
tapectl catalog rebuild --from-volume \
    --device "$TAPE" \
    --key escrow.age.key \
    --label L8-0002            # optional wrong-tape guard
```

`--key` takes the escrow secret file from step 3, or an operator key file
from the old home's `keys/` directory. Operator key files are named after
the operator tenant, for example `~/.tapectl/keys/mike-primary.age.key`.
Rebuilding one tape into an empty catalog with the escrow key printed:

```text
rebuilt from volume "L8-0002" (uuid 6ff3d909-0486-4310-a31e-68a0fd0d1ec5), 3 envelope(s) opened
  inserted: 2 tenant(s), 4 unit(s), 4 snapshot(s), 4 stage set(s),
            4 slice(s), 4 write(s), 4 position(s), 11 file row(s), 1 volume
  cartridge "E01003L8_1775794348" registered from this tape's own identity and bound
  the slice hashes recorded are the tape's own claim; run `tapectl volume verify L8-0002` to check them
  escrow receipts: 4 stage set(s) carried theirs on the tape
```

Run it once per cartridge, in any order. Running it twice, or over a catalog
that is damaged rather than absent, is safe, because of what it does to rows
it finds:

- **It inserts what is missing.** A second run over the same cartridge inserts
  nothing; when it changes nothing at all, it prints
  `no changes — the catalog already knew this volume`.
- **It fills only what was empty.** Run with the registered escrow key, it
  records an escrow receipt on a rebuilt stage set that has none, once it has
  proved it (*Attest*, below). A cartridge row with no chip serial gets the one
  this drive read (`medium serial learned onto cartridge "…"`).
- **It reports a volume row it disagrees with, and leaves it alone.** If the
  catalog already has this volume as something other than `sealed`, or with
  an `observed_condition` other than `ok` (a quarantine a failed verify left),
  the report prints a `warning:` naming it. The tape's units are attached to
  that row, and its status and condition stay as they were.
- **It displaces the old volume on a reused cartridge.** When the chip serial
  proves this tape is a cartridge the catalog binds to a *different* volume,
  that volume's bytes are gone from the medium. The rebuild marks it `erased`,
  as `volume init` would, and prints each unit that loses a copy, flagging
  one left with `ZERO copies`. A barcode alone never displaces a live volume
  (see the cartridge identity paragraph below).

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
| tenant ownership, escrow receipts, the per-file index | the operator envelope's `catalog.db` (tapes written after 2026-09-11); older tapes give ownership from the tenant envelopes and no receipt. Tapes written by 1.1.0 and later also record each file's kind and a symlink's target; on an older tape every non-directory comes back as a regular file |
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

**5. Rotate before staging anything new.** Any tenant, the operator included,
with an active key whose `.age.key` is not in `keys/` must be rotated
(`tapectl key rotate --tenant <name>`) — and nothing warns you: `stage create`
would encrypt new data to keys whose private halves are gone. If you skipped
step 3 (no kit), rotate the operator even though its key files came back: `init`
gave it fresh rows, and step 2 put different private files under those names.
Rebuilt tenants have no key rows until you rotate or `key import` them. The full
check is [Keys and recovery](keys-and-recovery.md), runbook C, step 9.

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
| family | active |          | 2026-09-29 08:19:49 | family photos and letters |
+--------+--------+----------+---------------------+---------------------------+
| mike   | active | yes      | 2026-09-29 08:19:49 | System operator           |
+--------+--------+----------+---------------------+---------------------------+
| work   | active |          | 2026-09-29 08:19:50 | business records          |
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
