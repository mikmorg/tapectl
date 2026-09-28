# Walkthrough: your first archive, end to end

This page walks through one complete session — from an empty machine to two
sealed, verified tapes in two places, a restore, a Heir Kit and a disaster-recovery
rehearsal — with the **real output** of every command. It is the fastest way to see
how the pieces fit before you read the reference pages.

The session was captured on a virtual tape library ([mhvtl](operator-guide.md)), so
the drive is an LTO-8 emulation and the cartridges' serials look like
`E01001L8_1775794348`. On a real drive the commands and the shape of the output are
the same.

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
> **Write the secret down on paper before you do anything else.** It is not stored
> anywhere on the machine and is never shown again. It is the last-resort key to
> every tape you will ever write — see [Keys and recovery](keys-and-recovery.md).

The operator tenant (`mike` here) is created by `init`; don't `tenant add` it again.

## 2. Staging and a drive

Staging is where encrypted slices wait before they go to tape; it needs room for
everything one tape will hold. Set it in `~/.tapectl/config.toml`
([reference](configuration.md)):

```toml
[staging]
directory = "/srv/staging"
```

Then tell tapectl about the drive. You declare only what the **drive** is; each
cartridge's own generation is read from the cartridge later.

```bash
tapectl backend add --name lto8 --device-tape "$TAPE" --device-sg /dev/sg5 --generation LTO-8
tapectl config check
```

```text
backend "lto8" added to ~/.tapectl/config.toml (LTO-8, tape=/dev/tape/by-id/scsi-XYZZY_A1-nst, sg=/dev/sg5)
verify it with: tapectl config check

config: valid
dar: 2.7.13 at '/usr/bin/dar' (meets minimum 2.6)
staging: '/srv/staging' exists and is writable
```

The `sg` node is the drive's SCSI-generic twin, used for health pages and the
cartridge's memory chip: `ls /sys/class/scsi_tape/nst0/device/scsi_generic/` names it
(`lsscsi -g` shows the pair).

## 3. Places for cartridges

A **location** is somewhere a cartridge can physically be. Two copies in two places
is what protects you from a fire or a burglary.

```bash
tapectl location add home-rack -d "the shelf beside the drive"
tapectl location add offsite -d "a fire-safe box at a relative's house"
tapectl location list
```

```text
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
| family-backup  | backup  | yes    |        | age1p39zlwg4wf57xv2kk7dj... | 2026-09-28 21:43:58 |
+----------------+---------+--------+--------+-----------------------------+---------------------+
| family-primary | primary | yes    |        | age1guqq2ueeq3x6mddm4qwm... | 2026-09-28 21:43:58 |
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

`stage create` does the expensive work: it runs `dar` over the unit, checks every
file's sha256, encrypts the archive to the tenant's, the operator's and the escrow
keys, and writes the encrypted **slices** to staging.

```bash
tapectl stage create family/letters
tapectl stage create family/photos/2019-italy
tapectl stage create family/photos/2020-garden
tapectl stage create work/invoices-2024
tapectl volume plan --copies 2
```

```text
staged: family/letters (1 slices, 1.1 KiB dar, 1.7 KiB encrypted)
staged: family/photos/2019-italy (1 slices, 1.1 MiB dar, 1.1 MiB encrypted)
staged: family/photos/2020-garden (1 slices, 587.3 KiB dar, 588.1 KiB encrypted)
staged: work/invoices-2024 (1 slices, 1.1 KiB dar, 1.7 KiB encrypted)

volume write plan (2 copy/copies):
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  family/letters v1: 1 slices, 1.7 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB x 2 = 3.4 MiB
estimated tapes: 1 (at 92% usable capacity)
```

`volume plan` is an estimate. The authoritative capacity is read from the cartridge
at `volume init`, and `volume write` refuses an over-full plan before writing a byte.

## 8. Write, verify and shelve tape 1

Load a blank cartridge and write the label on it. `volume init` reads the cartridge's
chip: its generation (which fixes its capacity) and its serial (which is how tapectl
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
  family/letters v1: 1 slices, 1.7 KiB
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB
volume "L8-0001" write completed

verify L8-0001 (full tier): 13 checked, 13 passed, 0 failed
```

