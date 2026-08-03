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
file inside it (issue #109).

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

This is the documented external procedure (issue #72). tapectl does not run
any of step 2 for you — it produces the bytes and records the result.

> **Which lines here have been executed.** Every `tapectl`, `age`, `dar`,
> `tr` and `sha256sum` invocation in this section and the next was run against
> a real dumped volume, and the outputs quoted are the real ones. The `rclone`
> and `aws` lines were **not** — neither tool is installed on the development
> box, so they are transcribed from vendor documentation and are the one part
> of this procedure to re-check against your own installed version before you
> rely on it. Do a dry run against a scratch bucket before your first real
> deposit; `rclone copy --dry-run` will tell you if a flag has moved.

**1. Dump the sealed volume to a directory.** `restore raw-volume` reads a
tape using only what is on the tape itself, verifies every file against the
front index as it goes, and needs no database:

```bash
tapectl restore raw-volume --device /dev/nst0 --to /staging/MHVTLR3 \
  --from MHVTLR3
```

`--from` is a wrong-tape guard against the tape's own reported label, not a
catalog lookup. Expect output like `dumped 23 files … verified: 21
mismatched: 0 unverifiable: 2`. **`mismatched` must be 0.** The two
unverifiable files are always the front index and the seal marker — neither
can carry its own hash — so `2` is the correct number, not a warning.

Files land named `{position:04}_{type}.bin`, e.g.:

```
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

```
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

> **Carried by the Heir Kit (issue #69, discharged).** The two caveats above
> — the billing fragility and the retrieval wait — are exactly what an heir
> needs, because they will meet this copy with no context at all. The printed
> cover sheet now says both, in plain language, and only when the catalog
> actually records a deposit (a tape-only archive gets no cloud paragraph).
> See "The Heir Kit" below.

**A deposit stops counting when its source volume does.** Deposits are gated
on the source volume still being `sealed`, so quarantining or retiring the
cartridge also removes its deposit from every count — even though the cloud
object itself is unaffected. That is the conservative reading, chosen so a
deposit can never be the thing that keeps a unit looking covered after its
tape went bad.

## Cadence

Everything below is a **manual rhythm you run**. tapectl schedules nothing and
has no daemon or listener — that shape is permanently out of scope (issue #13's
ratified verdict: every cost of a server exists to serve multi-machine access
this system does not have). The read-only advisory half *can* be put on a
systemd timer — see [Scheduling the advisory half](#scheduling-the-advisory-half)
below — but that is a wrapper around the same manual commands, not a daemon.
`volume write` stays manual forever, because it needs a human and a
physically-present cartridge.

The two operations that need no tape in the drive are the ones worth doing
often, because they cost nothing but attention:

```bash
tapectl audit               # 0 = clean, 1 = warnings, 2 = violations
tapectl report verify-status
```

### Weekly — cheap, no tape

Run `tapectl audit`. It implements all six compliance checks, including copy
count, location presence, and **verification age against each unit's resolved
`verify_interval_days`**. That last check is what produces your "what is overdue"
list — you do not track it yourself. Per ADR-0004 it is advisory: it warns, it
never blocks, and a stale volume still counts as a copy.

`tapectl report dirty` and `tapectl report pending` are the companion glance —
what has drifted since its last snapshot, and what is staged but not yet on tape.

### Scheduling the advisory half

The weekly glance is the one part of the cadence a machine can do for you,
because it is read-only and needs no tape in the drive. `contrib/systemd/`
carries a timer, a service, and a wrapper script for exactly that:

```bash
sudo install -Dm755 contrib/systemd/tapectl-scheduled-audit.sh \
    /usr/local/lib/tapectl/tapectl-scheduled-audit.sh
sudo install -Dm644 contrib/systemd/tapectl-audit.service \
    /etc/systemd/system/tapectl-audit.service
sudo install -Dm644 contrib/systemd/tapectl-audit.timer \
    /etc/systemd/system/tapectl-audit.timer

# Both CHANGEME placeholders in the .service must become your username —
# tapectl resolves ~/.tapectl from $HOME, and a systemd service inherits none.
sudoedit /etc/systemd/system/tapectl-audit.service

sudo systemctl daemon-reload
sudo systemctl enable --now tapectl-audit.timer
systemctl start tapectl-audit.service   # run once now to check it works
journalctl -u tapectl-audit.service -n 50
```

The wrapper runs `tapectl audit` followed by `tapectl report verify-status`, and
its exit status is `audit`'s:

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
check. The verification-age *check* with real exit codes is one of `audit`'s six
compliance checks, so nothing is lost.

Set `TAPECTL_HEALTHCHECK_URL` in the service to ping a healthchecks.io-style
endpoint (`/start` before, bare URL on 0 or 1, `/fail` on 2). It is off unless
set, and a missing `curl` or a failed ping never changes the run's own result.

Two things the timer does **not** change:

- **It never writes.** No tape command is scheduled, ever. The service sets
  `PrivateDevices=true` so it cannot reach `/dev/nst*` even by mistake.
- **It is safe to fire during other work, with one cosmetic caveat.** Opening
  the database runs the startup sweep, which marks an `in_progress` write
  session `interrupted` — so an audit landing in the middle of a `volume write`
  produces a spurious "recovered orphaned write sessions" event. The session
  stays fully resumable and revalidates on resume, so nothing is lost, but
  prefer a schedule outside your usual write window to keep the event log
  honest.

### Monthly — verify a rotating slice of the library

Do **not** try to verify every volume every month. The rotation exists to bound
drive and tape wear, which is the same reason snapraid scrubs ~8% of an array
per pass rather than all of it. tapectl has no percentage selector and computes
no rotation for you — this is a human procedure driven by one report:

```bash
tapectl report verify-status          # verification recency, oldest first
tapectl volume verify L6-0003         # --full is the default
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
*Raw-recovery drill* section. Follow it there rather than a second copy here; two
drifting checklists is the failure this repo keeps finding in code, and Markdown
is no safer. The drill's essentials: load a real tape, pull `RESTORE.sh` off the
plaintext front zone with `mt` + `dd`, and run it using **only** `mt`, `dd`,
`age`, `dar`, and `sha256sum` — no `tapectl` — against nothing but the printed
key material.

A drill is a CTO-scheduled session on real hardware. Nothing automated performs
one.

While you are there, confirm your off-tape recovery inputs are current:

```bash
tapectl db backup --to /path/to/backup        # add --include-keys only if the
                                              # destination is treated as secret
tapectl key list --tenant <name>              # find the aliases, then
tapectl key export <alias>                    # public half, for the record
```

### The Heir Kit

`key escrow-kit` generates the kit (ADR-0005 / ADR-0009, issue #69):

```bash
tapectl key escrow-kit --out ~/heir-kit
```

It writes three files:

| file | what to do with it |
|---|---|
| `COVER.txt` | **Print this.** The plain-text cover sheet, carrying the escrow key in retypable Bech32. It is the artifact with the decades-scale claim — readable with `cat` when no browser exists. |
| `escrow-kit.html` | Same content with the key as a QR. Open it and use the browser's print dialog. Self-contained: it renders with no network. |
| `catalog.db.age` | The whole catalog, encrypted to the escrow recipient. Put it on the media that travels with the paper. |

**The command stops at the files. The rest is yours, and the kit is worth
nothing until you do it:**

1. print `COVER.txt` (and/or the HTML page);
2. seal into a **tamper-evident envelope**;
3. store copies in **at least two independent failure domains** — not two
   shelves in one building;
4. paper in a UL-350 safe; Class-125 if stored together with tape.

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

`--to` must be a **shelf** location. A warehouse destination is refused
(issue #100): `volumes.location_id` records where to go to *fetch* the
cartridge, and the answer can never be an S3 bucket. To record that a copy of
a volume's bytes was uploaded to cold storage, use `volume deposit add`
instead — a deposit is a separate fact in a separate table, because a
cartridge has one location while a volume can carry several deposits and a
deposit never moves.

`volume retire` shows an impact analysis of which units lose copies, and — for
every affected unit that still retains coverage after the retirement — which
volume that remaining coverage rests on and how old its last passing
verification is (ADR-0004 Tier 1, issue #91). This appears in the plain-text
impact analysis, in `--json` (an `evidence` array plus an `evidence_summary`
string per affected unit), and again in the Tier-2 consent prompt when some
*other* unit in the same retirement is genuinely at risk. It is advisory and
never blocks — a stale or missing verification never stops the retirement, it
just tells you what you're trusting. `report verify-status` before the fact
remains the right way to check coverage across the fleet, not just at the one
volume you're about to retire.

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
