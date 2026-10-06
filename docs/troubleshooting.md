# Troubleshooting: when tapectl refuses, or something fails

This page is for the moment tapectl stops and prints `error: …`, when an audit
comes back red, or when an installer run halts. It explains what the exit code
means and how consent works: which refusals a flag can override and which none
can. After that comes a catalogue of the refusals and failures you are likely to
meet, in workflow order (setup → stage → write → verify → restore → audit). Each
entry gives the message as the code prints it, what it means, and the fix.

A few conventions:

- Commands are written `tapectl …`. On a host installed with
  `scripts/first-run.sh`, tapectl runs as a service user, so the same command is
  `tapectl-op …` (or `sudo -u tapectl -H tapectl …`). See
  [install.md](install.md#7-the-timers-and-the-operator-wrapper).
- Messages are quoted from the source, with placeholders shown as `<label>`,
  `<device>`, `<unit>` and so on. Most messages are one line, and the long
  ones are wrapped here so they are easier to read. Some have line breaks of
  their own: one fact per line under a refusal, a command recipe, a config
  example, one line per unit. Those breaks are shown as tapectl prints them.
  To find a message in a log, search for a short phrase from it, never for a
  whole wrapped line.
- Examples name the drive by its stable by-id path. `/dev/nstN` numbers can
  change across reboots.

  ```bash
  TAPE=/dev/tape/by-id/scsi-<SERIAL>-nst
  ```

## Contents

- [Exit codes](#exit-codes)
- [How consent works](#how-consent-works)
- [Setup and configuration](#setup-and-configuration)
- [Staging](#staging)
- [Initialising a volume](#initialising-a-volume)
- [Writing a volume](#writing-a-volume)
- [Verifying a volume](#verifying-a-volume)
- [Restoring](#restoring)
- [Audit findings](#audit-findings)
- [`report summary` counts stage sets held in staging](#report-summary-counts-stage-sets-held-in-staging)
- [first-run.sh stopped](#first-runsh-stopped)
- [Related pages](#related-pages)

---

## Exit codes

Any command that fails prints `error: <message>` on stderr and exits **2**.
There are two exceptions. `volume verify` exits **3** on every error
([below](#volume-verify-0-2-or-3)). A **busy catalog** exits **75**
([below](#75-catalog-busy)). When one failure caused another, the
message is the whole chain, joined by `: `. For example, a bad config file
prints:

```text
error: failed to load config: configuration error: <path>/config.toml: TOML parse error at line 68, column 1
```

A mistyped command or flag (clap's usage error) also exits 2, with one
exception: when the parse has already reached `volume verify`, an error in
what follows exits 3. That covers a missing label, an unknown flag after
`verify`, `--full` together with `--quick`, and a flag with no value. An error
that comes before `verify` is an ordinary usage error and still exits 2: a
mistyped subcommand such as `volume verfy`, or an unknown flag in front of
`verify` (`tapectl --hme <dir> volume verify <label>`,
`tapectl volume --bogus verify <label>`). `--help` exits 0 everywhere.

A few commands finish their work and then report a verdict through the exit code:

| Command | 0 | 1 | 2 |
|---|---|---|---|
| [`audit`](cli/audit.md#tapectl-audit) | clean | warnings only | at least one violation (or an error) |
| [`host check`](cli/host.md#tapectl-host-check) | host is quiet | something tripped | an error |
| [`config check`](cli/config.md#tapectl-config-check) | config valid | never used | config invalid (or an error) |
| [`db fsck`](cli/db.md#tapectl-db-fsck) | clean | problems found (repaired or not) | database integrity is broken (or an error) |
| [`collection sync/status/plan/run`](cli/collection.md#tapectl-collection) | clean | a unit was refused because its dotfile could not be parsed, or (`sync`) a folder could not be registered — an invalid unit name, or a tenant or archive set that does not exist; the rest ran, and each failure is an `error:` line | an error |

Apart from `volume verify` and a busy catalog, every other command exits 0 on
success and 2 on error.

### 75: catalog busy

SQLite lets one process write the catalog at a time. A command that needs
the write lock waits 5 seconds for it. Commands that record finished work
wait up to 10 minutes: a stage set's slices and its finalization, a tape
session's positions, its seal and its confirm. If the lock is still held
after that wait, the command exits **75**, sysexits' `EX_TEMPFAIL`, with
`error: ... catalog busy ...` or `database is locked` in the message.

Exit 75 is not a verdict. The catalog is not damaged, and an `audit` that
exits 75 found neither violations nor warnings: it did not run. Run the
command again once the other one has finished (`ps -C tapectl` shows it). A
`stage create` that stops this way keeps its encrypted slices. The next
command marks the set `failed`, and `staging clean` reclaims it; stage the
unit again. Opening the catalog takes the write lock only to recover a
crashed session, so read-only commands such as `report`, `catalog` and `db
backup` run while another command writes.

`volume verify` keeps its own contract: a busy catalog is one more way to
reach no verdict, so it exits 3. The `contrib/` timer wrappers log 75 as
"catalog busy" and do not ping `/fail`.

### `volume verify`: 0, 2 or 3

A failed verify has two outcomes with opposite remedies, and each has its own
exit code:

| Exit | Meaning | What to do |
|---|---|---|
| 0 | every checked file matched | nothing |
| 2 | the verify **proved the medium bad**: the volume is quarantined (or, verified again, still is) and no longer counts as a copy | salvage what still reads and write it to another cartridge ([Verifying a volume](#verifying-a-volume)) |
| 3 | **inconclusive**: a read or transport failure, or an error before any verdict: no cartridge loaded, the wrong tape, an unknown label, a drive that cannot read this generation, `--dry-run` (which `verify` refuses), a database or config error, a command line that does not parse after `verify`. The volume is untouched | check the drive, the cartridge in it or the command line, then verify again |

Once its command line has reached `verify`, `volume verify` exits 2 only
through a quarantine. A script whose command line is known to parse can
therefore act on the exit code alone. A usage error before `verify` exits 2
without any quarantine ([above](#exit-codes)). `volume verify --json` reports the same verdict in its
`quarantine` field (an object when the volume is quarantined, otherwise
`null`).

---

## How consent works

Before anything destructive, tapectl asks for consent according to
[ADR-0008](adr/0008-destructive-consent-tiers.md). What is at risk decides
which of three tiers applies:

| Tier | What is at risk | What tapectl does | Can a flag override it? |
|---|---|---|---|
| 1: staleness | nothing new; the evidence is old ("last verified 15y ago") | displays it | nothing to override |
| 2: degraded but non-zero | a unit left short of its `min_copies`, `min_locations` or `required_locations`, a dirty unit, an older version released while its superseder is short of policy, a staging directory that may be too small, a host that is not quiet | shows the facts and asks | yes: `--yes` (on some commands `--force` too) |
| 3: zero coverage or incoherence | writing over a sealed cartridge, marking a never-archived unit tape-only, retiring a unit's last copy, a drive that physically cannot write the medium, a staging directory proven too small for the unit | refuses | **no**, and no prompt appears either |

A Tier-3 refusal reports a fact, not a risk. It tells you what has to change
(load another cartridge, erase the tape, take a snapshot, free some space),
and none of `--yes`, `--force` or answering `y` gets past it.

### The prompt, and the non-interactive refusal

When stdin is a terminal, a Tier-2 command prints its facts on stderr, then
asks:

```text
<action> — proceed? [y/N]
```

Only `y` or `yes` proceeds. Pressing Enter, or giving any other answer, stops:

```text
error: <action>: aborted, not confirmed
```

When stdin is **not** a terminal (cron, a systemd timer, a pipe, a script) and
no `--yes` was given, tapectl never waits for an answer that cannot come. It
refuses at once and includes the facts it would have shown at the prompt:

```text
error: <action> refused: non-interactive session with no confirmation given — refusing
rather than assuming consent (re-run with --yes to proceed)
<fact line>
<fact line>
```

Read the fact lines. If you accept them, re-run with `--yes`. The one command
this does not carry through is `volume compact`, which needs a terminal
whatever its flags say ([below](#volume-compact-needs-a-terminal)).

For example, `unit mark-tape-only` on a unit that has been staged but never
written:

```text
error: mark unit "<unit>" tape-only refused: non-interactive session with no confirmation given — refusing
rather than assuming consent (re-run with --yes to proceed)
insufficient copies: 0 < 2 required by unit "<unit>"'s policy
insufficient locations: 0 < 2 required ([defaults] location floor)
(`--force` or `--yes` confirms this in advance)
```

`unit mark-tape-only` reads `min_copies` from the unit's resolved policy
(dotfile > archive set > `[defaults]`) and the location floor from
`[defaults] min_locations`. Other fact lines it can show:

- `insufficient locations: no copy at required location(s) <names> (policy
  requires <list>)`
- `unit is dirty: on-disk contents changed since the last snapshot — <changes>`
- under the copies line, when the unit has more than one current version,
  `  version <n> of <m> current versions has the fewest copies: <c>`
- when the unit has copies, a line on the weakest of them, just before the
  `(--force …)` line: `coverage for unit "<unit>" rests on <label>, last
  verified <d> days ago`, or `coverage for unit "<unit>" rests on <n> copies;
  weakest is <label>, never verified`

A unit that meets its policy is marked without any question. A unit that was never
archived is refused whatever the flags (Tier 3): take a snapshot first.

### Which flag answers which question

`--force` on `volume init` and `volume write` has nothing to do with the tiers.
There it means "overwrite a cartridge whose File 0 names a different volume or
is empty" ([below](#file-0-identifies-a-different-volume-or-is-empty)). It never
answers a prompt.

| Command | Tier-2 prompt answered by |
|---|---|
| `volume write` (quiet-host pre-flight) | `--yes` |
| `stage create`, `collection run`, `quick-archive` (a staging directory that may be too small) | `--yes` |
| `volume retire`, `volume abort` | `--yes` |
| `volume compact-finish`, and step 3 of `volume compact` | `--yes` or `--force` |
| `cartridge edit --serial`, `db import` | `--yes` |
| `cartridge retire`, `cartridge mark-erased` | `--yes` or `--force` |
| `unit mark-tape-only` | `--yes` or `--force` |
| `snapshot mark-reclaimable` | `--yes` or `--force` |

`snapshot mark-reclaimable` has one refusal that no prompt answers. When no
current version supersedes the version you name, marking it reclaimable would
release the unit's only current version. Neither `--yes` nor answering `y`
accepts that; only `--force` does:

```text
error: no superseding current snapshot exists for v1 — marking it reclaimable would release "<unit>"'s only
current version. That is not a shortfall a prompt or --yes can accept; only an explicit --force overrides it.
```

Its coverage shortfalls go through the ordinary prompt, with the fact
`superseding v<n> has <c> copies, needs <m>`, `superseding v<n> has no copy at
required location(s) <names> (policy requires <list>)` or `superseding v<n> in
<l> locations, needs <m>` (a tape-only unit's figures carry `(tape-only
<k>x)`). Even with `--force`, the unit's policy has to resolve.

---

## Setup and configuration

### `tapectl is not initialized`

```text
error: tapectl is not initialized — run `tapectl init` first
```

tapectl found no home at the resolved location: `--home`, then
`TAPECTL_HOME`, then `~/.tapectl`. Either this really is a new machine (run
[`tapectl init`](cli/init.md#tapectl-init)), or you are running as the wrong
user or with the wrong home. On a first-run install the archive belongs to the
service user, so use `tapectl-op`.

### `init: --operator is required when run as a system account`

```text
error: init: --operator is required when run as a system account ("root", uid 0, below UID_MIN 1000). Without it
the operator tenant would be named after the account rather than the person who operates this archive. Re-run
with --operator <name>, e.g. `tapectl init --operator alice`.
```

Without `--operator`, `init` names the operator tenant after the login name
(`$USER`). Under a system account (a uid below `UID_MIN` in
`/etc/login.defs`, 1000 when unset), that would be the account's name, such as
`root` or the `tapectl` service user, so `init` refuses instead. `--dry-run`
refuses the same way. Name the person who operates the archive:

```bash
tapectl-op init --operator alice
```

The same applies to the disaster-recovery recipe,
`init --escrow-public-key <original>`, when you run it as the service user.

### Config: unknown key

Every section of `config.toml` rejects keys it does not know. A typo is an
error on every command, not a warning:

```text
error: failed to load config: configuration error: <path>/config.toml: TOML parse error at line 68, column 1
   |
68 | foo = 1
   | ^^^
unknown field `foo`, expected `level` or `format`
```

`tapectl config check` lists every problem at once and exits 2 while any
remain:

```bash
tapectl config check
```

```text
config: INVALID
  - configuration error: <path>/config.toml: TOML parse error at line 68, column 1
   ...
  - unknown key: logging.foo
```

The fix is to edit the named line. There is no `backend edit` command;
config.toml is the only place to change these settings. See
[configuration.md](configuration.md).

### Config: a key an older version wrote

Several keys have been removed, moved or renamed. For these, tapectl replaces
the generic "unknown field" error with a specific explanation. Delete the
line, or rename it where the message says so:

| Key in your config | What the message tells you |
|---|---|
| `[defaults]` `min_copies_for_tape_only`, `min_locations_for_tape_only` | renamed: the keys are now `min_copies` and `min_locations`, with the same meaning and values. Rename the lines. `first-run.sh` offers to do it at step 0 |
| `[[backends.lto]]` `media_type`, `nominal_capacity` | moved: declare the **drive's** generation as `generation = "LTO-6"`; capacity now follows the cartridge's detected generation ([ADR-0010](adr/0010-media-generation-is-a-cartridge-property.md)); `capacity_override` is for virtual drives only |
| `[[backends.lto]]` `block_size` | removed: the block size is a format constant (512 KiB) |
| `[[backends.lto]]` `hardware_compression` | removed: drive compression is always switched off |
| `[packing]` (`min_free_for_append`, `strategy`, `fill_threshold`) | removed: there is no append, and the batch selector is not configurable |
| `[labels]` `format` | removed: volume labels are always given by you |
| `[defaults]` `hash` | removed: every checksum is sha256; `checksum_mode` is the real knob |

A config written by an older `init` carries both renamed keys, and every
command refuses it until they are renamed:

```text
error: failed to load config: configuration error: <path>/config.toml: defaults.min_copies_for_tape_only and
defaults.min_locations_for_tape_only were renamed: the keys are now defaults.min_copies and
defaults.min_locations, with the same meaning — the copy and location requirement every unit starts from (an
[[archive_sets]] entry's min_copies overrides it). Rename the lines; the values carry over unchanged.
```

`config check` names each one on a line of its own:

```text
[defaults].min_copies_for_tape_only was renamed to [defaults].min_copies — the config will not load while the
old name is present. Rename the key; its meaning and value are unchanged.
```

Another example, from a config written before generations were detected:

```text
error: failed to load config: configuration error: <path>/config.toml: backends.lto["a"]: "media_type" and
"nominal_capacity" moved — declare the DRIVE's generation as generation = "LTO-6"; capacity now follows the
cartridge's generation (ADR-0010); capacity_override is for virtual drives only
```

### An archive set names a location that is not registered

An archive set's `required_locations` may name only locations registered with
[`location add`](cli/location.md#tapectl-location-add). A name nobody
registered could never be met, so it is refused wherever it is set. From
config.toml, by [`archive-set sync`](cli/archive-set.md#tapectl-archive-set-sync):

```text
error: archive set "photos": required_locations names "offsite", which is not a registered location (no locations
are registered yet). Register a location first with `tapectl location add <name>`, or fix the spelling.
```

From the command line, by `archive-set create` and `archive-set edit`:

```text
error: --required-locations names "offsite", which is not a registered location (registered locations: shelf).
Register a location first with `tapectl location add <name>`, or fix the spelling.
```

An empty name (`""` in the file, or a stray comma on the command line) is
refused too:

```text
error: archive set "photos": required_locations contains an empty location name — name a registered location
(`tapectl location list`)
error: --required-locations "shelf," contains an empty location name — give a comma-separated list such as
"home-rack,offsite"
```

`sync` checks every `[[archive_sets]]` table before it writes anything, so one
bad name holds back every set in the file, not just its own. `create` and
`edit` refuse under `--dry-run` too. The usual cause is writing
`required_locations = ["offsite"]` before registering the place. Register it,
then sync again:

```bash
tapectl location add offsite -d "the box at my sister's"
tapectl archive-set sync
```

See [configuration.md](configuration.md#archive_sets).

### No drive configured

```text
error: configuration error: no LTO backend configured — tapectl does not know what tape drive to use.

Add a [[backends.lto]] section to your tapectl config (by default ~/.tapectl/config.toml):
...
`tapectl init` leaves a commented-out example there to uncomment.
```

Every write path (`volume init`, `volume write`, `volume resume`,
`collection run`) needs a `[[backends.lto]]` entry, and so do the planners,
`volume plan` and `collection plan`, even with `--generation` (`volume plan`
prints its plan first and then fails). Uncomment the example
`init` left in config.toml, or add one with
[`tapectl backend add`](cli/backend.md#tapectl-backend-add).

That message appears only when you leave out `--device`. With `--device`,
which every example on this page passes, tapectl instead reports that no
backend has the path you named, and `configured: none` says there are no
backends at all:

```text
error: configuration error: no [[backends.lto]] entry has device_tape = <device> (configured: none)
```

The fix is the same: add the backend.

Read paths (`volume identify`, `volume verify`, `restore`, `catalog rebuild`)
work without a backend as long as you pass `--device`. This is deliberate, so
a rebuilt machine holding only keys can still read its tapes.

### Several drives: `pass --device to select one`

```text
error: configuration error: multiple LTO backends configured (<name>, <name>); pass --device to select one
```

When exactly one drive is configured, `--device` defaults to it. When there
are several, no default exists. Name the drive every time, by the same path
its `device_tape` uses:

```bash
tapectl volume write L6-0002 --device "$TAPE"
```

If `--device` names a drive config.toml does not list, write paths refuse and
show the paths they do know:

```text
error: configuration error: no [[backends.lto]] entry has device_tape = <device> (configured: <path>, <path>)
```

Name the drive by one of the listed paths, or add a backend for the one you
meant. If the list reads `configured: none`, no drive is configured at all
([No drive configured](#no-drive-configured)).

### `device_sg` is not the drive `device_tape` names

The cartridge chip (MAM) and the drive's log pages are read through the SCSI
generic node, `device_sg`. `/dev/sgN` numbers move across reboots, so a
`device_sg` that was correct yesterday can point at another device today. A
write path checks the pairing against the kernel's own binding and refuses on
a proven mismatch:

```text
error: configuration error: refusing to resolve an LTO backend for a write: backend "<name>": device_sg =
<configured> is not the drive device_tape = <device> names — the kernel binds that tape node to <bound>. Its
cartridge chip (MAM) and log pages would be read from a different device (/dev/sgN numbering moves across
reboots). Set device_sg = "<bound>" in this [[backends.lto]] entry.
```

The message names the correct node. Edit `device_sg` in that
`[[backends.lto]]` entry to match, then check the config:

```toml
[[backends.lto]]
name = "lto6"
device_tape = "/dev/tape/by-id/scsi-<SERIAL>-nst"
device_sg = "/dev/sg4"          # the node the refusal named
generation = "LTO-6"
```

```bash
tapectl config check
lsscsi -g        # optional cross-check: the tape line's last column is its sg node
```

On a read path the same mismatch only produces a warning. The read goes ahead
as if no backend matched, so it records no cartridge chip, log pages or drive.

### Opening the drive fails: `tape I/O error: open <device>: …`

When the kernel refuses to open the tape device, the operating system's reason
follows the prefix:

| The OS says | Usually means | Fix |
|---|---|---|
| `Permission denied (os error 13)` | your user (or the service user) is not in the device's group | `ls -l` the `nst` and `sg` nodes and add the user to their group (usually `tape`); log in again |
| `Device or resource busy (os error 16)` | another process has the drive open | find it (`fuser -v "$TAPE"`) and wait for it |
| `No medium found (os error 123)` or `Input/output error (os error 5)` | the drive never became ready: still loading, or a fault. An empty drive is caught before this, by name ([No cartridge loaded](#no-cartridge-loaded)) | wait for the drive to settle, reseat the cartridge, check `mt -f "$TAPE" status` |
| `No such file or directory (os error 2)` | `--device` names a path that does not exist (a typo, or a by-id link for a drive that is not attached) | `ls -l /dev/tape/by-id/` and use the path it lists |
| `Read-only file system (os error 30)` | the cartridge's write-protect tab is set, and the command writes: `volume init`, `volume write`, or a `volume resume` whose seal is not yet recorded | slide the tab, or use another cartridge. Commands that only read open the drive read-only and work with the tab set: `volume verify`, `restore`, `volume identify`, `catalog rebuild`, and a `volume resume` that only re-confirms a recorded seal |

When the path cannot be opened at all, the error follows a logged warning,
`non-blocking open failed during no-medium probe (continuing)`, from the
empty-drive check that runs first.

**tapectl takes no lock on the drive.** The kernel's `st` driver allows only
one open at a time, and that is what produces "busy". The file
`/tmp/tapectl-tape.lock` belongs to the test harnesses
(`scripts/mhvtl-verify-gate.sh`, `scripts/lifecycle-suite.sh`,
`scripts/lto6-measure.sh`, `scripts/lto6-fill.sh`) and to `first-run.sh`
step 13 (the step-12 rehearsal takes it through `lifecycle-suite.sh`). A plain
`tapectl` command neither takes nor checks it. The
only lock tapectl itself takes is a per-stage-set lock under
`<home>/locks/stage-<id>.lock`, held while `stage create` runs. The
[operator guide](operator-guide.md#a-quiet-host-while-the-tape-runs) asks you
to keep everything else off the drive while a tape runs.

### `key import refuses: this public key is already in the catalog`

[`key import`](cli/key.md#tapectl-key-import) registers a public key once. A
key the catalog already has is refused, and the message says which key it is
and in what state:

```text
error: key import refuses: this public key is already in the catalog as "family-primary" (tenant "family",
deactivated — `key rotate` deactivates the keys it replaces). To make it a recipient of new writes again, re-run
with --reactivate.
```

```text
error: key import refuses: this public key is already in the catalog as "<alias>" (tenant "<tenant>", active) —
nothing to import
error: key import refuses: this public key is already in the catalog as "<alias>", a key of tenant "<owner>"
(active) — a key belongs to one tenant, so it cannot also be imported for "<tenant>"
error: key import refuses: this public key is the escrow identity "<alias>" (ADR-0005) — it is already a
recipient of every write, and it is never imported, deactivated or reactivated as a tenant key
```

A new key under an alias that is already taken gives `key already exists:
<tenant>-<alias>`. `--dry-run` makes the same checks.

To make a tenant's own deactivated key a recipient of new writes again, pass
`--reactivate` and leave `--alias` out. The key keeps the alias it was
registered under:

```bash
tapectl key import ~/.tapectl/keys/family-primary.age.pub --tenant family --reactivate
```

```text
key "family-primary" reactivated: a recipient of new writes for tenant "family" again
```

`--reactivate` has refusals of its own. It refuses a key the catalog has never
seen, a key that is still active, and an `--alias` that is not the one the key
already has:

```text
error: key import --reactivate refuses: this public key is not in the catalog, so there is nothing to reactivate
— drop --reactivate (and give --alias) to import it as a new key
error: key import refuses: this public key is already in the catalog as "family-primary" (tenant "family",
active) — nothing to reactivate
error: key import --reactivate refuses: this public key is registered as "family-primary", not "family-oldkey" —
reactivation keeps the alias a key was registered under; drop --alias, or pass the one it has
```

For the last one, drop `--alias`. `--alias` takes only the part after
`<tenant>-` (`--alias primary` for `family-primary`), so passing the full name
the message shows, `--alias family-primary`, is refused too, as `not
"family-family-primary"`.

### `db backup refuses: …`

[`db backup`](cli/db.md#tapectl-db-backup) writes to exactly the file `--to`
names, and creates no directories. It refuses, before it opens anything and
under `--dry-run` too:

```text
error: db backup refuses: the destination directory <dir> does not exist — create it, or mount the medium it
lives on, and run again (db backup does not create directories)
error: db backup refuses: --to <path> is a directory; --to names the backup FILE, e.g. <path>/tapectl.db
error: db backup refuses: <dir> is not a directory, so --to <path> cannot be written
error: db backup refuses: --to <path> ends in .keys, which is exactly where --include-keys puts the private-key
directory — the database file and the key directory would be the same path; give --to another extension, e.g. .db
```

A missing directory is usually a backup disk that is not mounted. With
`--include-keys`, the private keys go beside the file, with its extension
replaced by `.keys`:

```bash
tapectl db backup --to /mnt/usb/tapectl.db --include-keys
```

At the default `[logging] level = "warn"`, a log line on stderr comes first,
then the result on stdout:

```text
<timestamp>  WARN tapectl::cli::operations: private key material copied to backup destination — treat this location as secret destination=/mnt/usb/tapectl.keys
database backed up to /mnt/usb/tapectl.db; private keys copied to /mnt/usb/tapectl.keys/ — treat that directory as secret
```

### `migration 026 cannot run`

The first command a new tapectl runs on an older catalog brings its schema up
to date. One step removes statuses no tapectl release has ever written. If a
row carries one anyway (it was set by hand), every command that opens the
catalog refuses. Only the few that never open it, such as `host check`, still
run:

```text
error: failed to open database: migration error: migration 026 cannot run: units.status = 'retired' on 1 row(s)
(id 1); snapshots.status = 'superseded' on 2 row(s) (id 2, 3). Migration 026 removes these statuses from the
schema. No tapectl release has ever written one, so these rows were set by hand, and 026 will not guess what
they should say. Change each named row to a status the new schema allows -- units: active, tape_only, missing;
snapshots: created, staged, current, reclaimable, purged; volumes: initialized, active, full, retired, erased,
sealed -- then run the command again. Nothing has been changed.
```

The statuses it removes are `retired` (units), `superseded` and `failed`
(snapshots), and `blank` and `missing` (volumes). Nothing has been changed:
the whole step rolls back. Neither `db fsck --repair` nor `db backup` can
help, because both have to open the database too. Decide, row by row, which
allowed status tells the truth about it, then set it by hand while no tapectl
command is running.

That takes `sqlite3`, the SQLite command-line shell. tapectl carries its own
SQLite and never needs the shell, so it may not be installed (on Debian or
Ubuntu: `sudo apt install sqlite3`). Keep a copy of the file first:

```bash
DB=~/.tapectl/tapectl.db
cp "$DB" "$DB.before-026"
sqlite3 "$DB" "UPDATE units SET status = 'active' WHERE id = 1"
sqlite3 "$DB" "UPDATE snapshots SET status = 'current' WHERE id IN (2, 3)"
tapectl unit list
```

On a first-run install the catalog belongs to the service user, in a home
only that user can enter. Give the file's full path, because a `~` would
expand to your own home, and run each line as that user. That home is not
always `/var/lib/tapectl`: a host profile's `SVC_HOME_WANT` moves it (home2's
profile puts it at `/srv/archive_meta/tapectl`), so ask the system where it
is rather than typing a path:

```bash
DB="$(getent passwd tapectl | cut -d: -f6)/.tapectl/tapectl.db"   # or <dir>/tapectl.db if first-run was given --home <dir>
sudo -u tapectl -H test -f "$DB" && echo "catalog: $DB"    # no line printed: wrong path, stop here
sudo -u tapectl -H cp "$DB" "$DB.before-026"
sudo -u tapectl -H sqlite3 "$DB" "UPDATE units SET status = 'active' WHERE id = 1"
sudo -u tapectl -H sqlite3 "$DB" "UPDATE snapshots SET status = 'current' WHERE id IN (2, 3)"
tapectl-op unit list
```

### `migration 027 cannot run`

Migration 027 drops the `manifest_entries` table, an older copy of each
snapshot's file list that nothing reads; the `files` table keeps every path,
size and checksum. Before it drops anything it checks that `manifest_entries`
holds nothing `files` does not. Every tapectl release has kept the two in step,
so this refusal means the catalog was edited by hand:

```text
error: failed to open database: migration error: migration 027 cannot run: 1 manifest_entries row(s) with no
files row for the same snapshot and path (id 77001). Migration 027 drops the manifest_entries table, which every
tapectl release has kept in step with files, so these rows were set by hand and 027 will not guess which table is
right. Make each named manifest_entries row agree with its files row (or delete it), then run the command again.
Nothing has been changed.
```

The other finding it can name is a `manifest_entries` row carrying a sha256
that its `files` row lacks or disagrees with. Nothing has been changed. Look at
the named rows with `sqlite3`, the same way as for migration 026 above, and
either copy the value into `files` or delete the `manifest_entries` row.

Once 027 applies, the same command compacts the catalog once (SQLite's
`VACUUM`), which needs free disk about twice the catalog's size. If that fails
it only warns: the catalog is correct, just larger than it needs to be.

---

## Staging

### A snapshot is incomplete

```text
error: snapshot photos v1 is incomplete: it records 48212 file(s) but the catalog holds 31007 file row(s) for
it, so an earlier `snapshot create` was interrupted partway. Staging it would put a short file list on tape.
Recover with `tapectl snapshot delete photos --version 1`, then `tapectl snapshot create photos`
```

A `snapshot create` from a tapectl before 1.0.2 wrote its file list one row at
a time, so a Ctrl-C or a busy catalog could leave the snapshot with only part
of it. `stage create` refuses such a snapshot before running dar. Delete it
and take it again; nothing has been staged or written from it:

```bash
tapectl snapshot delete photos --version 1
tapectl snapshot create photos
```

Since 1.0.2 a snapshot and its whole file list are recorded together or not
at all.

### No escrow recipient is registered

```text
error: no escrow recipient is registered — staged slices would be unrecoverable with the escrow key and volume
write would refuse them (ADR-0005). Register one first: `tapectl key generate --escrow`, or adopt an existing
public key with `tapectl key import --escrow <age1...>`
```

This home was created with `init --no-escrow`. `stage create` refuses before
running dar, so no time is lost. Register the escrow recipient once:

```bash
tapectl key generate --escrow
```

`key generate --escrow` shows the escrow **secret** once. Read
[keys-and-recovery.md](keys-and-recovery.md) before you run it, and have
somewhere to write the secret down.

### The tenant has no active keys

```text
error: tenant for unit "<unit>" has no active keys — refusing to encrypt (the tenant could not decrypt its own
data); run `tapectl key rotate` or restore the tenant's keys first
```

`stage create` checks every recipient before it runs dar or records a stage
set, so this refusal costs nothing. A malformed public key in the catalog is
refused at the same point. Give the tenant a key and stage again:

```bash
tapectl key list --tenant <tenant>
tapectl key rotate --tenant <tenant>
tapectl stage create <unit>
```

### dar is missing or too old

```text
error: dar not found at configured path: <binary>
error: dar version <found> below minimum 2.6
```

Both come from `[dar] binary` in config.toml (default `dar`, looked up on
`PATH`). Install dar 2.6 or newer (2.7.20+ recommended), or point `binary` at
it. `tapectl config check` prints the dar it found and whether it meets the
minimum:

```text
dar: 2.7.13 at '/usr/bin/dar' (meets minimum 2.6)
```

A dar that exists but fails while archiving shows up as `dar error: dar -c
failed (exit <status>): <first lines of dar's stderr>`.

### The staging directory cannot be created or written

```text
error: cannot create staging directory <directory>: Permission denied (os error 13)
error: cannot write to staging directory <directory>: Permission denied (os error 13)
```

`stage create` (and `collection run` and `quick-archive`, which stage through
it) checks `[staging] directory` before it reads the source. It creates the
directory if it is missing, then proves it can write there by creating and
removing a probe file (`.tapectl-stage-probe-<pid>`). `tapectl config check`
also names the directory and says whether it exists and is writable:

```text
staging: '<directory>' exists and is writable
```

Fix the ownership (the service user must own it on a first-run install), or
point `[staging] directory` somewhere else. Staging must not live on a small
root partition.

### Staging space: refused, or asked about

Staging peaks at the unit's dar archive plus one encrypted slice written
beside it: about the unit's size plus one `slice_size`, or twice the unit when
it fits in a single slice. `stage create`
compares that with the free space in `[staging] directory`, and one of three
things happens.

**It fits.** Nothing is said.

**It cannot fit.** With `compression = "none"`, every non-zero byte of the
unit is a byte dar must store. When free space is below that bound, the stage
is refused. This is a fact, not a risk, so no flag gets past it. The unit has
to be read to count those bytes, so the refusal comes after the sha256 pass
and before dar:

```text
error: not enough space in staging directory <directory> for unit "<unit>": staging it needs at least 5.7 MiB —
its dar archive (at least 2.8 MiB, the unit's non-zero bytes, every one of which dar stores) plus one encrypted
slice (2.8 MiB) written beside it — and 1.0 MiB is free. Free space there, or point [staging] directory at a
larger filesystem.
```

**It may not fit.** Then you are asked (Tier 2). When the question comes
depends on `compression`:

- With `compression = "none"`, which is what `init` writes, nothing about
  space is printed before the source is read. The question comes **after** the
  sha256 pass, which can take hours on a large unit, and only when free space
  lies between the unit's non-zero bytes and its apparent size. Zero-filled
  disk images, sparse files and hard links make the two differ:

  ```text
  staging directory <directory> may be too small for unit "<unit>": 1.0 MiB free, and staging it needs between
  340 B and 5.7 MiB (the low end counts only non-zero bytes: dar stores runs of zeros as holes and a hard-linked
  file once); if it runs out, dar fails partway and the partial slices are removed
  ```

- With compression switched on, nothing can say in advance how well the data
  will compress. The question comes **before** the source is read, whenever
  free space is below the uncompressed figure:

  ```text
  staging directory <directory> may be too small for unit "<unit>": <free> free, and staging it needs up to <n>
  if its data does not compress (compression = "<algorithm>") — the dar archive plus one encrypted slice beside
  it; if it runs out, dar fails partway and the partial slices are removed
  ```

A terminal is asked `stage unit "<unit>" — proceed? [y/N]`. A non-interactive
run without `--yes` refuses with the same figures
(`error: stage unit "<unit>" refused: non-interactive session …`). With
`compression = "none"` that refusal also comes only after the sha256 pass, so
an unattended run that should go ahead anyway needs `--yes` from the start.
With `--yes` the stage goes ahead and still prints the figures, ending
`— staging anyway (--yes given)`:

```bash
tapectl --yes stage create <unit>
```

If tapectl cannot read the free space at all, it says so and stages without
the check:

```text
warning: could not read the free space of staging directory <directory> (<error>); staging unit "<unit>" without a space check
```

A refused attempt leaves no files to clean up. A refusal before the read
costs nothing more. A refusal after it has cost the sha256 pass: the hard
refusal, and with `compression = "none"` also an `N` at the prompt or a
non-interactive refusal. It leaves the attempt's stage set for the next
tapectl command to mark `failed`, and a `WARN` line says so each time. If the disk fills
anyway, the failure comes from dar (`dar error: dar -c failed …`) or from
encryption:

```text
error: cannot encrypt <directory>/<slice>.dar to <directory>/<slice>.dar.age: No space left on device (os error 28)
```

The half-built stage set's files are removed either way.

### The source changed after the snapshot

`stage create` re-reads every file and checks it against the snapshot before
archiving:

```text
error: DIRTY: source file size changed: <path> (expected <n> bytes, found <n> bytes) — a real edit (size and
content both differ) since the snapshot was taken. Take a new snapshot (`tapectl snapshot create <unit>`) and
stage that instead.
error: source file missing: <path>
error: BITROT suspected: <path> — sha256 differs at an unchanged size (<n> bytes): baseline=<sha>,
current=<sha>. Refusing to stage; investigate before re-staging (`tapectl unit check-integrity <unit>` checks
every file against its recorded baseline).
```

For a real edit or a deleted file, take a new snapshot and stage that:

```bash
tapectl snapshot create <unit>
tapectl stage create <unit>
```

`BITROT suspected` is different. The file has the same size as before but
different contents, which is what silent disk corruption looks like. Check
the rest of the unit with `tapectl unit check-integrity <unit>`, and before
you archive the new contents, compare the file against a copy you trust.

### `policy sets encrypt = false, which tapectl never honours`

```text
warning: unit "<unit>": policy sets encrypt = false, which tapectl never honours — its slices are encrypted
anyway, to the tenant, operator and escrow recipients (ADR-0005). Remove `encrypt = false` from the unit's
archive set or from [defaults].
```

This is printed on every `stage create` of such a unit, whatever
`logging.level` says. The stage is not refused. Every slice is encrypted
([ADR-0005](adr/0005-permanent-escrow-recipient.md)). Remove the key, as the
message says, to silence it.

---

## Initialising a volume

[`volume init`](cli/volume.md#tapectl-volume-init) is tapectl's first contact
with a cartridge. It reads the chip, decides the generation and capacity, and
writes File 0.

### No cartridge loaded

```text
error: no cartridge loaded in <device>
```

Every command that touches the tape asks the drive first whether a cartridge
is in place, and refuses within a second when none is: `volume init`,
`write`, `resume`, `verify`, `identify`, `read-slices`, `compact-read`,
`compact`, `compact-write`, `restore unit`, `restore file`,
`restore raw-volume` and `catalog rebuild --from-volume`. `collection run` and
`quick-archive` meet the same check when they reach their write, after
staging. This holds on a rebuilt machine with no backend configured as well.

Nothing is recorded for the refusal, and `--force` does not reach it, because
there is nothing to override. `volume verify` on an empty drive exits 3. Load
the cartridge and run the command again. To check the drive yourself:

```bash
mt -f "$TAPE" status     # DR_OPEN means no tape in place
```

### File 0 identifies a different volume, or is EMPTY

```text
error: refusing to write volume "<label>" (uuid <uuid>): the loaded cartridge's File 0 already identifies a
DIFFERENT volume (label="<other>", uuid="<uuid>") — this looks like the wrong physical cartridge. Verify the
correct tape is loaded, or if you are deliberately overwriting this cartridge, re-run with --force.
```

```text
error: refusing to write volume "<label>" (uuid <uuid>): the loaded cartridge's File 0 is EMPTY — a filemark at
the beginning of the tape with no bytes before it, not a tapectl ID thunk. That identifies no volume, but it is
not a blank tape either: something wrote that filemark, and tapectl only writes to a cartridge it can prove is
blank or its own. If you know what this cartridge holds and are deliberately overwriting it, re-run with --force.
```

When File 0 holds bytes that do not parse, the first message reads `a present
but unparseable/corrupt File 0` in place of the label and uuid.

tapectl writes without asking only to a cartridge it can prove is blank, or to
one that is already its own. First find out what is actually loaded:

```bash
tapectl volume identify --device "$TAPE"
```

If this is the wrong tape, swap it. If you mean to overwrite it (a stale
volume, a tape erased with `mt weof`), re-run with `--force`:

```bash
tapectl volume init L6-0003 --device "$TAPE" --force
```

When the cartridge's chip serial matches a cartridge the catalog knows,
re-initialising it records the displacement: the volume it held is marked
`erased`, and tapectl warns about any unit that now has fewer copies:

```text
warning: cartridge <barcode> previously held volume "<old>"; it is now marked erased because these bytes are
being overwritten (ADR-0010).
         *** unit "<unit>" [<unit status>] now has ZERO copies ***
```

### The cartridge already carries a SEALED volume

```text
error: refusing to write volume "<label>": the loaded cartridge already carries a SEALED volume — a valid seal
marker parses at tape position <n>. ADR-0003: sealed volumes are immutable, there is no append, and --force
cannot override this. If this cartridge should be reused: retire its current volume, erase the tape in the drive
(a long erase, `mt -f <device> erase`, takes hours on real LTO; a filemark at its start, `mt -f <device> rewind;
mt -f <device> weof 1`, takes seconds and then needs `volume init --force`), then run `tapectl cartridge
mark-erased` before writing to it again. Never degauss or bulk-erase an LTO cartridge: that destroys its factory
servo tracks, and the cartridge with them.
```

This is a Tier-3 refusal. A sealed tape is never appended to or written over,
and no flag changes that ([ADR-0003](adr/0003-sealed-volumes-immutable-no-append.md)).
`volume init` finds the seal through the tape's own File 0, so it catches a
sealed tape of any label. If you really mean to reuse the cartridge:

1. Retire the volume it holds. The command shows which units lose a copy, and
   it refuses outright if this is a unit's last copy:
   ```bash
   tapectl volume retire <old-label>
   ```
2. Erase the tape **in the drive**. Never use a bulk eraser (degausser) on an
   LTO cartridge: LTO tape carries servo tracks written at the factory, a
   degausser wipes them, and no drive can use the cartridge after that. Two
   ways work:
   - A long erase overwrites the whole tape and leaves it blank. It takes
     hours on real LTO:
     ```bash
     mt -f "$TAPE" erase
     ```
   - A filemark at the start of the tape takes seconds. The old data stays on
     the tape past it, unreachable to a normal read but not overwritten:
     ```bash
     mt -f "$TAPE" rewind
     mt -f "$TAPE" weof 1
     ```
3. Record that its bytes are gone, then initialise it again:
   ```bash
   tapectl cartridge mark-erased <barcode>
   tapectl volume init <new-label> --device "$TAPE"
   ```
   After a long erase the tape is blank and `volume init` needs nothing more.
   After the filemark the seal is no longer readable, `volume init` reports
   File 0 as EMPTY, and wants `--force` (see the previous entry):
   ```bash
   tapectl volume init <new-label> --device "$TAPE" --force
   ```

### The drive reports no medium serial: name the cartridge

```text
error: this drive reports no medium serial, so tapectl cannot tell which physical cartridge is loaded. Name it:
    tapectl volume init <label> --device <dev> --cartridge <barcode>

A volume no cartridge claims is a copy the catalog cannot place: ... Register the cartridge first if it is new
(`tapectl cartridge register --barcode <barcode> --generation <GEN>`). There is no --force for this — it is a
fact tapectl cannot resolve on its own, not a risk to accept.
```

A cartridge is identified by its chip serial
([ADR-0012](adr/0012-copies-are-identical-content-cartridges-are-known-by-serial.md)).
When the chip cannot be read (an old drive, or a `device_sg` pointing
elsewhere; see [above](#device_sg-is-not-the-drive-device_tape-names)), you
have to say which cartridge this is:

```bash
tapectl cartridge register --barcode L6-0003 --generation LTO-6
tapectl volume init L6-0003 --device "$TAPE" --cartridge L6-0003
```

Related refusals:

- `cartridge "<barcode>" is not registered. Register it first …`: `--cartridge`
  named a barcode the catalog has never seen.
- `cartridge "<barcode>" is registered with medium serial <recorded>, but the
  drive holds <loaded>. That is a different physical cartridge.`: load the one
  you named, or leave out `--cartridge`.
- `… so tapectl cannot confirm the tape in the drive is cartridge "<barcode>" —
  and "<barcode>" is bound to volume "<label>", which is still live.`: tapectl
  cannot tell an erased cartridge from a different tape wearing the same
  sticker. The message lists the `volume retire` commands, or
  `cartridge mark-erased <barcode>`, that settle it; run them only if they are
  true.
- `cartridge "<barcode>" is retired_permanent and must never be written again
  (ADR-0011)`: if the cartridge really is usable, run
  `tapectl cartridge unretire <barcode>`.

### A generation the drive cannot write

```text
error: an <drive-gen> drive cannot write <medium-gen> media. This is a physical limit of the drive, not a
policy — --force does not override it. Load a <drive-gen>-writable cartridge, or write this one in a drive that
can.

If this drive is not really an <drive-gen>, `generation` in the [[backends.lto]] block named "<name>" (<device>)
is wrong — edit config.toml (`tapectl config show` prints it; there is no `backend edit`, by decision:
ADR-0012) and run `tapectl config check`.
```

Up to LTO-7, a drive writes its own generation and the one before, and reads
one generation further back. LTO-8 and later break that pattern: an LTO-8
drive handles only LTO-7, LTO-7 Type M and LTO-8 media. tapectl detects the
cartridge's generation from the medium itself. Check which side is wrong:

- The cartridge really is too old (or too new) for the drive: use another
  cartridge, or another drive.
- The drive is not the generation config.toml claims: fix `generation` in that
  `[[backends.lto]]` entry.

The reading side has the same refusal, `an <drive-gen> drive cannot read
<medium-gen> media`, with the same fix.

When no source reports the medium's generation, `volume init` goes ahead and
says so:

```text
warning: medium generation not detectable from this drive; assuming <gen> (from <source>)
```

The capacity for the volume's whole life is decided from that generation, so
check the assumption before you write.

---

## Writing a volume

[`volume write`](cli/volume.md#tapectl-volume-write) runs a fixed sequence of
checks against the catalog, then the empty-drive check, then the quiet-host
pre-flight, then the checks that need the tape, and only then writes. A
refusal at any stage leaves the tape untouched.

### Not a write target

Only a volume that `volume init` left `initialized`, and that has never been
written, can be written. No flag overrides any of these four:

```text
error: volume "<label>" is <status> and is not a write target (ADR-0012): only a volume that `volume init` left
`initialized` can be written, and a sealed volume is never written again (ADR-0003). ...
```

```text
error: volume "<label>" already has a completed write recorded and is not a write target (ADR-0012): ...
```

```text
error: volume "<label>" is quarantined (ADR-0012, the 2026-09-17 amendment) and is not a write target: a prior
contact check found evidence this medium cannot be trusted, ...
```

```text
error: refusing to write volume "<label>": the catalog RECORDS its seal (a write session sealed this cartridge;
`volumes.sealed_at` is set). ADR-0003: sealed volumes are immutable, there is no append, and --force cannot
override this. ...
```

Each message ends with its own way forward. Most often that is to start a new
volume on another cartridge with `tapectl volume init <new-label>`. The last
one may instead name a `volume verify` followed by a `volume resume`, which
re-confirms a sealed session without writing anything.

### `no staged data to write`

```text
error: no staged data to write — run `tapectl stage create` first
```

Nothing is in staging. Snapshot and stage the units first
([walkthrough](walkthrough.md#7-stage)).

### `volume write` writes everything in staging

This is not an error, but it surprises people. `volume write` puts **every**
stage set that is still staged onto the tape, and it names them before it
touches the drive:

```text
about to write to volume "L8-0001":
  family/letters v1: 1 slices, 1.8 KiB
  family/photos/2019-italy v1: 1 slices, 1.1 MiB
  family/photos/2020-garden v1: 1 slices, 588.1 KiB
  work/invoices-2024 v1: 1 slices, 1.7 KiB

total: 4 slices, 1.7 MiB
```

A stage set stays in staging after it has been written, until
`staging clean` releases it. That is how the same staged data becomes the
second copy on the next tape. It also means leftovers ride along. No flag
picks a subset: if the list is wrong, stop, run
[`staging clean`](cli/staging.md#tapectl-staging-clean) to release what is
already safely on tape, and start again. See the
[operator guide](operator-guide.md#archive-to-tape).

### Wrong cartridge or wrong medium loaded

```text
error: wrong cartridge: volume "<label>" was initialised on <serial> (cartridge <barcode>), the drive holds
<serial>. Load that cartridge, or `volume init` a new label on this one.
```

```text
error: wrong medium: volume "<label>" was initialised on <gen> media, the drive holds <gen>. Its plan, capacity
gate and ID thunk all assume <gen>.
```

The cartridge in the drive is not the one this volume was initialised on. Load
the right one. `tapectl volume info <label>` shows the cartridge it is bound
to.

### The quiet-host pre-flight

A drive fed more slowly than it streams stops and restarts, which wastes tape.
A process killed for memory mid-write costs the whole session. So before
`volume write` touches the drive, it runs the same check as
[`tapectl host check`](cli/host.md#tapectl-host-check). A quiet host is never
asked anything. When something trips, you get a Tier-2 prompt with the
findings as its facts. In a non-interactive session without `--yes`, the
write refuses:

```text
error: volume write "<label>" on a host that is not quiet refused: non-interactive session with no
confirmation given — refusing rather than assuming consent (re-run with --yes to proceed)
host check: <what> — <measured> (<threshold>)
host check: the drive stops and restarts (costing tape) when the host feeds it slower than ~54 MB/s, and a
process killed for memory mid-write costs the session — pause what is named above for the duration
(`docs/operator-guide.md`, "A quiet host while the tape runs")
```

With `--yes`, the same findings are still printed. They go to stderr before
the write proceeds.

To see what tripped, run the check by itself. This is the tour's host, which
was quiet:

```bash
tapectl host check
```

```text
load average       2.69 over 16 CPUs = 0.17/CPU             max 1.00/CPU
available memory   8071 MiB                                 min 2048 MiB
memory pressure    0.00% full avg60                         max 10.00%
I/O pressure       0.60% full avg60                         max 10.00%
processes          cargo, rustc, docker, Runner.Worker      contender_processes
units              (none listed)                            contender_units
host check: quiet — nothing listed above is running or over its limit
```

The `processes` row lists the names it **watches for**, not processes it
found. A finding line names what is actually running. Here is one with the
limits tightened on purpose (`max_load_per_cpu = 0.01`,
`min_available_mb = 999999`, `contender_processes = ["bash"]`):

```text
host check: load average — 2.35 over 16 CPUs = 0.15 per CPU (max_load_per_cpu 0.01)
host check: available memory — 7339 MiB available (min_available_mb 999999)
host check: process "bash" — 3 running (pid 1792542, 1875134, 2985361) (listed in contender_processes)
host check: the drive stops and restarts (costing tape) when ... (`docs/operator-guide.md`, "A quiet host while the tape runs")
```

`host check` exits 1 when anything trips. It never refuses anything itself.
Pause what it names (CI runners and their timers, container builds, other
backups), then write. The limits and your host's own contenders live in
`[host_check]` ([configuration.md](configuration.md)). `--unit <name>` checks
an extra systemd unit for one run.

**A kernel without PSI.** Memory and I/O pressure come from `/proc/pressure/`,
which only exists on Linux 4.20 and later. On an older kernel, those rows read
`not available` and `host check` adds:

```text
host check: memory pressure and I/O pressure not measured (no /proc/pressure: PSI needs Linux 4.20+) — watch the staging disk yourself
```

That is not a finding. `host check` still reports "quiet" and exits 0 if
nothing else tripped, and the `volume write` pre-flight says nothing about it
at all. On such a host, watching disk load during a write is up to you.

### Over-full plan: the pre-flight capacity gate

```text
error: volume "<label>" failed pre-write validation: capacity exceeded: on-tape <needed> + reserve <reserve> > available <available>
```

tapectl has no end-of-tape salvage, so the capacity gate is its only defence
against running out of tape. It compares the whole planned layout with the
capacity decided **at `volume init`**: the cartridge's detected generation,
times `usable_capacity_factor`, less the `enospc_buffer` reserve. The staged
batch does not fit on this cartridge. Nothing has been written.

Size the batch before you write:

```bash
tapectl volume plan --copies 2
tapectl staging status
```

Release what is already on enough tapes (`staging clean`), so that less rides
along. Or split the work across cartridges. The
[Collection](cli/collection.md#tapectl-collection) commands batch
folder-per-unit archives to fit.

### The escrow key cannot open a staged set

```text
error: volume "<label>" failed pre-write validation: stage set <id> for unit '<unit>' was encrypted without the
current escrow recipient (<reason>) — its slices cannot be recovered with the escrow key; re-stage it: tapectl
staging clean --force (releases the old set), then tapectl stage create <unit> --version <snapshot version>
(ADR-0005)
```

The set was staged before this archive's escrow recipient existed, or for a
different one. Re-stage it as the message says. `--allow-missing-escrow` exists
only for copying a dying pre-escrow tape forward. See
`tapectl volume write --help`.

### An unfinished write session

```text
error: volume "<label>" already has an unresolved write session (status planned/in_progress/interrupted). If it
was interrupted, reload the same cartridge and run `tapectl volume resume <label>` — ...
```

An earlier write stopped part-way. A fresh write would not match what is
already on the tape, so the session is resumed, never rewritten:

```bash
tapectl volume resume L6-0003 --device "$TAPE"
```

`volume resume` explains itself when there is nothing it can pick up:

- ``has a `planned` write session, not an interrupted one: … nothing was ever
  written to tape``: clear it with `tapectl volume abort <label>`, then run
  `volume write` again.
- ``has an `in_progress` write session … ANOTHER PROCESS IS WRITING THIS TAPE
  RIGHT NOW``: tapectl turns crashed sessions into `interrupted` whenever it
  opens the database, so a row still `in_progress` means a writer is live.
  Find it and do not start a second one.
- `has no write sessions at all` / `its write sessions are all resolved`: use
  `volume write`.

### The write seems stuck: nothing moves

A write that shows no progress for a long time is in a phase that moves no
bytes, or waiting on something. Its session log names which:

```bash
ls -t ~/.tapectl/logs/ | head -3          # the newest session first
tail -f ~/.tapectl/logs/<session>.log
```

(On a service-user install the home is the service user's; see
[install.md](install.md).) The last `phase start:` line without a matching
`phase end:` is the phase it is in. A `wait start: <what> (5.0 s so far)`
line with no `wait end:` names what it is blocked on right now: a tape rewind
or space, opening the device, an `sg_read_attr`/`sg_logs`/`sg_inq` run, a busy
catalog, or the operator's answer to the quiet-host question. A `stall:` line
means a byte-counting phase moved nothing for a minute with no named wait:
look at the drive (`sg_logs`, the host's own `dmesg`) and at the staging disk.
`slow: one tape block write took …` lines mean the drive itself held single
blocks for seconds.

Hours in `prewrite-check` are the full read of the staged slices that
`--prewrite-hash` asks for; without the flag that phase is a size check and
takes moments. Hours in `confirm` are the readback of the whole tape, which
every write ends with.

When the command has finished, `volume info <label>` shows the same phases
with their durations and rates (see the operator guide, [Watching a long
operation](operator-guide.md#watching-a-long-operation-progress-and-the-session-log)).

### You pressed Ctrl-C (or the session got SIGTERM or SIGHUP)

```text
error: stopped by a signal: volume "<label>" write interrupted — the tape is left unsealed, and the session's
`writes`/`write_positions` rows are in the `interrupted` state. Reload the same cartridge and run `tapectl volume
resume <label>` to continue from where it stopped.
```

Do exactly that. The session carries on from its frozen staging files, so do
**not** run `staging clean` in between.

Ctrl-C, a SIGTERM (a shutdown) and a SIGHUP (a dropped ssh session) all stop
any long operation the same way: at the next slice, file or tape entry, with an
`error: stopped by a signal: …` line saying where it stopped and what to run
next — `volume resume` after a write or confirm, the same command again after a
stage, verify, read-slices or restore. A second signal stops at once (exit
130); the next command's startup recovers the session as after a crash.

### A real end of tape during the write

If the drive reports that it is out of space, the session ends as a clean
abort:

```text
error: volume "<label>" write aborted: execute failed at position <n>: tape I/O error: write: <OS error, e.g. No space left on device (os error 28)>
```

The same shape covers any other failure while streaming a file, and a slice
whose hash no longer matches staging (`hash mismatch at position <n>: …`).

That hash-mismatch abort is now **how a slice that rotted in staging is
caught**. Since 1.0.3 the write no longer reads every staged slice in full
before the tape moves (it checks each one exists at its recorded size); the
bytes are hashed as they stream to the drive, and a mismatch stops the write
before anything is sealed (ADR-0012, 2026-09-30). To recover, find the unit
the slice belongs to: the `expected` hash in the message is that slice's
recorded hash, and every stage report lists its slices' hashes, so
`grep -l <expected hash> <home>/stage-reports/*` names the unit and version.
Re-stage it, then write the cartridge again from the beginning:

```bash
tapectl staging clean --force --unit <unit> --version <n>
tapectl stage create <unit> --version <n>
tapectl volume write <label> --device "$TAPE"
```

The volume is still `initialized`, so `volume write` starts over from the
beginning of the same tape; re-initialising it first is not needed. Pass
`--prewrite-hash` to `volume write` (or `volume resume`) to have every staged
slice fully hashed before the tape moves — it costs one extra full read of the
batch, and a mismatch is then refused before any tape I/O.

What an abort leaves behind:

- **On tape:** no seal marker. The tape is unsealed. Nothing half-written is
  ever presented as a finished volume.
- **In the catalog:** the write session is `aborted` and the volume is still
  `initialized`. Nothing counts as a copy.
- **`volume resume` will not adopt it.** An aborted session with no recorded
  seal is never resumable:
  ```text
  error: volume "<label>" has no write session `volume resume` can adopt: its session was ABORTED before its seal
  was recorded (`volumes.sealed_at` is empty), so there is no sealed tape to re-confirm, and an unsealed aborted
  session is never resumable. Run `tapectl volume write <label>` to start a new one.
  ```
- **`volume abort` is not needed.** The session is already aborted.
  [`volume abort`](cli/volume.md#tapectl-volume-abort) is for a `planned`
  session, or an interrupted one you know cannot be resumed.

Running into the physical end means the capacity decided at `volume init` was
more than this cartridge really holds. Typical causes are a `capacity_override`
or a registered `--capacity` set too high, or a short or worn tape. A new
`volume write <label>` starts again from the beginning of the same tape with
**every** staged set, so it will run out again unless the batch shrinks.
Before you retry, correct the cause, check the batch with `volume plan`, and
release what is already safe with `staging clean`. Use another cartridge if
this one is suspect.

### Confirm could not complete, or the volume was quarantined at write

After sealing, the write reads the tape back ("confirm"). There are two
failure outcomes:

```text
error: volume "<label>": confirm could not complete — <n> mismatch(es) during readback, none proving the medium
itself is bad (drive/transport evidence only). The tape is physically unharmed and the write is not lost; run
`tapectl volume resume <label>` to retry the confirm readback.
```

Check the drive (clean it, reseat the cartridge), then run `volume resume`. It
re-enters confirm on the tape as written, and never writes or seals again.

```text
error: volume "<label>" quarantined: confirm chain-walk (full read-back) found <n> mismatch(es): position <p>:
<kind> — expected <e>, found <a>; position <p>: ...
```

Here the readback **proved** the bytes on the medium are wrong. The volume is
quarantined and counts as no copy. Treat the cartridge as suspect and write
the data to another one. The next section explains how quarantine is judged
and lifted.

`volume resume` quarantines the same way when the tape it finds is not the
session's: `volume "<label>" quarantined: identity mismatch: expected
label="<label>", uuid="<uuid>"; found label="<other>", uuid="<uuid>"` (or
`found a present but unparseable/corrupt File 0`), or `tape already carries a
seal marker at position <n> (ADR-0003: sealed volumes are immutable)`.

### `volume compact` needs a terminal

[`volume compact`](cli/volume.md#tapectl-volume-compact) runs compaction's three
steps in one flow, through one drive. Between step 1, which reads the source,
and step 2, which writes the destination, someone has to unload the one
cartridge and load the other, so it always pauses there, with or without
`--to`. With no terminal on stdin (cron, a systemd timer, a pipe, a script)
nobody can make the swap, so it refuses before step 1, whatever `--yes`,
`--force` or `--to` say:

```text
error: volume compact refused: it reads "L6-0001" and writes the destination through ONE drive, so someone has to
swap cartridges between the two steps, and stdin is not a terminal — there is nobody to do it or to say it is
done. Nothing was read. Run the three steps separately, which works unattended:
    tapectl volume compact-read L6-0001
    (unload "L6-0001", load the destination)
    tapectl volume compact-write --destination L6-0010
    tapectl volume compact-finish L6-0001
```

Nothing was read and the drive was not touched. Without `--to`, the
destination in the recipe reads `<DEST>`. Run the three steps as it lists them,
adding `--device` when more than one drive is configured. Each step still asks
its own Tier-2 question when there is one (the quiet-host pre-flight of
`compact-write`; `compact-finish` when retiring the source would leave a unit
below its policy), and with no terminal it refuses with the facts
([above](#the-prompt-and-the-non-interactive-refusal)). Pass `--yes` to that
step once you accept them.

An empty `--to` is refused before step 1 too:
`error: --to was given an empty destination label`.

At a terminal, you can stop at the swap prompt, before step 2. Step 1's work
is kept, and the message says how to finish. With `--to`, the prompt asks you
to press Enter once the destination is loaded, and Ctrl-D stops:

```text
error: volume compact stopped before step 2: no answer at the swap prompt — the live slices of "L6-0001" are
staged; to finish, load the destination and run `tapectl volume compact-write --destination L6-0010`, then
`tapectl volume compact-finish L6-0001`
```

Without `--to`, the prompt asks for the destination's volume label. Ctrl-D,
or Enter with no label typed, stops with the destination left as `<DEST>`:

```text
error: no destination label provided — the live slices of "L6-0001" are staged; to finish, load the destination
and run `tapectl volume compact-write --destination <DEST>`, then `tapectl volume compact-finish L6-0001`
```

If you decline step 3's question, the destination is already written and
sealed, and nothing needs redoing. Just before the usual `error: …: aborted,
not confirmed` line, tapectl prints the one command that finishes the job:

```text
Nothing was lost: destination "L6-0010" is written and sealed, and source "L6-0001" is simply not retired yet.
To complete step 3 without re-reading or re-writing anything:
    tapectl volume compact-finish L6-0001 --force
```

---

## Verifying a volume

[`volume verify`](cli/volume.md#tapectl-volume-verify) reads the tape back
without any key, following the chain from the seal marker through the front
index to every content file. `--full` is the default. `--quick` skips hashing
the contents. A clean result looks like this, and exits 0:

```text
verify L8-0001 (full tier): 13 checked, 13 passed, 0 failed
```

Every failure is listed with its kind and a verdict:

```text
    position <n>: <kind> (proves the medium is bad) — expected <e>, found <a>
    position <n>: <kind> (not medium evidence) — expected <e>, found <a>
```

| Kind | Proves the medium is bad? |
|---|---|
| `content_hash_mismatch`, `front_index_diverges_from_seal`, `front_index_inconsistent`, `navigation_disagreement` | yes |
| `content_unreadable`, `front_index_unreadable`, `seal_unreadable` | no: "we could not read it today" is not "the bytes are gone" |

A failed verify ends in one of two outcomes, and the exit code tells them
apart ([Exit codes](#volume-verify-0-2-or-3)).

**The medium is proven bad: the volume is quarantined. Exit 2.**

```text
volume "<label>" QUARANTINED: <n> of <m> failure(s) prove the medium is bad. Its status is untouched (ADR-0012,
2026-09-17) — only its condition changed, from "<old>" to "quarantined" — but it no longer counts as a copy, so
`volume retire` will no longer refuse it as the last one — salvage what still reads off it first (`volume
read-slices --from <label> --unit <UNIT>`).
```

The volume's **condition** becomes `quarantined` (its **status** stays
`sealed`), and it stops counting as a copy. `audit` will now report the
missing copies. A volume that was already quarantined and fails again prints
`volume "<label>" was ALREADY quarantined; this verify confirms it — <n> of
<m> failure(s) prove the medium is bad.` and exits 2 as well. Salvage what
still reads, write it to a new cartridge, then retire the volume:

```bash
tapectl volume read-slices --from L6-0001 --unit family/letters --device "$TAPE"
```

Then load a blank cartridge and write the salvaged slices to it:

```bash
tapectl volume init L6-0009 --device "$TAPE"
tapectl volume write L6-0009 --device "$TAPE"
```

**Only read or transport errors: the volume is left alone. Exit 3.**

```text
volume "<label>" NOT quarantined: no failure here proves the medium is bad — these are read or transport
failures, and "we could not read it today" is not "the bytes are gone". The volume's status is unchanged. Check
the drive (cleaning, block size, cabling, the right tape loaded) and verify again.
```

The catalog is untouched. Clean the drive, check that the right tape is
loaded, and verify again. A verify that stops on an error before any verdict
(an empty drive, the wrong tape, an unknown label) also exits 3 and leaves the
volume alone.

**Coming back.** A later full verify that reads every file cleanly lifts the
quarantine:

```text
volume "<label>" RETURNED TO SERVICE: a full verify read every file back and found no mismatch, so its condition
moves from "<old>" to "ok" and it counts as a copy again (ADR-0012, 2026-09-18). Its status was never touched.
```

This does not happen for a `retired` volume. A clean read changes its
condition, but it still counts for nothing, and the message says so.

---

## Restoring

### `no data for unit … on volume …`

```text
error: no data for unit "<unit>" on volume "<label>"
```

The catalog has no completed write of that unit on that volume. Ask where the
unit actually is:

```bash
tapectl catalog locate family/letters
```

If the volume holds the unit, but not the version you asked for:

```text
error: unit "<unit>" has no version <n> on volume "<label>"; version(s) on it: <list> — pass one of those with --version
```

### Wrong tape loaded

`restore` checks the loaded tape's own File 0, and the cartridge's chip
serial, against the volume named in `--from`:

```text
error: wrong tape: this command names volume "<label>", but the loaded cartridge's File 0 identifies volume
"<other>". Load "<label>"'s cartridge, or re-run naming "<other>". There is no --force for this — it is a fact
tapectl cannot resolve on its own, not a risk to accept.
```

Variants of the same fact: `wrong tape: volume "<label>" is uuid <a> in this
catalog, but the loaded cartridge's File 0 carries uuid <b> under the same
label` (a label reused after a retire), and `wrong cartridge: …` when the chip
serial disagrees. `volume identify` shows what is loaded, and needs no key:

```bash
tapectl volume identify --device "$TAPE"
```

### The destination is not empty

```text
error: refusing to restore into <dest>: it is not empty (it holds "<entry>", and maybe more). dar would keep every
existing file that collides with one from the tape, so the result would not be what is on tape. Restore into an
empty or new directory, or pass --overwrite to replace what collides. Nothing was read from tape.
```

`restore unit` checks `--to` before it opens the drive. Restore into an empty
or new directory, or add `--overwrite` to replace every file that collides
(the files in `--to` that the unit does not have are left alone). For
`restore file` only the one name matters:

```text
error: refusing to restore "<file>": <dest>/<name> already exists. Choose another --to, move that file away, or
pass --overwrite to replace it. Nothing was read from tape.
```

### The restore is INCOMPLETE: the destination was not empty

```text
error: restore into "<dest>" is INCOMPLETE: dar declined to overwrite <n> file(s) that already existed, and would
otherwise have reported success. The stale copies are still in place — the restored data is NOT what is on tape.
Skipped: <paths>. Restore into an empty directory, or remove those files first.
```

When dar meets a file that already exists, it keeps the old one. A
destination that is not empty is refused before the tape is read (above), so
this now means a file appeared in `--to` while the restore ran. Restore into an
empty directory, or pass `--overwrite`.

### `restore file`: the file is not in the catalog

```text
error: unit "<unit>" version <n> has no file "<file>" in the catalog's record of what it archived, so the tape was
not touched. A path is relative to the unit's root and matched exactly; `tapectl catalog search <words of the name>`
finds one, and `tapectl catalog ls <unit>` lists the newest version's files.
```

`--file` is checked against the version's file list before the drive is
opened. Give the path relative to the unit's root, with no leading `/` or
`./`, exactly as `catalog ls` prints it:

```bash
tapectl catalog search letter mum
tapectl catalog ls family/letters
```

A directory is refused the same way (`"<path>" is a directory …`):
`restore file` restores one file. Restore the unit and take the directory
from it.

### Not enough disk space for the restore

```text
error: not enough disk space in <dir> for this restore: it needs about <size>, and <size> is free. Nothing was read
from tape. A restore decrypts every slice of the unit to disk before dar extracts them, so with the scratch space
and --to on one disk it needs the unit's size about twice over, plus one slice. Free space there, choose a larger
disk with --to, or put the decrypted slices on another disk with --scratch DIR. (--no-space-check skips this
check, for a filesystem that holds more than it reports free, such as a compressed or thin-provisioned one.)
```

The decrypted slices wait in a `.tapectl-restore-tmp` directory inside `--to`,
never in the system temp directory. With `--scratch DIR` they wait in `DIR`
instead, and each disk is checked for its own share: the slices plus one more
in `DIR`, the restored files in `--to`. `restore file` needs room for the
unit's slices and the one file. The arithmetic is the same as RESTORE.sh's.

### A scratch directory already exists

```text
error: refusing to restore: the scratch directory <dir>/.tapectl-restore-tmp already exists. A restore that was
killed (or lost power) leaves it behind, and it may hold DECRYPTED archive slices. Remove it
(`rm -rf <dir>/.tapectl-restore-tmp`) and run the restore again. Nothing was read from tape.
```

A restore removes its scratch directory on every way out it controls,
including an error. A `kill -9` or a power cut skips that. The directory can
hold decrypted data, so treat it as you would the restored files, remove it,
and restore again.

### Restoring as a non-root user

When the restore runs as anyone but root, tapectl logs a warning. It is shown
at the default `logging.level`, and printed without colour when stderr is not
a terminal or `NO_COLOR` is set to a non-empty value. From the tour:

```text
2026-09-29T08:20:05.900411Z  WARN tapectl::dar::restore: restoring as a non-root user: restored files will be owned by the invoking user, not their archived owners
```

The contents are exact. Only ownership differs. If the original owners
matter, run the restore as root. The service user of a first-run install is
not root.

### Keys, and restores after a key rotation

Restore tries **every** key file in `<home>/keys/` that belongs to the tenant,
active and rotated-out alike, and, for an ordinary tenant, the operator's keys
as well. Nothing has to be selected. The catalog's key rows decide which files
are the tenant's, so tenant `family` never tries tenant `family-old`'s keys. A
catalog rebuilt from tape has no key rows. There, each key file is tried for
every tenant it could belong to by its name (`family-old-primary` for both
`family` and `family-old`), so a tenant's own key is never left out.
[`key rotate`](cli/key.md#tapectl-key-rotate) deactivates old keys but leaves
their files in place, so data encrypted before a rotation still restores.
Problems start only when the key files themselves are missing:

```text
error: encryption error: no secret keys found for tenant "<tenant>"
error: encryption error: decrypt: <age's reason>
```

The second means none of the keys present opens this slice. Copy the missing
secret key files back into `<home>/keys/`. A `db backup --include-keys` copy
keeps them in the `.keys/` directory beside the backup file. `key import`
cannot help here: it registers a public key only. Otherwise recover with the
operator or escrow key. See [keys-and-recovery.md](keys-and-recovery.md).

### A slice fails its checksum on the way back

```text
error: slice <n> checksum mismatch on tape: expected <sha>..., got <sha>...
```

The ciphertext read off the tape differs from the checksum recorded when it
was staged. Nothing is decrypted from a slice that fails this check. Run a
full `volume verify` on the volume and restore from another copy
(`catalog locate` lists them). Scratch space for decrypted slices is removed
when a restore fails. If tapectl cannot remove it, it warns and names the
path, because that directory may hold decrypted data.

### RESTORE.sh: not enough disk space, or OUT OF DISK SPACE

```text
FATAL: not enough disk space in /restore to restore this unit:
       it needs about 50.0 GiB, and 8.0 GiB is free.
       No slice has been read yet. ...
```

The heir script off the tape (`RESTORE.sh --restore`) decrypts every slice of
the unit to disk before dar extracts them, so with the scratch space and
`--to` on one disk a unit needs about twice its size free, plus one slice. It
measures this after it has picked the version and before it reads any slice,
and prints what it needs either way (`Disk space: needs about …`). Free space,
choose a larger disk with `--to`, or put the decrypted slices on another disk
with `--scratch DIR`. `--no-space-check` skips the check, for a compressed or
thin-provisioned filesystem that holds more than `df` reports.

If the disk fills anyway, the message says `OUT OF DISK SPACE in <dir>` and
names the step (`reading tape file N` or `decrypting slice N`). That is not a
tape or key problem: free space and run the same command again. Tapes written
before tapectl 1.0.0 carry an older RESTORE.sh that put the slices in `/tmp`,
which is RAM on many systems, and stopped with no message or blamed the keys
when it filled. For such a tape, use the RESTORE.sh from a newer tape (the
script reads any layout-v2 tape) or `tapectl restore`.

### RESTORE.sh: `this tape is layout_version N`

```text
FATAL: this tape is layout_version 3; this script reads layout v2 only.
```

A RESTORE.sh reads the layout it was written with. Every tape carries its own
script at file 2, so read that one: `mt -f <dev> rewind && mt -f <dev> fsf 2`,
then `dd if=<dev> bs=512k | tr -d '\0' > RESTORE.sh`. A tape whose File 0
states no layout_version at all (a damaged ID thunk) only warns, and is read
as layout v2.

---

## Audit findings

[`audit`](cli/audit.md#tapectl-audit) compares the catalog with each unit's
resolved policy. It is advisory: it never blocks anything. It exits 0 when
clean, 1 when there are only warnings, and 2 when there is any violation.
This is the tour's audit after the first tape:

```text
VIOLATIONS (4):
  [copy_count] family/letters: has 1 copies, needs 2
  [copy_count] family/photos/2019-italy: has 1 copies, needs 2
  [copy_count] family/photos/2020-garden: has 1 copies, needs 2
  [copy_count] work/invoices-2024: has 1 copies, needs 2
WARNINGS (1):
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 4 violations, 1 warnings (exit 2)
```

`--action-plan` adds a `fix:` line under each finding, built from the current
state of your archive:

```text
  [copy_count] family/letters: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
  ...
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
    fix: tapectl key escrow-kit --out <dir>
```

After the second tape, only the heir-kit warning was left, and `audit` exited 1:

```text
WARNINGS (1):
  [escrow_kit_missing] archive: 2 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 0 violations, 1 warnings (exit 1)
```

`--json` gives the same findings as objects with `severity`, `unit`, `check`,
`message` and `action`.

The per-unit checks cover every unit, whatever its status (`active`,
`tape_only` or `missing`), except `dirty`, which looks only at `active` ones.
The archive-wide checks (the last four rows below) run only for a
whole-archive audit, not with `--unit`.

| Check | Severity | Message | Meaning and fix |
|---|---|---|---|
| `copy_count` | violation | `has <n> copies, needs <m>` | Fewer sealed, in-service copies of the unit's current Version than its policy's `min_copies`. A unit is as covered as its least-covered live Version. Fix: write another volume. The `fix:` line re-stages or `read-slices` first when the staged data is gone. |
| `location_presence` | violation | `no copy at required location(s) <names> (policy requires <list>; copies are in <n> location(s))` | A location the policy names in `required_locations` holds no copy of the unit's current Version (checked by name; a name that is not a registered location counts as missing). Fix: write another copy and move it there. The `fix:` line ends `&& tapectl volume move <OTHER-LABEL> --to <location>`, and says to repeat it, one new copy each, when several locations are missing. |
| `warehouse_copies` | violation | `has <n> warehouse deposit(s), needs <m>` | The policy asks for cold-cloud deposits (`warehouse_copies > 0`). Fix: the deposit procedure in the [operator guide](operator-guide.md#warehouse-copies-cold-cloud), then `volume deposit add`. |
| `encryption` | violation | `<n> unencrypted stage set(s) on tape, policy requires encryption` | Something on tape was written unencrypted (only possible with data from very old versions). Fix: re-stage and rewrite, as the `fix:` line spells out. |
| `policy_unresolvable` | violation | `policy could not be resolved (<why>); copy_count/location_presence/verify_age/encryption checks were SKIPPED for this unit` | The unit's policy chain (dotfile > archive set > `[defaults]`) is broken, so **no** check ran for it. The `fix:` line names the layer at fault: the unit's `.tapectl-unit.toml`, its archive set, or `[defaults]`. |
| `dirty` | warning | `source has drifted since last archive (<a> added, <r> removed, <m> modified)` | The source moved on after its last snapshot. This is routine. Fix: `snapshot create`, `stage create`, `volume init`, then `volume write`, as the `fix:` line spells out. |
| `dirty` | violation | `dirty scan could not run (<error>)` | tapectl could not even read the source to compare it (permissions, a missing path). Fix the access or the unit's dotfile. |
| `no_archive` | warning | `no current snapshot or tape copies` | Never archived. Fix: snapshot, stage, init and write. |
| `verify_age` | warning | `not verified within <d> days (last: <date or never>)` | Only when the policy sets a verify interval. Fix: `volume verify <LABEL>` on a volume holding it. |
| `escrow_coverage` | warning | `volume <label> (stage set <id>): <reason> — the current escrow key cannot recover it` | That copy cannot be opened by the escrow key. `coverage unknown …` means a catalog rebuilt from a tape with no recipient list; `catalog rebuild --key <escrow key>` attests it. Otherwise re-stage and rewrite, or accept that only the original recipients can open it. |
| `compaction_candidate` | warning | `live data is <n>% of the archive data on this volume (<live> live, <r> in reclaimable/purged snapshots), below the <t>% compaction threshold` | Part of the volume's data belongs to snapshots marked reclaimable or purged, and the live share has fallen below `[compaction] utilization_threshold`. Only data slices count, never the fixed per-volume metadata, and a volume with nothing reclaimable is never a candidate. Fix: [compaction](operator-guide.md#compaction), starting with `volume compact-read <label>`. |
| `escrow_kit_missing` | warning | `<n> sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them` | Fix: `tapectl key escrow-kit --out <dir>`, then do the paper steps it lists ([keys-and-recovery.md](keys-and-recovery.md)). |
| `escrow_kit_stale` | warning | `` <n> volume(s) were sealed after the last heir kit (<date>): the kit's escrow secret still opens them (it is a recipient of every tape), but the kit's encrypted catalog (catalog.db.age) does not list them — regenerate the kit, or rebuild the catalog from those tapes with `tapectl catalog rebuild --from-volume` `` | Only the kit's catalog is behind; its secret opens every tape. Fix: `tapectl key escrow-kit --out <dir>` and replace the stored copies. The escrow secret itself does not change. |
| `escrow_identity_mismatch` | warning | `<n> stage set(s) the current escrow key cannot open all name a recipient this catalog does not recognise: <key> — …` | Typical after a disaster rebuild that created a **new** escrow identity. No command replaces a registered escrow identity. The fix is to re-initialise a fresh home with `tapectl init --escrow-public-key <original>` (from the heir kit) and run `catalog rebuild` again. |

---

## `report summary` counts stage sets held in staging

In the tour, after both copies were written, `report summary` said:

```text
tapectl summary
  Tenants:    2 (the operator not counted)
  Units:      4 active
  Snapshots:  4
  Volumes:    2 holding data (retired, erased and quarantined not counted)
  Writes:     8 completed
  Total data: 7.0 MiB on tape
  Staging:    4 stage set(s) held, none owe a copy of their own version (`tapectl staging clean` decides which can be released)
```

A stage set stays in staging after its writes on purpose, so it can become the
next copy, and every set still held there rides along on the next
`volume write`. The `Staging` line counts those sets, written or not, and says
how many still owe a copy: their version has fewer copies than its unit's
`min_copies`. When some do, the line reads ``<n> stage set(s) held, <m> still
owe a copy (`tapectl report pending` lists them)``. `report pending` lists only
those:

```text
stage sets that still owe a copy:
  <unit> v<n>: <s> slices, <size> — <c> of <m> copies
```

When none owe a copy, it prints `no stage set owes a copy`. A last line counts
the sets that are only held in staging.

The line goes away once `staging clean` has released every held set. To see
how often each set has actually been written, read the `Writes` column:

```bash
tapectl staging status
```

The tour's two writes were both done, and `staging clean` then released all
four sets.

A script that read `staged_pending` from `report summary --json` finds it gone:
`stage_sets_held` and `stage_sets_owing_a_copy` replace it, and
`report pending --json` now lists only the sets that owe a copy. The operator
guide lists every `--json` field that changed, under
[Scripting against tapectl](operator-guide.md#scripting-against-tapectl).

---

## first-run.sh stopped

[`scripts/first-run.sh`](install.md) stops at the first thing it cannot
settle, usually with a `✗` line saying why. Only some of those lines name
where to re-enter (`--from N`); most do not. The step that stopped is the last
`== Step N — … ==` header above the stop.

To re-enter, run the script again with **every option you first gave it**
(`--profile`, `--device`, `--home`, `--no-service-user`, `--user`,
`--tapectl`, …), plus `--from N`. Options are not remembered from one run to
the next. Without `--profile` or `--device`, the drive is lost. Step 6 is the
only step that asks for it, so a run that starts after step 6 stops at step 8,
12 or 13 (whichever it reaches first) with `no device chosen — pass --device,
or run with --from 6`. Without `--home` or `--no-service-user`, the run works
on a different home. The first lines of every run print the `home:` and
`service user:` it will use; check them before you answer anything. See
[install.md §5](install.md#5-resuming).

**Stops you can still meet.**

- **Step 0, the config uses renamed keys.** A home initialised by an older
  version carries `min_copies_for_tape_only` and `min_locations_for_tape_only`
  in `[defaults]`, and every tapectl command refuses that config
  ([above](#config-a-key-an-older-version-wrote)). Step 0 notices and offers
  to rename them in place (`Rename them in place now (the old file is kept
  beside it)?`), keeping the old file as `config.toml.pre-rename-<timestamp>`.
  If you decline, it says `left as is — every tapectl command will refuse this
  config until the two keys are renamed`, and a later step stops on that
  refusal. Re-run and accept, or rename the two lines by hand.
- **Step 13, `stage failed for <unit>`.** When the staging directory may be too
  small for a unit ([Staging space](#staging-space-refused-or-asked-about)),
  `stage create` asks before it goes ahead. Step 13 runs it with stdin taken
  from its list of units, not from your terminal, so the question cannot be
  asked and the stage is refused. The figures are in the output above the
  stop. If you accept them, stage that unit yourself, then re-enter with the
  options of your first run. The `tapectl-op` wrapper is installed only at
  step 14, so on a first run it does not exist yet; run tapectl as the service
  user directly:
  ```bash
  sudo -u tapectl -H tapectl --yes stage create <unit>
  scripts/first-run.sh --profile contrib/hosts/<host>.profile --from 13   # if the first run used a profile
  scripts/first-run.sh --device "$TAPE" --from 13                          # if it named the drive instead
  ```
  Run one of the two `first-run.sh` lines, adding any other option the first
  run had. If first-run was given `--home <dir>`, pass the same
  `--home <dir>` to tapectl and to the script. Under `--no-service-user`, drop
  the `sudo -u tapectl -H` and pass `--no-service-user` to the script again.
  With `--user <name>`, put that name after `-u`.
  Step 13 skips a unit that is already staged. If the unit is proven too big
  for the staging directory, no flag helps: free space there, or point
  `[staging] directory` at a larger filesystem.

**Stops that have been fixed.** If you hit one of these, update to the current
checkout and re-run from that step:

- **Step 4, the test suite seemed to wait for input.** It happened when the
  suite was run from a terminal: tests that exercise a consent prompt reached a
  real one. Fixed. A test build never treats stdin as a terminal, and step 4
  runs the suite with stdin from `/dev/null`.
- **Step 6, `✗  does not exist` with no name shown.** The sg-node prompt
  stored an arrow key's escape sequence as the answer. Fixed. Prompts now use
  line editing and strip control bytes, and a node that does not exist is
  shown with its real spelling and asked for again
  (`<node> does not exist — press Enter for /dev/sgN, or type the node`).
- **Step 7, `could not generate a fresh config; the original is untouched`.**
  When step 7 offers to replace a config.toml tapectl cannot load, it builds
  the new one with a throwaway `init` run as the service user. `init` now
  refuses to run without `--operator` under a system account
  ([above](#init---operator-is-required-when-run-as-a-system-account)), and
  the throwaway run did not pass one. Fixed: it passes
  `--operator config-template`.
- **Step 12, `required binary missing: mtx`.** The rehearsal runs
  `scripts/lifecycle-suite.sh`, which checks for `mtx` even on a real drive.
  Debian installs `mtx` in `/usr/sbin`, which is not on a normal user's
  `PATH`. Fixed: step 12 adds `/usr/sbin:/sbin`.
- **A check that failed although the thing it checked was fine.** Checks of
  the form `cmd | grep -q X` fail under `pipefail` when `grep` exits early:
  `cmd` dies of SIGPIPE, and the pipeline reports failure. That is how a
  correct dar 2.7.21 was reported as "not 2.7.21". Fixed. Such checks now read
  their whole input.

For anything else, the step that stopped says why, and
[install.md](install.md) describes what each step does and how to re-enter.

---

## Related pages

- [README](../README.md) and the [documentation index](README.md)
- [Concepts](concepts.md): Copy, Version, Escrow Recipient, Receipt, Quarantine
- [Configuration](configuration.md): every `config.toml` key, including
  `[host_check]` and `[[backends.lto]]`
- [Keys and recovery](keys-and-recovery.md): escrow, the heir kit, rebuilding
  the catalog
- [Operator guide](operator-guide.md): day-to-day operation and the quiet-host
  rule
- [Install](install.md): the first-run runbook
- [Walkthrough](walkthrough.md): a complete first archive, with real output
- [Command reference](cli/README.md)
