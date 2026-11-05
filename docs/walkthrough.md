# Walkthrough: your first archive, end to end

This page walks through one complete session — from an empty machine to two
sealed, verified tapes in two places, a restore, a Heir Kit and a disaster-recovery
rehearsal — with the **real output** of every command. It is the fastest way to see
how the pieces fit before you read the reference pages.

The session was captured on a virtual tape library ([mhvtl](operator-guide.md)), so
the drive is an LTO-8 emulation and the cartridges' serials look like
`E01001L8_1775794348`. The demo's drive was also declared with `--capacity-override 2G`,
a flag for virtual drives only, which the commands below leave out. That is why
`volume info` below reports a capacity of 2.0 GB where a real LTO-8 cartridge shows
`12.00 TB`. On a real drive the commands and the shape of the output are the same.

> [!NOTE]
> A production install made with [`scripts/first-run.sh`](install.md) runs tapectl as
> a dedicated `tapectl` service user, so there every command below is
> `tapectl-op …` (or `sudo -u tapectl -H tapectl …`). This walkthrough runs tapectl as
> yourself, with the default home `~/.tapectl`.

At any step, `tapectl status` shows where things stand: the home in use, the units
by state, and the next command to run. The session below looks at it three times.

**Contents**

1. [Initialise](#1-initialise)
2. [Staging and a drive](#2-staging-and-a-drive)
3. [Places for cartridges](#3-places-for-cartridges)
4. [Tenants](#4-tenants)
5. [Units](#5-units)
6. [Snapshot](#6-snapshot)
7. [Stage](#7-stage)
8. [Write tape 1: one command](#8-write-tape-1-one-command)
9. [Audit, and the second copy](#9-audit-and-the-second-copy)
10. [Find things](#10-find-things)
11. [Restore](#11-restore)
12. [Staging, after the second copy](#12-staging-after-the-second-copy)
13. [The Heir Kit and a catalog backup](#13-the-heir-kit-and-a-catalog-backup)
14. [Rehearse a disaster](#14-rehearse-a-disaster)
15. [Where next](#15-where-next)

The data being archived:

```text
/media/family/letters/1998-letter-to-mum.txt
/media/family/photos/2019-italy/IMG_001.jpg … IMG_003.jpg
/media/family/photos/2020-garden/IMG_101.jpg, IMG_102.jpg
/media/work/invoices-2024/2024-001.txt
```

`TAPE` is your drive's non-rewinding device **by serial** — never
`/dev/nst0`, whose number can change at every boot. It is used once, to register
the drive in step 2 (and again in step 14): with one drive registered, tapectl finds
it from then on and no command here takes `--device`. Find yours:

```bash
ls -l /dev/tape/by-id/                       # find yours; it ends in -nst
TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst
```

---

## 1. Initialise

`init` creates the home (`~/.tapectl`: database, config, keys), the **operator**
tenant, and the permanent **escrow identity**.

```bash
tapectl init --operator alice
```

```text
================================================================================
  ESCROW IDENTITY GENERATED -- THIS SECRET IS SHOWN EXACTLY ONCE, RIGHT NOW
================================================================================
  ...
  SECRET -- transcribe this line:

    AGE-SECRET-KEY-1…(redacted)

  Public key (already saved to disk and the database -- safe to keep there):

    age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7

================================================================================

tapectl initialized at ~/.tapectl
  operator: alice
  database: ~/.tapectl/tapectl.db
  config:   ~/.tapectl/config.toml
  escrow:   age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7
            (the SECRET was printed above — transcribe it now onto paper; ADR-0005)
  dar:      dar (found at /usr/bin/dar)

next steps:
  a tenant, whose data is archived    tapectl tenant add <TENANT>
  the tape drive                      tapectl drive add --name <NAME> --device-tape /dev/tape/by-id/<DRIVE>-nst
  what to archive, a folder of units  tapectl collection add <NAME> <ROOT> --tenant <TENANT>
  or one folder as a unit             tapectl unit init <PATH> --tenant <TENANT>
  where copies are kept               tapectl location add <PLACE>
  a copy, once units are staged       tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <PLACE>
`tapectl status` shows where things stand and the next command at every step.
```

> [!WARNING]
> **Write the secret down on paper before you do anything else.** It is not stored
> anywhere on the machine and is never shown again. It is the last-resort key to
> every tape you will ever write — see [Keys and recovery](keys-and-recovery.md).

The operator tenant (`alice` here) is created by `init`; don't `tenant add` it again.
The next steps `init` lists are the rest of this page, in about that order.

## 2. Staging and a drive

Staging is where encrypted slices wait before they go to tape; it needs room for
everything you stage before a write, which is a whole tape's worth if you fill one
in a session. `init` already created a staging directory at `~/.tapectl/staging`,
which is enough to follow along. To put it on a bigger disk, as this session did, create the
directory, make it yours, and change the `directory` line of the `[staging]` table
in `~/.tapectl/config.toml` ([reference](configuration.md)):

```bash
sudo install -d -o "$USER" -m 700 /srv/staging
```

```toml
[staging]
directory = "/srv/staging"
```

Then check the result:

```bash
tapectl config check
```

```text
config: valid
home: ~/.tapectl (this account's ~/.tapectl)
dar: 2.7.13 at '/usr/bin/dar' (meets minimum 2.6)
staging: '/srv/staging' exists and is writable
host check (defaults, no [host_check] table): units none; processes cargo, rustc, docker, Runner.Worker; max load 1.00/CPU; min available 2048 MiB; max memory pressure 10.00%; max I/O pressure 10.00% — `tapectl host check` runs it
note: no drive is configured, so every write refuses: add it with `tapectl drive add --name lto6 --device-tape /dev/tape/by-id/<drive>-nst --generation LTO-6`
defaults: verify_interval_days = 365 (`audit` warns when a unit has gone longer without a passing full `volume verify`; an archive set that names its own interval overrides this for its units)
```

The `home:` line names the home every command here uses and why (here, the account's
own `~/.tapectl`; `--home` or `TAPECTL_HOME` name another). A `note:` is not a
problem with the file, only something that will matter later, like the missing drive.

Next, tell tapectl about the drive. It needs only `TAPE`: the drive's generation is
read from its model. The drive also has an `sg`
node, its SCSI-generic twin, used for health pages and the cartridge's memory chip;
tapectl derives it from the tape node through sysfs at every use, so a renumbering
of `/dev/sgN` never matters (this session's was `sg5`). You declare only what the
**drive** is; each cartridge's own generation is read from the cartridge later.

```bash
tapectl drive add --name lto8 --device-tape "$TAPE"
```

```text
generation LTO-8, read from the drive attached at /dev/tape/by-id/scsi-XYZZY_A1-nst (model IBM ULT3580-TD8)
drive "lto8" added to ~/.tapectl/config.toml (LTO-8, tape=/dev/tape/by-id/scsi-XYZZY_A1-nst, sg=/dev/sg5 (derived from device_tape))
verify it with: tapectl config check
```

A model tapectl does not know is refused, naming `--generation LTO-N`, which then
declares it by hand. Run `tapectl config check` again as it says. With a drive declared it also reports
whether both device nodes are present and which `sg` node it derived, and warns if
staging has less free space than one full cartridge of that generation holds. If
you pass `--device-sg` to override the derivation, it warns when that node is not
the one the kernel pairs with the tape node, and `volume write` refuses the drive
until that is fixed. It also notes that the new drive has no `post_warning_bytes`,
the room past the drive's early warning that only a measurement gives
([LTO profiling](lto-profiling.md)); a write without it plans to the fill ceiling.

## 3. Places for cartridges

A **location** is somewhere a cartridge can physically be. Two copies in two places
is what protects you from a fire or a burglary.

```bash
tapectl location add home-rack -d "the shelf beside the drive"
tapectl location add offsite -d "a fire-safe box at a relative's house"
tapectl location list
```

```text
location "home-rack" added (id=1, kind=shelf)
location "offsite" added (id=2, kind=shelf)

Name        Kind    Cartridges   Volumes   Deposits   Description
home-rack   shelf   0            0         0          the shelf beside the drive
offsite     shelf   0            0         0          a fire-safe box at a relative's house
```

## 4. Tenants

A **tenant** is a key domain: everything a tenant owns is encrypted to that tenant's
keys (plus the operator's and escrow). Tenants cannot read each other's data, even on
the same tape.

```bash
tapectl tenant add family -d "family photos and letters"
tapectl tenant add work -d "business records"
tapectl key list --tenant family
```

```text
tenant "family" created (id=2) with primary and backup keys
tenant "work" created (id=3) with primary and backup keys

Alias            Type      Active   Escrow   Fingerprint                   Created
family-backup    backup    yes               age1nc8704hzj4ys4zdqw6tl...   2026-11-05 02:28 UTC
family-primary   primary   yes               age144hq9lhwyvg6w8huq8s9...   2026-11-05 02:28 UTC
```

Without `--tenant`, `key list` lists every tenant's keys, with a Tenant column.

## 5. Units

A **unit** is one directory archived as one thing — a photo trip, a year of invoices.
It gets a small `.tapectl-unit.toml` holding a uuid, so renaming or moving the
directory later doesn't lose it.

A tree that is already "one folder per thing", like the photos here, is a
**collection**: `collection add` names it once, and every folder under it is a unit,
now and as folders are added. The collection's name begins each unit's name. One
folder on its own is a unit by `unit init`, whose name comes from its path, with a
leading `/media` or `/mnt` dropped (or pass `--name`).

```bash
tapectl collection add photos /media/family/photos --tenant family
tapectl collection sync
tapectl unit init /media/family/letters --tenant family --tag letters
tapectl unit init /media/work/invoices-2024 --tenant work
tapectl unit list
```

```text
added collection "photos" (root /media/family/photos, tenant family, a unit per folder 1 level below the root) to ~/.tapectl/config.toml
its sync, as `tapectl collection sync` would run it now:
collection "photos" (dry run): would create 2; 0 refused · 0 moved, 0 reactivated, 0 missing · 2 pending (2 never archived)

collection "photos": 2 created; 0 refused · 0 moved, 0 reactivated, 0 missing · 2 pending (2 never archived)
unit "family/letters" initialized (id=3)
unit "work/invoices-2024" initialized (id=4)

Name                 Status   Tenant   Path                               Tags
family/letters       active   family   /media/family/letters              letters
photos/2019-italy    active   family   /media/family/photos/2019-italy
photos/2020-garden   active   family   /media/family/photos/2020-garden
work/invoices-2024   active   work     /media/work/invoices-2024
```

`collection add` first checks that tapectl can read the whole tree and write each
unit folder's `.tapectl-unit.toml`; when it cannot, it prints the `setfacl` lines
that would grant it and adds nothing. `collection sync` registers what `add` showed;
run it again whenever folders are added ([Configuration](configuration.md) has the
collection's other settings).

A unit that has never been archived, or has changed since its last version, is
**pending**. `status` shows it:

```bash
tapectl status
```

```text
home: ~/.tapectl (this account's ~/.tapectl)
session logs: ~/.tapectl/logs
running: nothing
units: 4 active
  State          Units
  pending        4 (4 never archived, 0 dirty)
  staged         0, awaiting a write
  under-copied   0, below their copy floor
volumes due a verify: none
next:
  tapectl collection run photos --label <NEW-LABEL> --init --full-confirm --move-to <a short location>
```

`collection run` would snapshot, stage and write the collection's pending units in
one command. This session takes the steps one at a time instead, to show each, and
for all four units at once.

## 6. Snapshot

A **snapshot** is a fast walk of the unit that records every file's path, size and
time. It is the cheap step; nothing is archived yet.

```bash
tapectl snapshot create family/letters photos/2019-italy photos/2020-garden work/invoices-2024
```

```text
snapshot created: family/letters v1 (2 files, 235 B)
snapshot created: photos/2019-italy v1 (4 files, 1.1 MiB)
snapshot created: photos/2020-garden v1 (3 files, 586.1 KiB)
snapshot created: work/invoices-2024 v1 (2 files, 226 B)
```

(Each count includes the unit's own `.tapectl-unit.toml`.)

## 7. Stage

`stage create` does the expensive work: it reads every file, checks that it still
matches the snapshot and records its sha256, runs `dar` over the unit, encrypts the
archive to the tenant's, the operator's and the escrow keys, and writes the encrypted
**slices** to staging. It also leaves a short **stage report** for each unit (its
slices' sizes and hashes) in `~/.tapectl/stage-reports/`. A unit whose version waits
in staging for a copy it is still owed is **staged**.

```bash
tapectl stage create family/letters photos/2019-italy photos/2020-garden work/invoices-2024
tapectl volume plan
tapectl stage list --owing
```

```text
staging family/letters (1 of 4)
staged family/letters (stage set 1)
staging photos/2019-italy (2 of 4)
staged photos/2019-italy (stage set 2)
staging photos/2020-garden (3 of 4)
staged photos/2020-garden (stage set 3)
staging work/invoices-2024 (4 of 4)
staged work/invoices-2024 (stage set 4)
staged: family/letters (1 slice, 1.1 KiB dar, 1.7 KiB encrypted)
staged: photos/2019-italy (1 slice, 1.1 MiB dar, 1.1 MiB encrypted)
staged: photos/2020-garden (1 slice, 587.3 KiB dar, 588.1 KiB encrypted)
staged: work/invoices-2024 (1 slice, 1.1 KiB dar, 1.7 KiB encrypted)

volume write plan (2 copies: the copy floor of photos/2019-italy, the largest among the staged units; --copies plans another number):
  photos/2019-italy v1: 1 slice, 1.1 MiB
  photos/2020-garden v1: 1 slice, 588.1 KiB
  family/letters v1: 1 slice, 1.7 KiB
  work/invoices-2024 v1: 1 slice, 1.7 KiB

total: 4 slices, 1.7 MiB x 2 = 3.3 MiB
estimated tapes: 1 (at the 97% fill ceiling)
cross-volume index: about 2.8 KiB per tape, 5.5 KiB on it with the envelope backups (an estimate: all of the above on one tape, as the catalog stands now)

Stage Set   Unit                 Version   Status   Slices   Size        Copies   Staged
1           family/letters       v1        staged   1        1.7 KiB     0 of 2   2026-11-05 02:28 UTC
2           photos/2019-italy    v1        staged   1        1.1 MiB     0 of 2   2026-11-05 02:28 UTC
3           photos/2020-garden   v1        staged   1        588.1 KiB   0 of 2   2026-11-05 02:28 UTC
4           work/invoices-2024   v1        staged   1        1.7 KiB     0 of 2   2026-11-05 02:28 UTC
```

`volume plan` is an estimate, and it needs no drive. It plans as many copies as the
units' copy floor asks for (two here, the default `min_copies`). The authoritative
capacity is read from the cartridge at `volume init`, and `volume write` refuses an
over-full plan before writing a byte. `stage list --owing` lists the stage sets whose
version still owes a copy, and how many it has of how many it needs.

## 8. Write tape 1: one command

Load a blank cartridge. One command makes a copy: it writes the label on the
cartridge, writes the staged units, reads the whole tape back, and puts the tape on
its shelf in the catalog.

```bash
tapectl volume write L8-0001 --init --full-confirm --move-to home-rack
```

```text
cartridge L8-0001 auto-registered from MAM (chip serial E01001L8_1775794348; barcode L8-0001, the volume's label)
volume "L8-0001" initialized (id=1); writing it now. If the write does not complete the volume stays initialized, and what to run next is said when it stops.
medium: LTO-8, 12.00 TB (the cartridge chip's medium density code 0x5e)
about to write to volume "L8-0001":
  family/letters v1: 1 slices, 1.7 KiB
  photos/2019-italy v1: 1 slices, 1.1 MiB
  photos/2020-garden v1: 1 slices, 588.1 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB
volume "L8-0001" write completed
sealed and confirmed: 4 units, 4 slices, 1.7 MiB; full readback passed
staging kept for 4 units still short of their policy; the next copy reuses their staged slices:
  unit family/letters has 1 copy, needs 2
  unit photos/2019-italy has 1 copy, needs 2
  unit photos/2020-garden has 1 copy, needs 2
  unit work/invoices-2024 has 1 copy, needs 2
volume "L8-0001" moved to "home-rack"
  cartridge "L8-0001" moved with it
next: tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <location>
```

`--init` is `volume init`: it reads the cartridge's chip for its generation (which
sets its capacity on a real drive, where no `--capacity-override` is declared) and
its serial, which is how tapectl knows the cartridge from now on. A cartridge new to
the catalog takes the volume's label as its barcode, so the sticker you put on it
reads `L8-0001`.

`volume write` then plans the whole tape, writes it in one session, and **confirms
it**: it reads back the front index and the seal marker, checks that the two ends of
the tape agree with each other and with the plan, and only then seals it. A sealed
volume is never appended to. With `--full-confirm` the confirm also reads every file
back and hashes it against the front index, so the write ends with the volume's
**first full read-back** done, while the staged slices are still on disk to rewrite a
bad copy from. Then each unit this copy brought to its copy floor gives up its
staging; none did here, since each has one copy of two. `--move-to` records the shelf,
and the last line is the next command.

> [!NOTE]
> Before a write, tapectl checks that the host is quiet enough to keep the drive
> streaming (a slow feed costs tape). If it is not, it asks — or, with no terminal,
> refuses unless you pass `--yes`. See
> [Troubleshooting](troubleshooting.md) and `tapectl host check`.

`volume info` shows everything the catalog now knows about the tape:

```bash
tapectl volume info L8-0001
```

```text
Volume: L8-0001
  Status:      sealed
  Condition:   ok
  Drive:       lto8 (lto)
  Media:       LTO-8
  Capacity:    3.5 MiB / 2.0 GB (0.2%)
  Cartridge:   L8-0001
               serial E01001L8_1775794348
  Location:    home-rack
  Created:     2026-11-05 02:30 UTC
  First write: 2026-11-05 02:30 UTC (started)
  Last write:  2026-11-05 02:30 UTC (completed)
  Sealed:      2026-11-05 02:30 UTC
  …

Units carried: 4 (1.7 MiB across 2 tenants, 2026-11-05 02:28 UTC)
    photos/2019-italy v1 (family) — 1.1 MiB
    photos/2020-garden v1 (family) — 588.1 KiB
    family/letters v1 (family) — 1.7 KiB
    work/invoices-2024 v1 (work) — 1.7 KiB

Writes:
    work/invoices-2024 v1: completed (2026-11-05 02:30 UTC)
    photos/2020-garden v1: completed (2026-11-05 02:30 UTC)
    photos/2019-italy v1: completed (2026-11-05 02:30 UTC)
    family/letters v1: completed (2026-11-05 02:30 UTC)

Verification history:
    [full] passed: started 2026-11-05 02:30 UTC, completed 2026-11-05 02:30 UTC (15/15 slices passed)

Warehouse deposits: none
```

(The lines cut at `…` are the write check and the drive's feed ratio; the phase
timings of the write follow the deposits.) The one verification is the write's own
full confirm; its 15 files are every file on the tape, the front index and the seal
marker among them. A volume written without `--full-confirm` gets a `quick` entry
instead, which reads none of the data, and until a full read-back passes `audit`
warns about it (`no_full_verify`) and `report verify-status` lists it as owed one:
`volume verify L8-0001` reads it back then.

## 9. Audit, and the second copy

`audit` compares the catalog against policy. It is advisory — it never blocks
anything — and its exit code says how bad things are: 0 clean, 1 warnings,
2 violations.

```bash
tapectl audit --action-plan
```

```text
VIOLATIONS (4):
  [copy_count] family/letters: has 1 copy, needs 2
  [copy_count] photos/2019-italy: has 1 copy, needs 2
  [copy_count] photos/2020-garden: has 1 copy, needs 2
  [copy_count] work/invoices-2024: has 1 copy, needs 2
WARNINGS (1):
  [escrow_kit_missing] archive: 1 sealed volume exists but no heir kit has ever been generated — nothing off-site can decrypt it
ACTION PLAN (2 fixes):
  fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL> — for 4 units: family/letters, photos/2019-italy, photos/2020-garden, work/invoices-2024
  fix: tapectl key escrow-kit <DIR> — for archive
audit: 4 violations, 1 warning (exit 2)
```

Without `--action-plan`, `audit` prints the findings and points at it. One copy is
not enough. The staged slices are still on disk (each unit keeps its staging until a
write brings it to its copy floor, and this is the first of two copies), so the second
copy is just another cartridge, and the same one command as the first: the fix's
`volume init` and `volume write` are what `volume write --init` runs. With L8-0001 out
of the drive and on its shelf, load a second **blank** one: `--init` refuses any tape
that already carries a volume label, and on a sealed tape `--force` does not change
that.

```bash
tapectl volume write L8-0002 --init --full-confirm --move-to offsite
tapectl audit
tapectl report copies
tapectl status
```

```text
cartridge L8-0002 auto-registered from MAM (chip serial E01003L8_1775794348; barcode L8-0002, the volume's label)
volume "L8-0002" initialized (id=2); writing it now. If the write does not complete the volume stays initialized, and what to run next is said when it stops.
medium: LTO-8, 12.00 TB (the cartridge chip's medium density code 0x5e)
...
volume "L8-0002" write completed
sealed and confirmed: 4 units, 4 slices, 1.7 MiB; full readback passed
staging released: 4 stage sets, 1.7 MiB freed (every unit written has its policy's copies)
volume "L8-0002" moved to "offsite"
  cartridge "L8-0002" moved with it
next: nothing to do; every unit written has its policy's copies

WARNINGS (1):
  [escrow_kit_missing] archive: 2 sealed volumes exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 0 violations, 1 warning (exit 1)
for the fixes: tapectl audit --action-plan

  family/letters: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  photos/2019-italy: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  photos/2020-garden: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  work/invoices-2024: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]

home: ~/.tapectl (this account's ~/.tapectl)
session logs: ~/.tapectl/logs
running: nothing
recent:
  …
units: 4 active
  State          Units
  pending        0
  staged         0, awaiting a write
  under-copied   0, below their copy floor
volumes due a verify: none
next: nothing to do
```

The violations are gone, and so is the staging: this copy brought every unit to its
copy floor, so each released its staged slices. The one warning left, the missing
Heir Kit, is dealt with in step 13. No volume is due a verify: both were read back in
full as they were written. A year on (`verify_interval_days`, 365 by default), `status`
names each one again, and `tapectl volume verify <LABEL>` reads it back in full.

## 10. Find things

```bash
tapectl catalog list family/letters
tapectl catalog search letter
tapectl catalog search "IMG 101"
tapectl catalog locate family/letters
```

```text
Path                       Size    Modified               Sha256
  .tapectl-unit.toml       188 B   2026-11-05 02:28 UTC   cad4f8ef479e...
  1998-letter-to-mum.txt   47 B    2026-11-05 02:26 UTC   8e91287dfeae...

  family/letters v1: 1998-letter-to-mum.txt (47 B)
1 result

  photos/2020-garden v1: IMG_101.jpg (293.0 KiB)
1 result

Volume    Status   Condition   Location    Snapshot   Slices   Holds   Written                Serviceable   Warehouse   Escrow   Verified
L8-0001   sealed   ok          home-rack   1          1        whole   2026-11-05 02:30 UTC   yes           -           yes      0d ago
L8-0002   sealed   ok          offsite     1          1        whole   2026-11-05 02:33 UTC   yes           -           yes      0d ago

note: "Verified" is this catalog's last-known record of each copy's most recent PASSED full readback (a full `volume verify`, or a write's full confirm; a quick one reads none of the copy's bytes and does not count) — not a check of the tape performed just now. "never" means no passed full readback is on record, not that the copy is bad; an aged value does not mean the tape has since failed. Re-run `tapectl volume verify <label>` to refresh it.
```

`catalog search` matches **file names inside units**, word by word, each word as a
prefix: `"IMG 101"` finds `IMG_101.jpg`, but `garden` finds nothing
(`no files matching "garden"`), because no word in any file's path starts with it;
only the unit is called `2020-garden`. Quote a pattern of several words, since it is
one argument. It searches each unit's newest version, the one `catalog list` lists, so a
file kept through many versions is one line; `--all-versions` lists it once per version
that holds it. `catalog locate` answers "which tape, and where is it?".

## 11. Restore

Load either copy and name that one with `--from`: restore checks the loaded tape
against it before reading anything, and refuses a different one. Here that is
L8-0002. One file, named by its unit and its path in the unit:

```bash
tapectl restore file family/letters 1998-letter-to-mum.txt --from L8-0002 --to /tmp/restore
```

```text
note: restoring as a non-root user, so the restored files are owned by you, not by the owners the archive recorded
restored "1998-letter-to-mum.txt" from "family/letters" on L8-0002 to /tmp/restore
```

The `note:` goes to stderr: only root can give restored files back their archived
owners, so as yourself they come back owned by you.

A whole unit (`--dry-run` first shows what would happen):

```bash
tapectl restore unit family/letters --from L8-0002 --to /tmp/restore/unit --dry-run
tapectl restore unit family/letters --from L8-0002 --to /tmp/restore/unit
diff -r /media/family/letters /tmp/restore/unit && echo identical
```

```text
would restore "family/letters" v1 from L8-0002 (1 slice) to /tmp/restore/unit
note: restoring as a non-root user, so the restored files are owned by you, not by the owners the archive recorded
restored "family/letters" v1 from L8-0002 (1 slice) to /tmp/restore/unit
identical
```

Restoring does not need tapectl at all: every tape carries `RESTORE.sh` and a guide.
See [Keys and recovery](keys-and-recovery.md).

## 12. Staging, after the second copy

Staging is already empty:

```bash
tapectl stage list
```

```text
nothing is held in staging
```

A write releases staging unit by unit: each unit the copy brings to its copy floor
gives up its staged slices, and a unit still short keeps them for the next copy. So
nothing needs cleaning between or after copies. `stage list --all` adds the released
stage sets as history. `tapectl stage clean` remains for what a write leaves behind
by itself: it sweeps every stage set whose staging failed, since those hold nothing a
copy needs, and the leftovers of killed runs; it too keeps any unit still short of its
copies.

## 13. The Heir Kit and a catalog backup

The **Heir Kit** is the printed envelope that lets you — or someone who inherits the
tapes — get everything back without this machine.

```bash
tapectl key escrow-kit ~/heir-kit
```

```text
heir kit written to ~/heir-kit
  COVER.txt        the printable cover sheet (print this)
  escrow-kit.html  same content with a QR, for a browser's print dialog
  catalog.db.age   encrypted catalog, 656.4 KiB, covering 2 sealed volumes
  tapectl-1.9.0    the program that made this kit, … bytes, sha256 … (named on the sheet)

still to do, and only you can do it:
  1. print COVER.txt (or the HTML page)
  2. copy the escrow SECRET (AGE-SECRET-KEY-1..., shown once by `tapectl init`)
     by hand into the box marked WRITE IT HERE -- the kit prints only the
     public half, and without the secret the sheet opens nothing
  3. seal it in a tamper-evident envelope
  4. store copies in at least TWO independent failure domains
```

The kit also carries a copy of the tapectl program that made it (the fourth file, with
its size and sha256 on the cover sheet), so the heir's one tool for reading around a
damaged slice does not depend on a repository still existing.

Regenerate it after every write session. The escrow secret opens every tape ever
written, old and new; what goes stale is the kit's encrypted catalog, which lists only
the volumes that existed when the kit was made (`audit` warns when it has fallen
behind).

Back up the catalog at the end of every write session too. Name a directory, and each
backup gets its own dated file there. The backup holds no private keys unless you ask:

```bash
tapectl db backup /mnt/backup
```

```text
database backed up to /mnt/backup/tapectl-20261105T023512Z.db (520.0 KiB); private keys not included — pass --include-keys to copy them
```

The directory (`/mnt/backup` here) must already exist; `db backup` creates none. A
file name works too, and a file that already exists is refused, never overwritten.
With `--include-keys` the private keys are copied beside the file, into
`/mnt/backup/tapectl-20261105T023512Z.keys/`, and that directory must be kept as
secret as the keys themselves.

## 14. Rehearse a disaster

Suppose the machine is gone: no database, no keys — only the tapes and the paper.
On a fresh machine, start a new home that **adopts** the original escrow identity
(its public key is on the Heir Kit), point it at the drive, and rebuild the catalog
from a tape with the escrow secret you wrote down:

```bash
tapectl init --operator alice --escrow-public-key age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7
tapectl drive add --name lto8 --device-tape "$TAPE"
tapectl catalog rebuild --from-volume --key escrow.key
tapectl unit list
```

(Find `TAPE` again on the new machine, as at the top of this page. `escrow.key` is a
file containing the one `AGE-SECRET-KEY-1…` line from the paper; delete it
afterwards.)

```text
tapectl initialized at ~/.tapectl
  operator: alice
  database: ~/.tapectl/tapectl.db
  config:   ~/.tapectl/config.toml
  escrow:   adopted age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7 (imported — its secret lives on the heir kit, not here)
  dar:      dar (found at /usr/bin/dar)

next steps:
  …

generation LTO-8, read from the drive attached at /dev/tape/by-id/scsi-XYZZY_A1-nst (model IBM ULT3580-TD8)
drive "lto8" added to ~/.tapectl/config.toml (LTO-8, tape=/dev/tape/by-id/scsi-XYZZY_A1-nst, sg=/dev/sg5 (derived from device_tape))
verify it with: tapectl config check

rebuilt from volume "L8-0002" (uuid 6ff3d909-0486-4310-a31e-68a0fd0d1ec5), 3 envelopes opened
  inserted: 2 tenants, 4 units, 4 snapshots, 4 stage sets,
            4 slices, 4 writes, 4 positions, 11 file rows, 1 volume
  cartridge "L8-0002" registered from this tape's own identity and bound
  the slice hashes recorded are the tape's own claim; run `tapectl volume verify L8-0002` to check them
  escrow receipts: 4 stage sets carried theirs on the tape

Name                 Status   Tenant   Path   Tags
family/letters       active   family
photos/2019-italy    active   family
photos/2020-garden   active   family
work/invoices-2024   active   work
```

Every unit, tenant and file row is back, read from the tape itself. Repeat
`catalog rebuild` for each tape, then `volume verify` them. The full procedure, and
what a rebuilt catalog does not know, is in [Keys and recovery](keys-and-recovery.md).

## 15. Where next

- [Concepts](concepts.md) — the model behind all of this.
- [Operator guide](operator-guide.md) — day-to-day operation, copies, compaction,
  cadence, warehouse copies.
- [Configuration](configuration.md) — every `config.toml` key, collections, policy.
- [Keys and recovery](keys-and-recovery.md) — keys, the Heir Kit, disaster recovery.
- [Troubleshooting](troubleshooting.md) — what each refusal means and what to do.
- [Command reference](cli/README.md) — every command and flag.
