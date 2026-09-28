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
  `<device>`, `<unit>` and so on. tapectl prints each message on one line; the
  long ones are wrapped here so they are easier to read.
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
- [`report summary` says "Pending: N stage set(s) awaiting write"](#report-summary-says-pending-n-stage-sets-awaiting-write)
- [first-run.sh stopped](#first-runsh-stopped)
- [Related pages](#related-pages)

---

## Exit codes

Any command that fails prints `error: <message>` on stderr and exits **2**.
When one failure caused another, the message is the whole chain, joined by
`: `. For example, a bad config file prints:

```text
error: failed to load config: configuration error: <path>/config.toml: TOML parse error at line 68, column 1
```

A mistyped command or flag (clap's usage error) also exits 2.

A few commands finish their work and then report a verdict through the exit code:

| Command | 0 | 1 | 2 |
|---|---|---|---|
| [`audit`](cli/audit.md#tapectl-audit) | clean | warnings only | at least one violation (or an error) |
| [`volume verify`](cli/volume.md#tapectl-volume-verify) | every check passed | never used | any check failed, whether or not the volume was quarantined (or an error) |
| [`host check`](cli/host.md#tapectl-host-check) | host is quiet | something tripped | an error |
| [`config check`](cli/config.md#tapectl-config-check) | config valid | never used | config invalid (or an error) |
| [`db fsck`](cli/db.md#tapectl-db-fsck) | clean | problems found (repaired or not) | database integrity is broken (or an error) |
| [`collection sync/status/plan/run`](cli/collection.md#tapectl-collection) | clean | a unit's dotfile was refused while the rest ran | an error |

Every other command exits 0 on success and 2 on error.

`volume verify` uses 2 for **both** outcomes described in
[Verifying a volume](#verifying-a-volume). A script that has to tell a bad
medium from a transport error should read `volume verify --json` (its
`quarantine` field), not the exit code.

---

## How consent works

Before anything destructive, tapectl asks for consent according to
[ADR-0008](adr/0008-destructive-consent-tiers.md). What is at risk decides
which of three tiers applies:

| Tier | What is at risk | What tapectl does | Can a flag override it? |
|---|---|---|---|
| 1: staleness | nothing new; the evidence is old ("last verified 15y ago") | displays it | nothing to override |
| 2: degraded but non-zero | a unit left below its `min_copies` or `min_locations`, a dirty unit, a host that is not quiet | shows the facts and asks | yes: `--yes` (on some commands `--force` too) |
| 3: zero coverage or incoherence | writing over a sealed cartridge, marking a never-archived unit tape-only, retiring a unit's last copy, a drive that physically cannot write the medium | refuses | **no**, and no prompt appears either |

A Tier-3 refusal reports a fact, not a risk. It tells you what has to change
(load another cartridge, erase the tape, take a snapshot), and none of `--yes`,
`--force` or answering `y` gets past it.

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

Read the fact lines. If you accept them, re-run with `--yes`.

### Which flag answers which question

`--force` on `volume init` and `volume write` has nothing to do with the tiers.
There it means "overwrite a cartridge whose File 0 names a different volume or
is empty" ([below](#file-0-identifies-a-different-volume-or-is-empty)). It never
answers a prompt.

| Command | Tier-2 prompt answered by |
|---|---|
| `volume write` (quiet-host pre-flight) | `--yes` |
| `volume retire`, `volume abort`, `volume compact-finish` | `--yes` |
| `cartridge edit --serial`, `db import` | `--yes` |
| `cartridge retire`, `cartridge mark-erased` | `--yes` or `--force` |

`unit mark-tape-only` is the exception. It never prompts. Below its policy's
copies or locations, it refuses outright, and only `--force` gets past; `--yes`
has no effect:

```text
error: insufficient copies: <n> < <m> required (use --force to override)
error: insufficient locations: <n> < <m> required (use --force to override)
```

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

Several keys have been removed or moved. For these, tapectl replaces the
generic "unknown field" error with a specific explanation. Delete the line,
or rename it where the message says so:

| Key in your config | What the message tells you |
|---|---|
| `[[backends.lto]]` `media_type`, `nominal_capacity` | moved: declare the **drive's** generation as `generation = "LTO-6"`; capacity now follows the cartridge's detected generation ([ADR-0010](adr/0010-media-generation-is-a-cartridge-property.md)); `capacity_override` is for virtual drives only |
| `[[backends.lto]]` `block_size` | removed: the block size is a format constant (512 KiB) |
| `[[backends.lto]]` `hardware_compression` | removed: drive compression is always switched off |
| `[packing]` (`min_free_for_append`, `strategy`, `fill_threshold`) | removed: there is no append, and the batch selector is not configurable |
| `[labels]` `format` | removed: volume labels are always given by you |
| `[defaults]` `min_copies` | not a setting: the general requirement is `min_copies_for_tape_only`; a per-set override goes on an `[[archive_sets]]` entry as `min_copies` |
| `[defaults]` `hash` | removed: every checksum is sha256; `checksum_mode` is the real knob |

For example:

```text
error: failed to load config: configuration error: <path>/config.toml: backends.lto["a"]: "media_type" and
"nominal_capacity" moved — declare the DRIVE's generation as generation = "LTO-6"; capacity now follows the
cartridge's generation (ADR-0010); capacity_override is for virtual drives only
```

### No drive configured

```text
error: configuration error: no LTO backend configured — tapectl does not know what tape drive to use.

Add a [[backends.lto]] section to your tapectl config (by default ~/.tapectl/config.toml):
...
`tapectl init` leaves a commented-out example there to uncomment.
```

Every write path (`volume init`, `volume write`, `collection run`) needs a
`[[backends.lto]]` entry. Uncomment the example `init` left in config.toml, or
add one with [`tapectl backend add`](cli/backend.md#tapectl-backend-add).

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
| `No medium found (os error 123)` or `Input/output error (os error 5)` | no cartridge, or the drive never became ready | load a cartridge and wait for the drive to settle |
| `Read-only file system (os error 30)` | the cartridge's write-protect tab is set (on a write path) | slide the tab, or use another cartridge |

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

---

## Staging

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

This check runs **after** dar has built the archive, just before encryption,
so on a large unit you will already have waited for dar. tapectl removes the
unencrypted slices from the failed attempt. Give the tenant a key and stage
again:

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

### The staging directory is not writable, or fills up

tapectl does **not** refuse in advance for lack of space. Before running dar,
it compares free space in `[staging] directory` with three times the unit's
size. If space is short it logs a warning (shown at the default
`logging.level = "warn"`) and carries on:

```text
<timestamp>  WARN tapectl::staging: staging space may be insufficient available_gb=<n> needed_gb=<n>
```

If the disk really fills, the failure comes from dar (`dar error: dar -c
failed …`) or from the write that ran out of room. The half-built stage set's
files are removed, and the row is marked `failed` the next time tapectl opens
the database.

A staging directory tapectl cannot create or write to fails with the bare OS
error, which names no path:

```text
error: Permission denied (os error 13): Permission denied (os error 13)
```

When `stage create` prints only that, suspect the staging directory first.
`tapectl config check` names the directory and says whether it exists and is
writable:

```text
staging: '<directory>' exists and is writable
```

Fix the ownership (the service user must own it on a first-run install), or
point `[staging] directory` at a disk with room. Staging must not live on a
small root partition.

### The source changed after the snapshot

`stage create` re-reads every file and checks it against the snapshot before
archiving:

```text
error: DIRTY: source file size changed: <path> (expected <n> bytes, found <n> bytes) — a real edit ...
error: source file missing: <path>
error: BITROT suspected: <path> — sha256 differs at an unchanged size (<n> bytes): baseline=<sha>, current=<sha>.
Refusing to stage (see #32); investigate before re-staging.
```

For a real edit or a deleted file, take a new snapshot and stage that:

```bash
tapectl snapshot create <unit>
tapectl stage create <unit>
```

`BITROT suspected` is different. The file has the same size as before but
different contents, which is what silent disk corruption looks like. Before
you archive the new contents, compare the file against a copy you trust.

---

## Initialising a volume

[`volume init`](cli/volume.md#tapectl-volume-init) is tapectl's first contact
with a cartridge. It reads the chip, decides the generation and capacity, and
writes File 0.

### No cartridge loaded

```text
error: no cartridge loaded in <device>
```

Only `volume init` checks for an empty drive, and it answers within a second.
Other commands (`volume write`, `verify`, `identify`, `restore`) do not make
this check. On an empty drive, the kernel's open waits for the drive to become
ready (about two minutes on the virtual library) and then fails with a
[`tape I/O error: open …`](#opening-the-drive-fails-tape-io-error-open-device-). If a
command seems to hang at the start, check the drive before anything else:

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
         *** unit "<unit>" [<version>] now has ZERO copies ***
```

### The cartridge already carries a SEALED volume

```text
error: refusing to write volume "<label>": the loaded cartridge already carries a SEALED volume — a valid seal
marker parses at tape position <n>. ADR-0003: sealed volumes are immutable, there is no append, and --force
cannot override this. If this cartridge should be reused: retire its current volume, bulk-erase the physical
tape, then run `tapectl cartridge mark-erased` before writing to it again.
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
2. Erase the tape. A bulk eraser works, and so does the drive's long erase
   (`mt -f "$TAPE" erase`, which takes hours on real LTO).
3. Record that its bytes are gone, then initialise it again:
   ```bash
   tapectl cartridge mark-erased <barcode>
   tapectl volume init <new-label> --device "$TAPE"
   ```

If instead you only wrote a filemark at the start of the tape (`mt weof 1`),
the seal is no longer readable. `volume init` then reports File 0 as EMPTY or
unparseable, and wants `--force` (see the previous entry).

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
is wrong — edit config.toml (`tapectl config show` prints it; there is no `backend edit`, by decision: ADR-0012,
#143) and run `tapectl config check`.
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
checks against the catalog, then the quiet-host pre-flight, then the checks
that need the tape, and only then writes. A refusal at any stage leaves the
tape untouched.

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
  family/letters v1: 1 slices, 1.7 KiB
  ...
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
load average       3.78 over 16 CPUs = 0.24/CPU             max 1.00/CPU
available memory   7709 MiB                                 min 2048 MiB
memory pressure    0.01% full avg60                         max 10.00%
I/O pressure       0.23% full avg60                         max 10.00%
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

- `has a planned write session, not an interrupted one: … nothing was ever
  written to tape`: clear it with `tapectl volume abort <label>`, then run
  `volume write` again.
- `has an in_progress write session … ANOTHER PROCESS IS WRITING THIS TAPE
  RIGHT NOW`: tapectl turns crashed sessions into `interrupted` whenever it
  opens the database, so a row still `in_progress` means a writer is live.
  Find it and do not start a second one.
- `has no write sessions at all` / `its write sessions are all resolved`: use
  `volume write`.

### You pressed Ctrl-C

```text
error: volume "<label>" write interrupted (SIGINT) — the tape is left unsealed, and the session's
`writes`/`write_positions` rows are in the `interrupted` state. Reload the same cartridge and run `tapectl volume
resume <label>` to continue from where it stopped.
```

Do exactly that. The session carries on from its frozen staging files, so do
**not** run `staging clean` in between.

### A real end of tape during the write

If the drive reports that it is out of space, the session ends as a clean
abort:

```text
error: volume "<label>" write aborted: execute failed at position <n>: tape I/O error: write: <OS error, e.g. No space left on device (os error 28)>
```

The same shape covers any other failure while streaming a file, and a slice
whose hash no longer matches staging (`hash mismatch at position <n>: …`).
What that leaves behind:

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
error: volume "<label>" quarantined: confirm chain-walk found <n> mismatch(es) at tier <tier>: [...]
```

Here the readback **proved** the bytes on the medium are wrong. The volume is
quarantined and counts as no copy. Treat the cartridge as suspect and write
the data to another one. The next section explains how quarantine is judged
and lifted.

---

## Verifying a volume

[`volume verify`](cli/volume.md#tapectl-volume-verify) reads the tape back
without any key, following the chain from the seal marker through the front
index to every content file. `--full` is the default. `--quick` skips hashing
the contents. A clean result looks like this:

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

This leads to one of two outcomes. Both exit 2.

**The medium is proven bad: the volume is quarantined.**

```text
volume "<label>" QUARANTINED: <n> of <m> failure(s) prove the medium is bad. Its status is untouched (ADR-0012,
2026-09-17) — only its condition changed, from "<old>" to "quarantined" — but it no longer counts as a copy, so
`volume retire` will no longer refuse it as the last one — salvage what still reads off it first (`volume
read-slices --from <label> --unit <UNIT>`).
```

The volume's **condition** becomes `quarantined` (its **status** stays
`sealed`), and it stops counting as a copy. `audit` will now report the
missing copies. Salvage what still reads, write it to a new cartridge, then
retire the volume:

```bash
tapectl volume read-slices --from L6-0001 --unit family/letters --device "$TAPE"
```

Then load a blank cartridge and write the salvaged slices to it:

```bash
tapectl volume init L6-0009 --device "$TAPE"
tapectl volume write L6-0009 --device "$TAPE"
```

**Only read or transport errors: the volume is left alone.**

```text
volume "<label>" NOT quarantined: no failure here proves the medium is bad — these are read or transport
failures, and "we could not read it today" is not "the bytes are gone". The volume's status is unchanged. Check
the drive (cleaning, block size, cabling, the right tape loaded) and verify again.
```

The catalog is untouched. Clean the drive, check that the right tape is
loaded, and verify again.

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

### The restore is INCOMPLETE: the destination was not empty

```text
error: restore into "<dest>" is INCOMPLETE: dar declined to overwrite <n> file(s) that already existed, and would
otherwise have reported success. The stale copies are still in place — the restored data is NOT what is on tape.
Skipped: <paths>. Restore into an empty directory, or remove those files first.
```

When dar meets a file that already exists, it keeps the old one. Restore into
an empty directory.

### Restoring as a non-root user

When the restore runs as anyone but root, tapectl logs a warning (shown at
the default `logging.level`, with the usual timestamp and `WARN` prefix):

```text
restoring as a non-root user: restored files will be owned by the invoking user, not their archived owners
```

The contents are exact. Only ownership differs. If the original owners
matter, run the restore as root. The service user of a first-run install is
not root.

### Keys, and restores after a key rotation

Restore tries **every** key file the tenant has in `<home>/keys/`, active and
rotated-out alike, and, for an ordinary tenant, the operator's keys as well.
Nothing has to be selected. [`key rotate`](cli/key.md#tapectl-key-rotate)
deactivates old keys but leaves their files in place, so data encrypted before
a rotation still restores. Problems start only when the key files themselves
are missing:

```text
error: encryption error: no secret keys found for tenant "<tenant>"
error: encryption error: decrypt: <age's reason>
```

The second means none of the keys present opens this slice. Bring back the
missing key files with [`key import`](cli/key.md#tapectl-key-import), or
recover with the operator or escrow key. See
[keys-and-recovery.md](keys-and-recovery.md).

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
WARNINGS (2):
  [compaction_candidate] volume:L8-0001: utilization 49% < 50% threshold
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
audit: 4 violations, 2 warnings (exit 2)
```

`--action-plan` adds a `fix:` line under each finding, built from the current
state of your archive:

```text
  [copy_count] family/letters: has 1 copies, needs 2
    fix: tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>
  ...
  [compaction_candidate] volume:L8-0001: utilization 49% < 50% threshold
    fix: tapectl volume compact-read L8-0001
  [escrow_kit_missing] archive: 1 sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them
    fix: tapectl key escrow-kit --out <dir>
```

`--json` gives the same findings as objects with `severity`, `unit`, `check`,
`message` and `action`.

The per-unit checks cover units whose status is `active`, `tape_only` or
`missing`. Retired units are never audited, and `dirty` looks only at
`active` ones. The archive-wide checks (the last four rows below) run only for
a whole-archive audit, not with `--unit`.

| Check | Severity | Message | Meaning and fix |
|---|---|---|---|
| `copy_count` | violation | `has <n> copies, needs <m>` | Fewer sealed, in-service copies of the unit's current Version than its policy's `min_copies`. A unit is as covered as its least-covered live Version. Fix: write another volume. The `fix:` line re-stages or `read-slices` first when the staged data is gone. |
| `location_presence` | violation | `in <n> locations, needs <m> ([...])` | Copies are not spread over the locations the policy requires (only when `required_locations` is set). Fix: write a copy and `volume move` it to the missing location. |
| `warehouse_copies` | violation | `has <n> warehouse deposit(s), needs <m>` | The policy asks for cold-cloud deposits (`warehouse_copies > 0`). Fix: the deposit procedure in the [operator guide](operator-guide.md#warehouse-copies-cold-cloud), then `volume deposit add`. |
| `encryption` | violation | `<n> unencrypted stage set(s) on tape, policy requires encryption` | Something on tape was written unencrypted (only possible with data from very old versions). Fix: re-stage and rewrite, as the `fix:` line spells out. |
| `policy_unresolvable` | violation | `policy could not be resolved (<why>); copy_count/location_presence/verify_age/encryption checks were SKIPPED for this unit` | The unit's policy chain (dotfile > archive set > `[defaults]`) is broken, so **no** check ran for it. The `fix:` line names the layer at fault: the unit's `.tapectl-unit.toml`, its archive set, or `[defaults]`. |
| `dirty` | warning | `source has drifted since last archive (<a> added, <r> removed, <m> modified)` | The source moved on after its last snapshot. This is routine. Fix: `snapshot create`, `stage create`, then write it. |
| `dirty` | violation | `dirty scan could not run (<error>)` | tapectl could not even read the source to compare it (permissions, a missing path). Fix the access or the unit's dotfile. |
| `no_archive` | warning | `no current snapshot or tape copies` | Never archived. Fix: snapshot, stage, init and write. |
| `verify_age` | warning | `not verified within <d> days (last: <date or never>)` | Only when the policy sets a verify interval. Fix: `volume verify <LABEL>` on a volume holding it. |
| `escrow_coverage` | warning | `volume <label> (stage set <id>): <reason> — the current escrow key cannot recover it` | That copy cannot be opened by the escrow key. `coverage unknown …` means a catalog rebuilt from a tape with no recipient list; `catalog rebuild --key <escrow key>` attests it. Otherwise re-stage and rewrite, or accept that only the original recipients can open it. |
| `compaction_candidate` | warning | `volume:<label>: utilization <n>% < <t>% threshold` | Live (not reclaimable) slice bytes divided by all bytes written to the volume fell below `[compaction] utilization_threshold`. Fix: [compaction](operator-guide.md#compaction), starting with `volume compact-read <label>`. See the note below. |
| `escrow_kit_missing` | warning | `<n> sealed volume(s) exist but no heir kit has ever been generated — nothing off-site can decrypt them` | Fix: `tapectl key escrow-kit --out <dir>`, then do the paper steps it lists ([keys-and-recovery.md](keys-and-recovery.md)). |
| `escrow_kit_stale` | warning | `<n> volume(s) were sealed after the last heir kit (<date>); the printed kit still opens older tapes and silently misses these` | Fix: generate a new kit and replace the stored copies. |
| `escrow_identity_mismatch` | warning | `<n> stage set(s) the current escrow key cannot open all name a recipient this catalog does not recognise: <key> — …` | Typical after a disaster rebuild that created a **new** escrow identity. No command replaces a registered escrow identity. The fix is to re-initialise a fresh home with `tapectl init --escrow-public-key <original>` (from the heir kit) and run `catalog rebuild` again. |

> [!NOTE]
> **`compaction_candidate` on a new, small volume is harmless.** The divisor
> counts every file on the tape, including the fixed metadata (ID thunk,
> RESTORE.sh, envelopes, the encrypted catalog), while the numerator counts
> only data slices. On the tour's 1.7 MiB of data, that metadata made up half
> the tape (3.5 MiB written), so a volume with nothing reclaimable read as 49%.
> On a full-size tape the metadata is negligible. Compaction frees space only
> when snapshots on the volume have been marked reclaimable
> (`snapshot mark-reclaimable`). If none have, there is nothing to reclaim.

---

## `report summary` says "Pending: N stage set(s) awaiting write"

In the tour, after both copies were written, `report summary` still said:

```text
tapectl summary
  Tenants:    3
  Units:      4 active
  Snapshots:  4
  Volumes:    2 active
  Writes:     8 completed
  Total data: 7.0 MiB on tape
  Pending:    4 stage set(s) awaiting write
```

That line counts stage sets whose status is `staged`, meaning every set whose
files are **still in staging**, written or not. A set stays `staged` after
its writes on purpose, so it can become the next copy. It stops being counted
only when `staging clean` releases it. After `staging clean`, the line goes
away. So "awaiting write" really means "still held in staging, and will ride
along on the next `volume write`". `report pending` uses the same selection.
To see how often each set has actually been written, read the `Writes`
column:

```bash
tapectl staging status
```

The tour's two writes were both done, and `staging clean` then released all
four sets. (`Tenants` counts the operator as well as the two tenants you
created.)

---

## first-run.sh stopped

[`scripts/first-run.sh`](install.md) stops at the first thing it cannot
settle, and its last line names where to re-enter (`--from N`; see
[install.md §5](install.md#5-resuming)). Four stops seen on real hosts have
been fixed in the current script. If you hit one, update to the current
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