`volume write` plans the whole tape, writes it in one session, **reads every byte
back** against the plan, and only then seals it. A sealed volume is never appended
to. `verify --full` is a second, independent read that starts the tape's verification
history.

> [!NOTE]
> Before a write, tapectl checks that the host is quiet enough to keep the drive
> streaming (a slow feed costs tape). If it is not, it asks — or, with no terminal,
> refuses unless you pass `--yes`. See
> [Troubleshooting](troubleshooting.md) and `tapectl host check`.

Put the cartridge on its shelf and tell the catalog:

```bash
tapectl volume move L8-0001 --to home-rack
tapectl volume info L8-0001
```

```text
volume "L8-0001" moved to "home-rack"
  cartridge "E01001L8_1775794348" moved with it

Volume: L8-0001
  Status:      sealed
  Condition:   ok
  Backend:     lto (lto8)
  Media:       LTO-8
  Capacity:    3.5 MiB / 2.0 GB (0.2%)
  Cartridge:   E01001L8_1775794348
  ...
Units carried: 4 (1.7 MiB across 2 tenant(s), 2026-09-28 21:43:59)
    family/photos/2019-italy v1 (family) — 1.1 MiB
    family/photos/2020-garden v1 (family) — 588.1 KiB
    family/letters v1 (family) — 1.7 KiB
    work/invoices-2024 v1 (work) — 1.7 KiB
  ...
Verification history:
    2026-09-28 21:44:06 [full]: passed (13/13 slices passed)
    2026-09-28 21:44:05 [full]: passed (13/13 slices passed)
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
WARNINGS (2):
  [compaction_candidate] volume:L8-0001: utilization 49% < 50% threshold
    fix: tapectl volume compact-read L8-0001
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
    fix: tapectl key escrow-kit --out <dir>
audit: 4 violations, 2 warnings (exit 2)
```

One copy is not enough. The staged slices are still on disk (a write does not remove
them), so the second copy is just another cartridge:

```bash
tapectl volume init L8-0002 --device "$TAPE"
tapectl volume write L8-0002 --device "$TAPE"
tapectl volume move L8-0002 --to offsite
tapectl audit
tapectl report copies
```

```text
cartridge E01003L8_1775794348 auto-registered from MAM (barcode = medium serial)
volume "L8-0002" initialized (id=2)
...
volume "L8-0002" write completed
volume "L8-0002" moved to "offsite"
  cartridge "E01003L8_1775794348" moved with it

WARNINGS (3):
  [compaction_candidate] volume:L8-0001: utilization 49% < 50% threshold
  [compaction_candidate] volume:L8-0002: utilization 49% < 50% threshold
  [escrow_kit_missing] archive: 2 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 0 violations, 3 warnings (exit 1)

  family/letters: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  family/photos/2019-italy: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  family/photos/2020-garden: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
  work/invoices-2024: 2 copies, 2 locations [tapes holding any version: L8-0001,L8-0002]
```

The violations are gone. (The compaction warnings are an artefact of this tiny demo
cartridge; the Heir Kit warning is dealt with in step 13.)

## 10. Find things

```bash
tapectl catalog ls family/letters
tapectl catalog search letter
tapectl catalog search IMG 101
tapectl catalog locate family/letters
```

