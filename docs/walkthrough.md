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

**Contents**

1. [Initialise](#1-initialise)
2. [Staging and a drive](#2-staging-and-a-drive)
3. [Places for cartridges](#3-places-for-cartridges)
4. [Tenants](#4-tenants)
5. [Units](#5-units)
6. [Snapshot](#6-snapshot)
7. [Stage](#7-stage)
8. [Write, verify and shelve tape 1](#8-write-verify-and-shelve-tape-1)
9. [Audit, and the second copy](#9-audit-and-the-second-copy)
10. [Find things](#10-find-things)
11. [Restore](#11-restore)
12. [Clean up staging](#12-clean-up-staging)
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

Throughout, `TAPE` is your drive's non-rewinding device **by serial** — never
`/dev/nst0`, whose number can change at every boot:

```bash
ls -l /dev/tape/by-id/                       # find yours; it ends in -nst
TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst
```

---

## 1. Initialise

`init` creates the home (`~/.tapectl`: database, config, keys), the **operator**
tenant, and the permanent **escrow identity**.

```bash
tapectl init --operator mike
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
  operator: mike
  database: ~/.tapectl/tapectl.db
  config:   ~/.tapectl/config.toml
  escrow:   age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7
            (the SECRET was printed above — transcribe it now onto paper; ADR-0005)
  dar:      dar (found at /usr/bin/dar)
```

> [!WARNING]
> **Write the secret down on paper before you do anything else.** It is not stored
> anywhere on the machine and is never shown again. It is the last-resort key to
> every tape you will ever write — see [Keys and recovery](keys-and-recovery.md).

The operator tenant (`mike` here) is created by `init`; don't `tenant add` it again.

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
dar: 2.7.13 at '/usr/bin/dar' (meets minimum 2.6)
staging: '/srv/staging' exists and is writable
host check (defaults, no [host_check] table): units none; processes cargo, rustc, docker, Runner.Worker; max load 1.00/CPU; min available 2048 MiB; max memory pressure 10.00%; max I/O pressure 10.00% — `tapectl host check` runs it
```

Next, tell tapectl about the drive. It needs two device nodes: `TAPE`, and the
drive's `sg` node, its SCSI-generic twin, used for health pages and the cartridge's
memory chip. Find the `sg` node that belongs to your drive through sysfs
(`lsscsi -g` shows the pair too); this session's was `sg5`:

```bash
SG=/dev/$(ls "/sys/class/scsi_tape/$(basename "$(readlink -f "$TAPE")")/device/scsi_generic/")
echo "$SG"
```

You declare only what the **drive** is; each cartridge's own generation is read from
the cartridge later.

```bash
tapectl backend add --name lto8 --device-tape "$TAPE" --device-sg "$SG" --generation LTO-8
```

```text
backend "lto8" added to ~/.tapectl/config.toml (LTO-8, tape=/dev/tape/by-id/scsi-XYZZY_A1-nst, sg=/dev/sg5)
verify it with: tapectl config check
```

Run `tapectl config check` again as it says. With a drive declared it also reports
whether both device nodes are present, warns if the `sg` node is not the one the
kernel pairs with that tape node (`volume write` refuses the drive until that is
fixed), and warns if staging has less free space than one full cartridge of that
generation holds.

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

+-----------+-------+------------+---------+----------+---------------------------------------+
| Name      | Kind  | Cartridges | Volumes | Deposits | Description                           |
+-----------+-------+------------+---------+----------+---------------------------------------+
| home-rack | shelf | 0          | 0       | 0        | the shelf beside the drive            |
+-----------+-------+------------+---------+----------+---------------------------------------+
| offsite   | shelf | 0          | 0       | 0        | a fire-safe box at a relative's house |
+-----------+-------+------------+---------+----------+---------------------------------------+
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

+----------------+---------+--------+--------+-----------------------------+---------------------+
| Alias          | Type    | Active | Escrow | Fingerprint                 | Created             |
+----------------+---------+--------+--------+-----------------------------+---------------------+
| family-backup  | backup  | yes    |        | age1k83yzew0lqgc2ghf7ktc... | 2026-09-29 08:19:49 |
+----------------+---------+--------+--------+-----------------------------+---------------------+
| family-primary | primary | yes    |        | age194da4h246gzse65r0a3k... | 2026-09-29 08:19:49 |
+----------------+---------+--------+--------+-----------------------------+---------------------+
```

## 5. Units

A **unit** is one directory archived as one thing — a photo trip, a year of invoices.
It gets a small `.tapectl-unit.toml` holding a uuid, so renaming or moving the
directory later doesn't lose it. The unit's name comes from its path, with a leading
`/media` or `/mnt` dropped (or pass `--name`).

```bash
tapectl unit init /media/family/letters --tenant family --tag letters
tapectl unit init-bulk /media/family/photos --tenant family --tag photos   # one unit per subdirectory
tapectl unit init /media/work/invoices-2024 --tenant work
tapectl unit list
```

```text
unit "family/letters" initialized (id=1)
  ok: /media/family/photos/2020-garden (id=2)
  ok: /media/family/photos/2019-italy (id=3)
2 units created, 0 skipped
unit "work/invoices-2024" initialized (id=4)

+---------------------------+--------+--------+----------------------------------+---------+
| Name                      | Status | Tenant | Path                             | Tags    |
+---------------------------+--------+--------+----------------------------------+---------+
| family/letters            | active | family | /media/family/letters            | letters |
+---------------------------+--------+--------+----------------------------------+---------+
| family/photos/2019-italy  | active | family | /media/family/photos/2019-italy  | photos  |
+---------------------------+--------+--------+----------------------------------+---------+
| family/photos/2020-garden | active | family | /media/family/photos/2020-garden | photos  |
+---------------------------+--------+--------+----------------------------------+---------+
| work/invoices-2024        | active | work   | /media/work/invoices-2024        |         |
+---------------------------+--------+--------+----------------------------------+---------+
```

For a tree that is already "one folder per thing", a [collection](configuration.md)
registers every folder at once and keeps doing so as folders are added.

## 6. Snapshot

A **snapshot** is a fast walk of the unit that records every file's path, size and
time. It is the cheap step; nothing is archived yet.

```bash
tapectl snapshot create family/letters
tapectl snapshot create family/photos/2019-italy
tapectl snapshot create family/photos/2020-garden
tapectl snapshot create work/invoices-2024
```

```text
snapshot created: family/letters v1 (2 files, 224 B)
snapshot created: family/photos/2019-italy v1 (4 files, 1.1 MiB)
snapshot created: family/photos/2020-garden v1 (3 files, 586.1 KiB)
snapshot created: work/invoices-2024 v1 (2 files, 212 B)
```

(Each count includes the unit's own `.tapectl-unit.toml`.)

## 7. Stage

`stage create` does the expensive work: it reads every file, checks that it still
matches the snapshot and records its sha256, runs `dar` over the unit, encrypts the
archive to the tenant's, the operator's and the escrow keys, and writes the encrypted
**slices** to staging. It also leaves a short **stage report** for each unit (its
slices' sizes and hashes) in `~/.tapectl/stage-reports/`.

```bash
tapectl stage create family/letters
tapectl stage create family/photos/2019-italy
tapectl stage create family/photos/2020-garden
tapectl stage create work/invoices-2024
tapectl volume plan --copies 2
```

```text
staged: family/letters (1 slices, 1.1 KiB dar, 1.8 KiB encrypted)
staged: family/photos/2019-italy (1 slices, 1.1 MiB dar, 1.1 MiB encrypted)
staged: family/photos/2020-garden (1 slices, 587.3 KiB dar, 588.1 KiB encrypted)
staged: work/invoices-2024 (1 slices, 1.1 KiB dar, 1.7 KiB encrypted)

volume write plan (2 copy/copies):
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  family/letters v1: 1 slices, 1.8 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB x 2 = 3.4 MiB
estimated tapes: 1 (at the 97% fill ceiling)
```

`volume plan` is an estimate. The authoritative capacity is read from the cartridge
at `volume init`, and `volume write` refuses an over-full plan before writing a byte.

## 8. Write, verify and shelve tape 1

Load a blank cartridge and write the label on it. `volume init` reads the cartridge's
chip: its generation (which sets its capacity on a real drive, where no
`--capacity-override` is declared) and its serial (which is how tapectl
knows the cartridge from now on — a barcode sticker is optional and can be added
later with [`cartridge relabel`](cli/cartridge.md#tapectl-cartridge-relabel)).

```bash
tapectl volume init L8-0001 --device "$TAPE"
tapectl volume write L8-0001 --device "$TAPE"
tapectl volume verify L8-0001 --device "$TAPE" --full
```

```text
cartridge E01001L8_1775794348 auto-registered from MAM (barcode = medium serial)
volume "L8-0001" initialized (id=1)

about to write to volume "L8-0001":
  family/letters v1: 1 slices, 1.8 KiB
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB
volume "L8-0001" write completed

verify L8-0001 (full tier): 13 checked, 13 passed, 0 failed
```

`volume write` plans the whole tape, writes it in one session, then **confirms
it**: it reads back the front index and the seal marker, checks that the two ends of
the tape agree with each other and with the plan, and only then seals it. A sealed
volume is never appended to. That confirm reads none of the data back (`volume write
--full-confirm` would, at the cost of a second pass over the whole tape), so
`verify --full` is the volume's **first full read-back**: it hashes every file on the
tape against the front index. Run it soon after the write, while the staged slices
are still on disk to rewrite a bad copy from.

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
  Backend:     lto (lto8)
  Media:       LTO-8
  Capacity:    3.5 MiB / 2.0 GB (0.2%)
  Cartridge:   E01001L8_1775794348
               serial E01001L8_1775794348
  Location:    (not placed)
  Created:     2026-09-29 08:19:59
  First write: 2026-09-29 08:20:01 (started)
  Last write:  2026-09-29 08:20:01 (completed)

Units carried: 4 (1.7 MiB across 2 tenant(s), 2026-09-29 08:19:50)
    family/photos/2019-italy v1 (family) — 1.1 MiB
    family/photos/2020-garden v1 (family) — 588.1 KiB
    family/letters v1 (family) — 1.8 KiB
    work/invoices-2024 v1 (work) — 1.7 KiB

Writes:
    work/invoices-2024 v1: completed (2026-09-29 08:20:01)
    family/photos/2020-garden v1: completed (2026-09-29 08:20:01)
    family/photos/2019-italy v1: completed (2026-09-29 08:20:01)
    family/letters v1: completed (2026-09-29 08:20:01)

Verification history:
    [full] passed: started 2026-09-29 08:20:02, completed 2026-09-29 08:20:02 (13/13 slices passed)
    [quick] passed: started 2026-09-29 08:20:01, completed 2026-09-29 08:20:01 (2/2 slices passed)

Warehouse deposits: none
```

The older of the two verifications is `volume write`'s own confirm — `quick`, and its
2 files are the front index and the seal marker; the newer one is `verify --full`,
the first full read-back. Until a full one passes, `audit` warns about the volume
(`no_full_verify`) and `report verify-status` lists it as owed one.

The tape is not placed anywhere yet. Put the cartridge on its shelf and tell the
catalog:

```bash
tapectl volume move L8-0001 --to home-rack
```

```text
volume "L8-0001" moved to "home-rack"
  cartridge "E01001L8_1775794348" moved with it
```

## 9. Audit, and the second copy

`audit` compares the catalog against policy. It is advisory — it never blocks
anything — and its exit code says how bad things are: 0 clean, 1 warnings,
2 violations.

```bash
tapectl audit --action-plan
```

```text
VIOLATIONS (4):
  [copy_count] family/letters: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
  [copy_count] family/photos/2019-italy: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
  [copy_count] family/photos/2020-garden: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
  [copy_count] work/invoices-2024: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
WARNINGS (1):
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
    fix: tapectl key escrow-kit --out <dir>
audit: 4 violations, 1 warnings (exit 2)
```

One copy is not enough. The staged slices are still on disk (a write does not remove
them), so the second copy is just another cartridge. With L8-0001 out of the drive
and on its shelf, load a second **blank** one first: `volume init` refuses any tape
that already carries a volume label, and on a sealed tape `--force` does not change
that.

```bash
tapectl volume init L8-0002 --device "$TAPE"
tapectl volume write L8-0002 --device "$TAPE"
tapectl volume verify L8-0002 --device "$TAPE" --full
tapectl volume move L8-0002 --to offsite
tapectl audit
tapectl report copies
```

```text
cartridge E01003L8_1775794348 auto-registered from MAM (barcode = medium serial)
volume "L8-0002" initialized (id=2)
...
volume "L8-0002" write completed
verify L8-0002 (full tier): 13 checked, 13 passed, 0 failed
volume "L8-0002" moved to "offsite"
  cartridge "E01003L8_1775794348" moved with it

WARNINGS (1):
  [escrow_kit_missing] archive: 2 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 0 violations, 1 warnings (exit 1)

  family/letters: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  family/photos/2019-italy: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  family/photos/2020-garden: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  work/invoices-2024: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
```

The violations are gone. The one warning left, the missing Heir Kit, is dealt with
in step 13.

## 10. Find things

```bash
tapectl catalog ls family/letters
tapectl catalog search letter
tapectl catalog search "IMG 101"
tapectl catalog locate family/letters
```

```text
+--------------------------+-------+---------------------------+-----------------+
| Path                     | Size  | Modified                  | SHA256          |
+--------------------------+-------+---------------------------+-----------------+
|   .tapectl-unit.toml     | 188 B | 2026-09-29T08:19:50+00:00 | f7aae6855718... |
+--------------------------+-------+---------------------------+-----------------+
|   1998-letter-to-mum.txt | 36 B  | 2026-09-29T08:19:49+00:00 | 770ef76f0932... |
+--------------------------+-------+---------------------------+-----------------+

  family/letters v1: 1998-letter-to-mum.txt (36 B)
1 result(s)

  family/photos/2020-garden v1: IMG_101.jpg (293.0 KiB)
1 result(s)

+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| Volume  | Status | Condition | Location  | Snapshot | Slices | Written             | Serviceable | Warehouse | Escrow | Verified |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0001 | sealed | ok        | home-rack | 1        | 1      | 2026-09-29 08:20:01 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0002 | sealed | ok        | offsite   | 1        | 1      | 2026-09-29 08:20:05 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+

note: "Verified" is this catalog's last-known record of each copy's most recent PASSED `volume verify` — not a check of the tape performed just now. "never" means no passed verification is on record, not that the copy is bad; an aged value does not mean the tape has since failed. Re-run `tapectl volume verify <label>` to refresh it.
```

`catalog search` matches **file names inside units**, word by word, each word as a
prefix: `"IMG 101"` finds `IMG_101.jpg`, but `garden` finds nothing
(`no files matching "garden"`), because no word in any file's path starts with it;
only the unit is called `2020-garden`. Quote a pattern of several words, since it is
one argument. It searches each unit's newest version, the one `catalog ls` lists, so a
file kept through many versions is one line; `--all-versions` lists it once per version
that holds it. `catalog locate` answers "which tape, and where is it?".

## 11. Restore

Load either copy and name that one with `--from`: restore checks the loaded tape
against it before reading anything, and refuses a different one. Here that is
L8-0002. One file:

```bash
tapectl restore file --file 1998-letter-to-mum.txt --unit family/letters --from L8-0002 --to /tmp/restore --device "$TAPE"
```

```text
2026-09-29T08:20:05.900411Z  WARN tapectl::dar::restore: restoring as a non-root user: restored files will be owned by the invoking user, not their archived owners
restored "1998-letter-to-mum.txt" from "family/letters" on L8-0002 to /tmp/restore
```

The `WARN` line is a log line on stderr: only root can give restored files back their
archived owners, so as yourself they come back owned by you.

A whole unit (`--dry-run` first shows what would happen):

```bash
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE" --dry-run
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE"
diff -r /media/family/letters /tmp/restore/unit && echo identical
```

```text
would restore "family/letters" v1 from L8-0002 (1 slices) to /tmp/restore/unit
2026-09-29T08:20:06.455931Z  WARN tapectl::dar::restore: restoring as a non-root user: restored files will be owned by the invoking user, not their archived owners
restored "family/letters" v1 from L8-0002 (1 slices) to /tmp/restore/unit
identical
```

Restoring does not need tapectl at all: every tape carries `RESTORE.sh` and a guide.
See [Keys and recovery](keys-and-recovery.md).

## 12. Clean up staging

With both copies written, the staged slices can go:

```bash
tapectl staging clean
```

```text
cleaned 4 stage set(s), 4 files removed, 2.1 MiB freed
  sessions: 2 reclaimed, 0 retained, 0 orphaned; 4 lockfiles reclaimed
```

`staging clean` releases the stage sets that have been written, and even then keeps
any unit that still has fewer copies than its policy asks for, so it is safe to run
between copies. It also sweeps every stage set whose staging failed, since those hold
nothing a copy needs.

## 13. The Heir Kit and a catalog backup

The **Heir Kit** is the printed envelope that lets you — or someone who inherits the
tapes — get everything back without this machine.

```bash
tapectl key escrow-kit --out ~/heir-kit
```

```text
heir kit written to ~/heir-kit
  COVER.txt        the printable cover sheet (print this)
  escrow-kit.html  same content with a QR, for a browser's print dialog
  catalog.db.age   encrypted catalog, 672130 bytes, covering 2 sealed volume(s)

still to do, and only you can do it:
  1. print COVER.txt (or the HTML page)
  2. copy the escrow SECRET (AGE-SECRET-KEY-1..., shown once by `tapectl init`)
     by hand into the box marked WRITE IT HERE -- the kit prints only the
     public half, and without the secret the sheet opens nothing
  3. seal it in a tamper-evident envelope
  4. store copies in at least TWO independent failure domains
```

Regenerate it after every write session. The escrow secret opens every tape ever
written, old and new; what goes stale is the kit's encrypted catalog, which lists only
the volumes that existed when the kit was made (`audit` warns when it has fallen
behind).

Back up the catalog at the end of every write session too. The backup holds no
private keys unless you ask:

```bash
tapectl db backup --to /mnt/backup/tapectl.db
```

```text
database backed up to /mnt/backup/tapectl.db (private keys not included — pass --include-keys to copy them)
```

The directory (`/mnt/backup` here) must already exist; `db backup` creates none. With
`--include-keys` the private keys are copied beside the file, into
`/mnt/backup/tapectl.keys/`, and that directory must be kept as secret as the keys
themselves.

## 14. Rehearse a disaster

Suppose the machine is gone: no database, no keys — only the tapes and the paper.
On a fresh machine, start a new home that **adopts** the original escrow identity
(its public key is on the Heir Kit), point it at the drive, and rebuild the catalog
from a tape with the escrow secret you wrote down:

```bash
tapectl init --operator mike --escrow-public-key age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7
tapectl backend add --name lto8 --device-tape "$TAPE" --device-sg "$SG" --generation LTO-8
tapectl catalog rebuild --from-volume --key escrow.key --device "$TAPE"
tapectl unit list
```

(Find `TAPE` and `SG` again on the new machine, as at the top of this page and in
step 2. `escrow.key` is a file containing the one `AGE-SECRET-KEY-1…` line from the
paper; delete it afterwards.)

```text
tapectl initialized at ~/.tapectl
  operator: mike
  database: ~/.tapectl/tapectl.db
  config:   ~/.tapectl/config.toml
  escrow:   adopted age130teljw9ws8rpmlf7w66penltdv4q59yf4xaqq9xjv45t8qmaqfqqrnmm7 (imported — its secret lives on the heir kit, not here)
  dar:      dar (found at /usr/bin/dar)

backend "lto8" added to ~/.tapectl/config.toml (LTO-8, tape=/dev/tape/by-id/scsi-XYZZY_A1-nst, sg=/dev/sg5)
verify it with: tapectl config check

rebuilt from volume "L8-0002" (uuid 6ff3d909-0486-4310-a31e-68a0fd0d1ec5), 3 envelope(s) opened
  inserted: 2 tenant(s), 4 unit(s), 4 snapshot(s), 4 stage set(s),
            4 slice(s), 4 write(s), 4 position(s), 11 file row(s), 1 volume
  cartridge "E01003L8_1775794348" registered from this tape's own identity and bound
  the slice hashes recorded are the tape's own claim; run `tapectl volume verify L8-0002` to check them
  escrow receipts: 4 stage set(s) carried theirs on the tape

+---------------------------+--------+--------+------+------+
| Name                      | Status | Tenant | Path | Tags |
+---------------------------+--------+--------+------+------+
| family/letters            | active | family |      |      |
+---------------------------+--------+--------+------+------+
| family/photos/2019-italy  | active | family |      |      |
+---------------------------+--------+--------+------+------+
| family/photos/2020-garden | active | family |      |      |
+---------------------------+--------+--------+------+------+
| work/invoices-2024        | active | work   |      |      |
+---------------------------+--------+--------+------+------+
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