```text
+--------------------------+-------+---------------------------+-----------------+
| Path                     | Size  | Modified                  | SHA256          |
+--------------------------+-------+---------------------------+-----------------+
|   .tapectl-unit.toml     | 188 B | 2026-09-28T21:43:58+00:00 | d61033021064... |
+--------------------------+-------+---------------------------+-----------------+
|   1998-letter-to-mum.txt | 36 B  | 2026-09-28T21:43:58+00:00 | 770ef76f0932... |
+--------------------------+-------+---------------------------+-----------------+

  family/letters v1: 1998-letter-to-mum.txt (36 B)
1 result(s)

  family/photos/2020-garden v1: IMG_101.jpg (293.0 KiB)
1 result(s)

+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| Volume  | Status | Condition | Location  | Snapshot | Slices | Written             | Serviceable | Warehouse | Escrow | Verified |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0001 | sealed | ok        | home-rack | 1        | 1      | 2026-09-28 21:44:05 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
| L8-0002 | sealed | ok        | offsite   | 1        | 1      | 2026-09-28 21:44:09 | yes         | -         | yes    | 0d ago   |
+---------+--------+-----------+-----------+----------+--------+---------------------+-------------+-----------+--------+----------+
```

`catalog search` matches **file names inside units**, word by word, each word as a
prefix (`IMG 101` finds `IMG_101.jpg`; `garden` does not match a file called
`IMG_101.jpg` even though its unit is `2020-garden`). `catalog locate` answers
"which tape, and where is it?".

## 11. Restore

Load either copy. One file:

```bash
tapectl restore file --file 1998-letter-to-mum.txt --unit family/letters --from L8-0002 --to /tmp/restore --device "$TAPE"
```

```text
WARN tapectl::dar::restore: restoring as a non-root user: restored files will be owned by the invoking user, not their archived owners
restored "1998-letter-to-mum.txt" from "family/letters" on L8-0002 to /tmp/restore
```

A whole unit (`--dry-run` first shows what would happen):

```bash
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE" --dry-run
tapectl restore unit --unit family/letters --from L8-0002 --to /tmp/restore/unit --device "$TAPE"
diff -r /media/family/letters /tmp/restore/unit && echo identical
```

```text
would restore "family/letters" v1 from L8-0002 (1 slices) to /tmp/restore/unit
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

`staging clean` keeps any unit that still needs more copies than it has, so it is
safe to run between copies.

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
  catalog.db.age   encrypted catalog, 668082 bytes, covering 2 sealed volume(s)

still to do, and only you can do it:
  1. print COVER.txt (or the HTML page)
  2. copy the escrow SECRET (AGE-SECRET-KEY-1…(redacted)..., shown once by `tapectl init`)
     by hand into the box marked WRITE IT HERE -- the kit prints only the
     public half, and without the secret the sheet opens nothing
  3. seal it in a tamper-evident envelope
  4. store copies in at least TWO independent failure domains
```

Regenerate it after every write session (`audit` reminds you when it is stale). Back
up the catalog too — it holds no private keys unless you ask:

```bash
tapectl db backup --to /mnt/backup/tapectl.db
```

```text
database backed up to /mnt/backup/tapectl.db (private keys not included — pass --include-keys to copy them)
```

## 14. Rehearse a disaster

Suppose the machine is gone: no database, no keys — only the tapes and the paper.
On a fresh machine, start a new home that **adopts** the original escrow identity
(its public key is on the Heir Kit), point it at the drive, and rebuild the catalog
from a tape with the escrow secret you wrote down:

```bash
tapectl init --operator mike --escrow-public-key age1zczqp0emrmylxcetrrz5k8xu5j097dlptp4eysje0vgu496mgc5qaq0nmp
tapectl backend add --name lto8 --device-tape "$TAPE" --device-sg /dev/sg5 --generation LTO-8
tapectl catalog rebuild --from-volume --key escrow.key --device "$TAPE"
tapectl unit list
```

(`escrow.key` is a file containing the one `AGE-SECRET-KEY-1…` line from the paper;
delete it afterwards.)

```text
tapectl initialized at ~/.tapectl
  operator: mike
  ...
  escrow:   adopted age1zczqp0emrmylxcetrrz5k8xu5j097dlptp4eysje0vgu496mgc5qaq0nmp (imported — its secret lives on the heir kit, not here)

rebuilt from volume "L8-0002" (uuid a5379113-e90f-4e97-86c5-66368cb6c406), 3 envelope(s) opened
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
