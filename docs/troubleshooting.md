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
- With one drive configured, tapectl finds it, so the tapectl examples leave
  `--device` out ([when it is needed](operator-guide.md#configure)). Where a
  command needs the drive named (`mt`, `fuser`, a host with several drives),
  the example uses its stable by-id path, `$TAPE`. `/dev/nstN` numbers can
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
- [What staging holds: `status` and `stage list`](#what-staging-holds-status-and-stage-list)
- [first-run.sh stopped](#first-runsh-stopped)
- [Related pages](#related-pages)

---

## Exit codes

Every code tapectl exits with, and the commands that use each, is in one table,
generated from the binary itself: the command reference's
[exit codes](cli/README.md#exit-codes). Each command's own `--help` names the codes
it uses. This section does not repeat that table. It says what to do after the
codes that need more than "read the error and run the command again": a busy
catalog, the write family, `audit`, `volume verify` and `cartridge erase`.

A command that fails prints `error: <message>` on stderr. When one failure
caused another, the message is the whole chain, joined by `: `. For example, a
bad config file prints:

```text
error: failed to load config: configuration error: <path>/config.toml: TOML parse error at line 68, column 1
```

Under `--json`, every error is also one JSON document on stdout, with the
message and the exit code, and the `error:` line still goes to stderr, so a
script that reads either keeps working (1.9.0):

```text
$ tapectl --json volume info NOPE
{
  "error": "volume not found: NOPE",
  "exit_code": 2
}
error: volume not found: NOPE
```

`status --json` is the one exception: when it cannot read the
catalog, its own document carries `error` and `exit_code` beside the home and the
running sessions, and nothing goes to stderr.

A mistyped command or flag (clap's usage error) also exits 2, with two
exceptions: when the parse has already reached `volume verify`, an error in
what follows exits 3, and when it has reached `audit`, 70. That covers a
missing label, an unknown flag after `verify` or `audit`, `--full` together
with `--quick`, and a flag with no value. An error that comes before `verify`
is an ordinary usage error and still exits 2: a mistyped subcommand such as
`volume verfy` or `audt`, or an unknown flag in front of `verify`
(`tapectl --hme <dir> volume verify <label>`,
`tapectl volume --bogus verify <label>`). `--help` exits 0 everywhere. A
spelling 1.9.0 renamed exits 2 too ([below](#tapectl-old-was-renamed-in-190-use-tapectl-new)).

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
command marks the set `failed`, and `stage clean` reclaims it; stage the
unit again. Opening the catalog takes the write lock only to recover a
crashed session, so read-only commands such as `report`, `catalog` and `db
backup` run while another command writes.

`volume retire`, `cartridge retire`, `cartridge mark-erased`, `volume
compact-finish` and `volume move` also exit **75** when a live `volume init`,
`write`, `resume` or `verify` holds the volume's session lock (#461). They wait
for nothing and change nothing; the message is `error: volume "L" has a live
session (held by pid N: ...): refusing to ...`. Run the command again once that
one has finished. A `--dry-run` is not refused.

`volume verify` keeps its own contract: a busy catalog is one more way to
reach no verdict, so it exits 3. A write that the busy catalog stops after its
session began exits by the write table instead (4, or 3 during the confirm).
The `contrib/` timer wrappers log 75 as "catalog busy" and do not ping
`/fail`.

### The write family: 0, 2, 3, 4, 5, 6 or 75

`volume write`, `volume resume`, `collection run`, `quick-archive`, `volume
compact-write` and `volume compact` share one exit table (ADR-0012, the
2026-10-07 amendment, item 20). The code says what the tape and the session
are left as, so a script knows what to do next without parsing the message:

| Exit | Meaning | What to do |
|---|---|---|
| 0 | **sealed and confirmed** | nothing (`collection run` exits 1 instead when it also refused a unit, [above](#exit-codes)) |
| 2 | an error, **the medium untouched**: refused before anything was written, or a usage error. That covers an unknown label or one that is not a write target, the wrong cartridge (the File 0 check), an empty drive, a drive that cannot write the medium, a layout over the fill ceiling, the quiet-host pre-flight, `--dry-run`, a resume whose revalidation failed or whose File 0 would not read, a stage that failed before the write (`collection run`, `quick-archive`), and a signal before the tape moved (`--prewrite-hash`'s full hash, a stage). A session that existed before the command is left as it was. With `volume write --init`, a refusal only the tape contact can raise (the capacity gate, the quiet-host question) exits 2 although the cartridge was initialised: stderr says the volume stays initialized, and `--json` carries `"initialized": true` and `"volume_status": "initialized"`; run `tapectl volume write L` without `--init` and with the write's other flags (`--full-confirm`, `--move-to X`, …), as stderr names it. A refusal the catalog alone can raise (nothing staged, a staged slice missing, a tenant without a key, no escrow) comes before the cartridge is touched and leaves no volume (#445) | read the `error:` line, fix it, run the command again |
| 3 | **confirm inconclusive**: the seal is on the tape and recorded, and the readback after it reached no verdict (a read or transport failure, as `volume verify`'s 3) | check the drive, reload the same cartridge, `tapectl volume resume <label>` re-enters the confirm. The message names a different next step in three cases (#531, #544): after a write the drive refused under Logical Block Protection, a tape file that could not be read back may be torn, and if it fails again the tape is retired and its units written to another cartridge; a session aborted under the confirm names `volume verify` then `volume resume`; a volume retired under it names no resume |
| 4 | **interrupted**: a signal, or a drive, staging-disk or catalog error, stopped the session part-way (writing, sealing, or a full confirm stopped by a signal). The tape is left as the session left it | fix the cause, reload the same cartridge, `tapectl volume resume <label>`: from 1.9.0 it keeps what the write asked for (`--full-confirm`, `--move-to`, `--keep-staging`, …) and says so |
| 5 | **aborted**: the session cannot resume — the medium ran out (end of tape) or a staged slice changed while it was written. The tape is left unsealed | a staged slice that changed: once it is right again, write the label again with the write's own flags (`tapectl volume write <label> --full-confirm --move-to <location>`). The medium's end: that cartridge cannot take this write (written again it plans the same files and meets the same end); the units are still staged, so load another cartridge and write them as a new copy (`tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <location>`) |
| 6 | **the medium proven bad**: the confirm, or a resume's contact check, found the tape is not what was written; the volume is quarantined and no longer counts as a copy | write the data to another cartridge |
| 75 | the catalog was busy before anything was written ([above](#75-catalog-busy)) | run the command again once the other one has finished |

`volume rewrite` uses the same table: a refusal in its preflight, the staged-slice re-read, the
capacity rehearsal or the consent exits 2 with the tape untouched; once the source is retired or
its cartridge erased, a stop exits **4** and `tapectl volume rewrite --resume NEW` finishes it.

`collection run` writes one copy per run, and exits with the worst outcome
among its copies, in the order 6, 5, 4, 3, 2, 0: a run with any copy
quarantined exits 6 whatever the others did.

A failure after a copy is sealed and confirmed exits **0**, because the copy
stands and the tape needs nothing (ADR-0012, 2026-10-07, item 33). The command
prints a `warning:` line naming the step that did not finish, and the command
that finishes it on its own:

| Step that did not finish | Finish it with |
|---|---|
| `volume compact`'s step 3, retiring the source, refused for consent (no terminal and no `--yes`, or a "no" at the prompt) | `tapectl volume compact-finish <source> --force` |
| releasing `collection run`'s staging (a catalog error) | `tapectl stage clean`, which retains whatever is still below its policy |
| recording the copy's figures (`bytes_written`, `num_data_files`, `last_write`) and its `write_completed` event (a catalog error) | `tapectl volume verify <label> --full`: once it passes, it records the figures that are missing or wrong, from the front index it read and the catalog's own rows, and logs a `write_figures_recorded` event naming each one. It records nothing when the tape's slices are not the ones the catalog staged for that volume, and says why |

```text
warning: volume "L6-0002" is sealed and confirmed and counts as a copy, but releasing the batch's staging did
not finish: database error: disk I/O error. The tape needs nothing. To finish it: tapectl stage clean
```

`volume compact --json` says `"source_retired": false` and carries the warning;
`collection run --json` carries it as `release_warning`.

Older builds exited 2 for every one of these failures.

`volume compact`'s step 3 refused for any other reason still exits 2 with its
`error:` line. Those are its absolute-floor refusals (a live slice with no
copy elsewhere, the last eligible copy of a live version): after step 2 they
mean some content was not carried forward to the destination, so that copy
does not stand in this sense. Read the error; `--force` does not reach them.

### `audit`: 0, 1, 2 or 70

`audit` reports its verdict through the exit code, and 2 means one thing only
(ADR-0012, the 2026-10-07 amendment, item 19):

| Exit | Meaning | What to do |
|---|---|---|
| 0 | clean | nothing |
| 1 | warnings only | read them; the audit is advisory (ADR-0004) |
| 2 | at least one **violation** | the findings name each one; `--action-plan` adds the fix |
| 70 | **no verdict**: the audit stopped on an error (a database or config error, an unknown `--unit`, an uninitialised home, a command line that does not parse after `audit`) | read the `error:` line, fix it, run the audit again |
| 75 | no verdict: the catalog was busy ([above](#75-catalog-busy)) | run it again once the other command has finished |

70 is sysexits' `EX_SOFTWARE`. Older builds exited 2 for an error too, so a
script could not tell "fix the policy" from "fix the run".
`contrib/systemd/tapectl-scheduled-audit.sh` logs 70 as "no verdict" and pings
`/fail`; it never reports it as violations.

### `volume verify`: 0, 2 or 3

A failed verify has two outcomes with opposite remedies, and each has its own
exit code:

| Exit | Meaning | What to do |
|---|---|---|
| 0 | every checked file matched | nothing |
| 2 | the verify **proved the medium bad**: the volume is quarantined (or, verified again, still is) and no longer counts as a copy | salvage what still reads and write it to another cartridge ([Verifying a volume](#verifying-a-volume)) |
| 3 | **inconclusive**: a read or transport failure, or an error before any verdict: no cartridge loaded, the wrong tape, an unknown label, a drive that cannot read this generation, a File 0 saying this tapectl cannot read the tape (a `requires` it does not know, #476), `--dry-run` (which `verify` refuses), a database or config error, a command line that does not parse after `verify`. The volume is untouched | check the drive, the cartridge in it or the command line, then verify again |

Once its command line has reached `verify`, `volume verify` exits 2 only
through a quarantine. A script whose command line is known to parse can
therefore act on the exit code alone. A usage error before `verify` exits 2
without any quarantine ([above](#exit-codes)). `volume verify --json` reports the same verdict in its
`quarantine` field (an object when the volume is quarantined, otherwise
`null`).

### `cartridge erase`: 0, 2, 3, 4, 6 or 75

`cartridge erase` erases the tape in the drive and then reads it back
(ADR-0012, 2026-10-12, items 51 and 52). Its codes follow the write family's
where the meaning is the same:

| Exit | Meaning | What to do |
|---|---|---|
| 0 | **erased and verified**: File 0 reads back EMPTY (the default short erase) or blank (`--long`), the chip serial is unchanged, and the catalog records it. A tape that already reads blank or EMPTY is erased again when the catalog still has a given-up (`retired`) volume on that cartridge, so that the erase it never recorded is recorded; the word to confirm it with is then the chip serial | nothing; `volume init` comes next and needs no `--force`: this catalog's record of the verified erase is the proof (ADR-0012 item 57) |
| 0 | **nothing to erase, the tape blank** (end of data at its beginning), and the catalog has nothing on the cartridge | nothing; `volume init` needs no `--force`: a blank tape proves itself |
| 0 | **nothing to erase, File 0 EMPTY** under the short erase, and the catalog has nothing on the cartridge | `volume init` needs no `--force` only when this catalog erased and verified this cartridge itself and nothing was written to it since (the record of that erase is the proof). An EMPTY File 0 nothing recorded (a filemark written by hand, `mt weof 1`) is not proof: `volume init --force`, or `cartridge erase --long` first |
| 0 | `--dry-run` | nothing |
| 2 | **refused, the tape untouched**: no cartridge, a write-protected one, a volume the catalog still holds (it names `volume retire`), a tape that reads blank or EMPTY while the catalog still has a live volume on that cartridge (it names `cartridge mark-erased <barcode>`, with `--force` unless the cartridge is `pending_erase`), a label that was not confirmed or does not match, a tape and catalog that disagree about which cartridge this is, the chip serial unreadable (the MAM read on `device_sg` failed: `tapectl volume identify` shows what the chip reports, `tapectl config check` checks the drive's `device_sg`), the chip naming a different cartridge just before the erase (one swapped while consent was asked), or a usage error | read the `error:` line, act on it, run the command again |
| 3 | **unverified**: the erase ran, but the tape could not be reopened to read it back, File 0 could not be read back and the tape does not prove itself blank, or the chip could not be read after it. Nothing was recorded | check the drive, run `cartridge erase` again |
| 4 | **the erase failed part-way**: the tape may be partly erased. Nothing was recorded | check the drive, run `cartridge erase` again |
| 6 | **the read-back contradicts the erase**: File 0 still holds what was there, or the chip reports a different serial. Nothing was recorded | check which cartridge is loaded and the drive (clean it), then run it again |
| 75 | the catalog was busy ([above](#75-catalog-busy)) | run it again once the other command has finished |

A verified erase whose catalog record does not finish (a database error)
still exits 0: the tape is erased. The command prints a `warning:` line. When
the catalog has a volume on that cartridge, `tapectl cartridge erase` run
again records it: the tape reads EMPTY, the catalog still owes the record,
and the erase runs again, is read back and is recorded, so `volume init` then
needs no `--force`.
`tapectl cartridge mark-erased <barcode>` (`--force` unless the cartridge is
`pending_erase`) records it too, but records no verified erase: `volume init`
then needs `--force`.

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
insufficient copies: 0 < 4 required by unit "<unit>"'s policy (tape-only 2x)
insufficient locations: 0 < 2 required ([defaults] location floor)
(`--force` or `--yes` confirms this in advance)
```

`unit mark-tape-only` reads `min_copies` from the unit's resolved policy
(dotfile > archive set > `[defaults]`) and the location floor from
`[defaults] min_locations`, and holds the unit to them as a tape-only unit: the
copy floor times `[compaction] tape_only_safety_multiplier`, which the
`(tape-only 2x)` names, and the location floor as set, never multiplied
(ADR-0012, 2026-10-12). So at the defaults (2, 2, 2x) it asks for 4 copies in 2
locations; a `0 < 4` copies is not a typo for 2. Set the multiplier to `1` for
no extra margin. Other fact lines it can show:

- `insufficient locations: no copy at required location(s) <names> (policy
  requires <list>)`
- `unit is dirty: on-disk contents changed since the last snapshot — <changes>`
- under the copies line, when the unit has more than one current version,
  `  version <n> of <m> current versions has the fewest copies: <c>`
- when the unit has copies, a line on the weakest of them, just before the
  `(--force …)` line: `coverage for unit "<unit>" rests on <label>, last
  verified <d> days ago`, or `coverage for unit "<unit>" rests on <n> copies;
  weakest is <label>, never verified`, where <n> is the fewest copies any current
  version has; when the versions are unequally covered it reads `rests on <n> copy
  at its least-covered current version — <m> copies are held across all its current
  versions; weakest of them is <label>, never verified` (#463)

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
| `cartridge erase` | neither: the label read from the tape's File 0 (its chip serial, for a tape tapectl did not write), typed at the prompt or given as `--confirm-label` |
| `volume rewrite` | Tier-2 (a version left below policy): `--yes`. The typed consent (the only copy will be the staging disk): the source's label, typed, or `--only-copy-on-staging LABEL`; `--yes` does not give it and there is no `--force` |

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
error: tapectl is not initialized at /srv/tapectl: there is no catalog /srv/tapectl/tapectl.db there. The home
was chosen because TAPECTL_HOME named it. TAPECTL_HOME is set, so the archive is meant to be there already:
check that it names the directory holding tapectl.db.
```

tapectl found no catalog in the home it resolved. The message names that home
and the rule that chose it: `--home`, `TAPECTL_HOME`, the directory of
`--config`, this account's `~/.tapectl`, the home pointer `/etc/tapectl/home`, or
the default `~/.tapectl` ([configuration.md](configuration.md#where-the-configuration-lives)).
Its last sentence says what to do:

- **`To start a new archive there: tapectl init, which mints a NEW escrow
  identity …`** Nothing named a home and the host has no home pointer, so this
  looks like a new machine: run [`tapectl init`](cli/init.md#tapectl-init) if it
  is one. If you are reading tapes of an archive that already exists (an heir, or
  a rebuilt host), do not: run the form the message names next,
  `tapectl init --operator NAME --escrow-public-key KEY`, with the escrow public
  key from the Heir Kit's sheet, so the home adopts the identity the tapes know.
- **`TAPECTL_HOME is set, so the archive is meant to be there already`**, or
  **`This host has a home pointer (…), so its archive is meant to exist
  already`**: tapectl offers no `init`, because one would make a second archive
  beside the real one. Check the variable or the pointer, or run the command as
  the service user (`tapectl-op <command>`): on a first-run install the archive
  belongs to it.

An account that cannot see into the home at all is told so instead
(`cannot reach the catalog … permission denied. The home belongs to another
account …`). `tapectl status` shows the home in use and its rule even then.

### `refusing to run as root`

```text
error: refusing to run as root: the home pointer /etc/tapectl/home names this host's tapectl home
(/srv/archive_meta/tapectl/home), which belongs to the service user, and as root tapectl would open
/root/.tapectl instead. Run it as the service user: tapectl-op <command>, e.g. `tapectl-op status`. To act on a
home as root on purpose, name it with --home <dir> or TAPECTL_HOME.
```

From 1.9.0, on a host with a home pointer, root is refused unless `--home` or
`TAPECTL_HOME` names the home: as root the pointer is never read, so the command
would otherwise open an archive nobody chose. Use `tapectl-op`. `host check`,
`--help`, `--version` and `completions` still run.

### `` `tapectl <old>` was renamed in 1.9.0: use `tapectl <new>` ``

```text
error: `tapectl staging clean` was renamed in 1.9.0: use `tapectl stage clean`
```

1.9.0 renamed some commands and flags, and the old spellings are refused by
name, with exit 2 and nothing done, however they are typed: as a command, with
`--help`, through `tapectl help <old>`, or as a bare old group. Type the new
spelling the message names. The full list is in the
[1.9.0 release notes](releases/1.9.0.md). Under `--json` the refusal is the JSON
error document ([Exit codes](#exit-codes)). A script written for 1.8 fails
loudly here, never quietly: change its spelling.

The three commands every tape's File 1 prints work for good: `init --operator
… --escrow-public-key …`, `catalog rebuild --from-volume --device … --key …` and
`restore unit --unit … --device … --to … --key …`.

`config add collections` and `config remove 'collections[…]'` are refused the
same way, pointing at `collection add` and `collection remove`.

### What is this key set to?

[`config get`](cli/config.md#tapectl-config-get) prints the value in force and
where it comes from: the file, a default, or derived (a drive's `device_sg`):

```text
$ tapectl config get verify_interval_days
defaults.verify_interval_days = 365  # set in ~/.tapectl/config.toml
```

A bare key names the one key of that name, and with one drive configured
`post_warning_bytes` names that drive's
([configuration.md](configuration.md#editing-from-the-command-line-config-get-set-add-remove)).

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

The fix is to edit the named line, by hand or with `tapectl config set` / `config
remove` (which refuse an unknown key by name and validate before they write). There
is no `backend edit` command. See [configuration.md](configuration.md).

### Config: a key an older version wrote

Several keys have been removed, moved or renamed. For these, tapectl replaces
the generic "unknown field" error with a specific explanation. Delete the
line, or rename it where the message says so:

| Key in your config | What the message tells you |
|---|---|
| `[defaults]` `min_copies_for_tape_only`, `min_locations_for_tape_only` | renamed: the keys are now `min_copies` and `min_locations`, with the same meaning and values. Rename the lines. `first-run.sh` offers to do it at step 0 |
| `[[backends.lto]]` `media_type`, `nominal_capacity` | moved: declare the **drive's** generation as `generation = "LTO-6"`; capacity now follows the cartridge's detected generation ([ADR-0010](adr/0010-media-generation-is-a-cartridge-property.md)); `capacity_override` is for virtual drives only |
| `[[backends.lto]]` `usable_capacity_factor` | renamed: the key is now `fill_ceiling`, with the same meaning (the fraction of a cartridge's capacity a write may fill). Rename the line to keep your value, or delete it to take the new default, 0.97. `first-run.sh` offers the rename at step 0 |
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
defaults.min_locations, with the same meaning — the copy requirement every unit starts from (an
[[archive_sets]] entry's min_copies overrides it) and the number of distinct locations a tape-only unit's copies
must span. Rename the lines; the values carry over unchanged.
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
error: configuration error: no LTO drive configured — tapectl does not know what tape drive to use. Add it with
`tapectl drive add --name lto6 --device-tape /dev/tape/by-id/<drive>-nst` (`ls -l /dev/tape/by-id/` lists
the drives; the generation is read from its model), which writes its [[backends.lto]] table to
~/.tapectl/config.toml; the commented example `tapectl init` left there is the same table by hand
```

Every write path (`volume init`, `volume write`, `volume resume`,
`collection run`) needs a `[[backends.lto]]` entry. Add one with
[`tapectl drive add`](cli/drive.md#tapectl-drive-add), or uncomment the example
`init` left in the config file the message names. `config check` says the same
in a `note:` line before any write.

The planners need none (1.9.0): `volume plan` and `collection plan` read a
configured drive's keys without opening it, so a drive that is not attached
plans, and with no drive at all `--generation` stands in for one. Without
either, `collection plan` refuses naming `--generation` and `drive add`, and
`volume plan` prints its plan with `estimated tapes: not estimated: …` and
exits 0.

That message appears only when you leave out `--device`. With `--device`,
tapectl instead reports that no backend has the path you named, and
`configured: none` says there are no backends at all:

```text
error: configuration error: no [[backends.lto]] entry has device_tape = <device> (configured: none)
```

The fix is the same: add the backend.

Read paths (`volume identify`, `volume verify`, `restore`, `catalog rebuild`)
work without a backend as long as you pass `--device`. This is deliberate, so
a rebuilt machine holding only keys can still read its tapes. Without
`--device` they say so:

```text
error: configuration error: no drive configured in ~/.tapectl/config.toml and no --device given: name the drive
with --device /dev/tape/by-id/<drive>-nst, or add it with `tapectl drive add`
```

### Several drives: `pass --device to select one`

```text
error: configuration error: multiple LTO drives configured (<name>, <name>); pass --device to select one
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
error: configuration error: refusing to resolve an LTO drive for a write: drive "<name>": device_sg =
<configured> is not the drive device_tape = <device> names — the kernel binds that tape node to <bound>. Its
cartridge chip (MAM) and log pages would be read from a different device (/dev/sgN numbering moves across
reboots). Set device_sg = "<bound>" in this [[backends.lto]] entry, or remove the device_sg line and tapectl
derives it from device_tape.
```

The message names the correct node. The simplest fix is to delete the `device_sg`
line: tapectl then derives it from `device_tape` on every load (issue #431). Or set
it to the node the refusal named, then check the config:

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

### `device_sg` is not set and cannot be derived

With no `device_sg` line, tapectl derives the node from `device_tape`. Where the
kernel names no node for it, a drive command refuses by name:

```text
drive "<name>": device_sg is not set and cannot be derived from device_tape = <path> — …
```

The rest of the message says why: the path does not exist on this machine (is the
drive attached?), or sysfs does not name exactly one SCSI generic node for it (is
`device_tape` a `/dev/nstN` tape node?). Attach the drive, or set
`device_sg = "/dev/sgN"` in the `[[backends.lto]]` entry (`lsscsi -g` lists it), then
run `tapectl config check`.

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
| `No such file or directory (os error 2)` | `device_tape` (or the `--device` you passed) names a path that does not exist (a typo, or a by-id link for a drive that is not attached) | `ls -l /dev/tape/by-id/` and use the path it lists |
| `Read-only file system (os error 30)` | the cartridge's write-protect tab is set, and the command writes: `volume init`, `volume write`, or a `volume resume` whose seal is not yet recorded | slide the tab, or use another cartridge. Commands that only read open the drive read-only and work with the tab set: `volume verify`, `restore`, `volume identify`, `catalog rebuild`, and a `volume resume` that only re-confirms a recorded seal |

When the path cannot be opened at all, the error follows a logged warning,
`non-blocking open failed during no-medium probe (continuing)`, from the
empty-drive check that runs first.

**The drive lock does not exclude a second command.** The kernel's `st`
driver allows only one open at a time, and that is what produces "busy".
tapectl's own drive lock, `<home>/locks/drive-<node>.lock`, is held by every
command that contacts the drive, from its contact until it exits; it exists so
that the [daily drive poll](operator-guide.md#the-daily-drive-poll) skips
(exit 75) instead of reading the drive's log pages under a running command. A
command that finds it held waits up to a minute (a poll takes seconds), then
warns `another process has held this drive's lock` and carries on, so a second
command still meets the kernel's "busy". The file `/tmp/tapectl-tape.lock`
belongs to the test harnesses (`scripts/mhvtl-verify-gate.sh`,
`scripts/lifecycle-suite.sh`, `scripts/lto6-measure.sh`,
`scripts/lto6-fill.sh`, `scripts/lto-profile.sh`) and to `first-run.sh` step 13 (the step-12 rehearsal
takes it through `lifecycle-suite.sh`); a plain `tapectl` command neither
takes nor checks it. The
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

[`db backup`](cli/db.md#tapectl-db-backup) takes one path: a file to write, or
an existing directory, where it writes `tapectl-YYYYmmddTHHMMSSZ.db` (the
scheduled backup's name). It creates no directories and never overwrites a file
(1.9.0). It refuses, before it opens anything and under `--dry-run` too:

```text
error: db backup refuses: the destination directory <dir> does not exist — create it, or mount the medium it
lives on, and run again (db backup does not create directories)
error: db backup refuses: <path> exists: name another file (a backup never overwrites one), or a directory, where
it gets a dated name
error: db backup refuses: <dir> is not a directory, so <path> cannot be written
error: db backup refuses: <path> ends in .keys, which is exactly where --include-keys puts the private-key
directory — the database file and the key directory would be the same path; give the backup file another
extension, e.g. .db
```

An existing file is a backup taken before: name a new file, or name the
directory and let it take a dated name. Two backups into one directory in the
same second meet the same refusal; run it again a second later. `db backup --to
<path>`, the spelling before 1.9.0, is refused by name
([renamed](#tapectl-old-was-renamed-in-190-use-tapectl-new)).

A missing directory is usually a backup disk that is not mounted. With
`--include-keys`, the private keys go beside the file, with its extension
replaced by `.keys`:

```bash
tapectl db backup /mnt/usb/tapectl.db --include-keys
```

At the default `[logging] level = "warn"`, a log line on stderr comes first,
then the result on stdout:

```text
<timestamp>  WARN tapectl::cli::operations: private key material copied to backup destination — treat this location as secret destination=/mnt/usb/tapectl.keys
database backed up to /mnt/usb/tapectl.db (464.0 KiB); private keys copied to /mnt/usb/tapectl.keys/ (276 B) — treat that directory as secret
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
(snapshots), and `blank` and `missing` (volumes). The volume statuses its
message lists still include `full`, which migration 032 removes in turn (see
[below](#migration-032-cannot-run)): do not choose it. Nothing has been changed:
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
always `/var/lib/tapectl/.tapectl`: a host profile's `SVC_HOME_WANT` moves the
account's home, and its `TAPECTL_HOME_DIR` the tapectl home itself (home2's is
`/srv/archive_meta/tapectl/home`, ADR-0012 item 197). The pointer
`/etc/tapectl/home` names it on an installed host, so ask it rather than typing a path:

```bash
DB="$(cat /etc/tapectl/home)/tapectl.db"   # no pointer: "$(getent passwd tapectl | cut -d: -f6)/.tapectl/tapectl.db"
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

### `migration 030 cannot run`

Migration 030 stores each unit's file list in a narrower shape: a path once
per unit, a sha256 as 32 bytes, a modified time as a number. It converts every
`files` row exactly or not at all, and refuses a value it could not turn back
into the same text:

```text
error: failed to open database: migration error: migration 030 cannot run: 1 files row(s) whose sha256 is not 64
lowercase hex characters (id 4012). Migration 030 converts every files row exactly (paths interned per unit,
sha256 as 32 bytes, modified_at as an integer) and will not guess a value it cannot convert. Correct each named
files row (a sha256 or modified_at you cannot recover may be set to NULL; delete only a row whose snapshot no
longer exists), then run the command again. Nothing has been changed.
```

Look at the named rows with `sqlite3`, the same way as for migration 026 above
(`sqlite3 "$DB" "SELECT * FROM files WHERE id = 4012"`), and correct them.
Do not delete a row whose snapshot still exists: a version records how many
files it holds, and one short a row cannot be staged (it reads as an
interrupted `snapshot create`, see [A snapshot is incomplete](#a-snapshot-is-incomplete)),
while the next `snapshot create` sees the file as added and records a new
version.

The findings it can name, and what to do about each:

- **A `modified_at` not in the spelling every tapectl release writes**
  (`2026-09-01T12:00:00+00:00`).
- **A sha256 that is not 64 lowercase hex characters.**
- **An `is_directory` and `file_type` that disagree**, or a `file_type` that
  is not `dir`, `regular`, `symlink` or `special`.
- **A row whose snapshot no longer exists.** This is the one row to delete:
  it belongs to no version.

Every row they name was edited by hand.
A sha256 or a `modified_at` you cannot recover may be set to NULL: a
version without a sha256 is baselined again by its next stage, and a file
whose modified time is NULL reads as changed to the next
`snapshot create`, which records a new version. A row with no `file_type` at
all is not refused: it takes the type its `is_directory` gives, as migration
005 did.

A `modified_at` in that spelling but before 1677-09-21 or after 2262-04-11 --
1601-01-01, the zero time of an NTFS volume, say -- is not refused, and
neither is a year past 9999 in the spelling earlier releases gave it, with a
leading sign (`+10000-01-01T00:00:00+00:00`). No nanosecond count holds
either, so 030 records no modified time for that file, which is what the walk
records for it today, and the unit still reads as unchanged. It says so once,
on stderr, naming up to ten rows and then the count (`, ... (12 rows)`):

```text
WARN tapectl::db: migration 030 recorded no modified time for 1 file row(s) whose modified_at is outside
1677-09-21..2262-04-11, the range a nanosecond count holds: files row 4012 (snapshot 31, path id 977, was
1601-01-01T00:00:00+00:00). A walk records none for such a file either, so these units still read as unchanged;
nothing else was altered
```

Nothing needs doing about it. The `files` row is gone once 030 applies; the
path it named is `sqlite3 "$DB" "SELECT path FROM paths WHERE id = 977"`.

Once 030 applies, the same command compacts the catalog once, as after 027.

### `migration 032 cannot run`

Migration 032 removes the volume status `full`, which no tapectl release has
ever written. A volume carrying it was set by hand, and 032 will not guess what
it should say:

```text
error: failed to open database: migration error: migration 032 cannot run: volumes.status = 'full' on 1 row(s)
(id 7). Migration 032 removes this status from the schema. No tapectl release has ever written it, so these rows
were set by hand, and 032 will not guess what they should say. Change each named row to a status the new schema
allows -- initialized, active, retired, erased, sealed -- then run the command again. Nothing has been changed.
```

Nothing has been changed. Decide which status tells the truth about each named
volume and set it with `sqlite3`, the same way as for migration 026 above
(`sqlite3 "$DB" "UPDATE volumes SET status = 'retired' WHERE id = 7"`). Only
`sealed` counts as a copy, so choose it only for a volume whose seal you can
stand behind; `volume verify` checks one afterwards.

### `migration 033 cannot run`

Migration 033 removes three cartridge columns no tapectl release has ever
written or read: `total_bytes_written`, `total_bytes_read` and `error_history`
(ADR-0012, 2026-10-07). A cartridge with a value in one of them other than its
default (0, 0, NULL; NULL counts as unwritten everywhere, and an empty text
is a value) had it set by hand, and 033 will not drop it silently:

```text
error: failed to open database: migration error: migration 033 cannot run: total_bytes_written, total_bytes_read
or error_history is set on 1 cartridge row(s) (id 4). Migration 033 removes these three columns from the schema.
No tapectl release has ever written them, so these values were set by hand, and 033 will not drop them silently.
Copy them somewhere if they matter, set each named row back to total_bytes_written = 0, total_bytes_read = 0,
error_history = NULL, then run the command again. Nothing has been changed.
```

Nothing has been changed. Read the values first if you want to keep them
(`sqlite3 "$DB" "SELECT id, barcode, total_bytes_written, total_bytes_read,
error_history FROM cartridges WHERE id = 4"`), then reset them:

```bash
sqlite3 "$DB" "UPDATE cartridges SET total_bytes_written = 0, total_bytes_read = 0, error_history = NULL WHERE id = 4"
```

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
of it. `stage create` refuses such a snapshot before running tar. Delete it
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
running tar, so no time is lost. Register the escrow recipient once:

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

`stage create` checks every recipient before it runs tar or records a stage
set, so this refusal costs nothing. A malformed public key in the catalog is
refused at the same point. Give the tenant a key and stage again:

```bash
tapectl key list --tenant <tenant>
tapectl key rotate --tenant <tenant>
tapectl stage create <unit>
```

### tar is missing, or below the writer's floor

```text
error: tar not found: <binary>
error: GNU tar 1.30 at <binary> is below the writer's floor; the writer's floor is GNU tar 1.35 or later, built with ACL support. Nothing was staged.
error: <binary> is not GNU tar (it says "bsdtar 3.7"); the writer's floor is GNU tar 1.35 or later, built with ACL support. Nothing was staged.
error: GNU tar 1.35 at <binary> does not store an ACL by number (<why>); the writer's floor is GNU tar 1.35 or later, built with ACL support. Nothing was staged.
```

All four come from `[tar] binary` in config.toml (default `tar`, looked up on `PATH`).
tapectl writes each unit with GNU tar and holds it to a floor: GNU tar 1.35 built with
ACL support. Below 1.35, `--numeric-owner` does not reach an ACL entry, so a user's or
group's name would be written into the stream; a 1.35 built without libacl writes no
ACL at all, and the version number alone cannot tell the two apart, so tapectl also
archives a probe file that has an ACL and looks at what tar wrote. Install a tar at the
floor (Debian 13 and Ubuntu 24.04 and later ship one; on an older host build it into
`/usr/local`, see [install.md](install.md#the-writers-tar)) or point `binary` at it.
`tapectl config check`, `init` and `setup` probe the same way and say so beforehand:

```text
tar: tar (GNU tar) 1.35 at '/usr/bin/tar' (meets the writer's floor, GNU tar 1.35 with ACL support)
warning: stage create would refuse this tar: GNU tar 1.30 at /usr/bin/tar is below the writer's floor; ...
```

The floor binds the host that writes. Reading needs no particular tar, so an heir's
`RESTORE.sh` runs with whatever `tar` the machine has. A tar that exists and meets the
floor but fails while archiving shows up as `tar failed archiving unit "<unit>" (exit
<status>)` followed by what tar said.

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

### Staging space: asked about

Staging holds a unit's encrypted slices and nothing else: at most about the
unit's stream (its files plus tar's headers), and about 1/4096 more for age's
tags. `stage create` compares that figure with the free space in
`[staging] directory` before it reads anything.

**It fits.** Nothing is said.

**It may not fit.** You are asked (Tier 2). The figure is an upper bound:
tar stores a hard-linked file once, and with
compression on, nothing can say in advance how well the data will compress.
So a short staging directory is never refused outright:

```text
staging directory <directory> may be too small for unit "<unit>": 1.0 MiB free, and staging it needs up to 5.7
MiB, its encrypted slices at the snapshot's full size (less if tar stores a
hard-linked file once); if it runs out, the stage stops there and its partial slices are removed.
`tapectl stage list` shows what staging holds; a bare `tapectl stage clean` frees what failed
stages left and what is already on tape
```

With compression on, the bracket reads `(less if its data may compress
(compression = "<algorithm>"))`. When other stages are running at the same time
(`stage create --jobs`, `collection run --jobs`, or another `stage create`),
what they may still write is not free for this unit, and the free figure says so:
`… 40.0 GiB free (12.5 GiB of it may still be written by 2 other stages running
now), and staging it needs …`.

A terminal is asked `stage unit "<unit>" — proceed? [y/N]`. A non-interactive
run without `--yes` refuses with the same figures
(`error: stage unit "<unit>" refused: non-interactive session …`), before
anything is read or written. With `--yes` the stage goes ahead and still prints
the figures, ending `— staging anyway (--yes given)`:

```bash
tapectl --yes stage create <unit>
```

If tapectl cannot read the free space at all, it says so and stages without
the check:

```text
warning: could not read the free space of staging directory <directory> (<error>); staging unit "<unit>" without a space check
```

A refused attempt leaves nothing to clean up. If the disk fills during a
stage that went ahead, the failure names the slice being written:

```text
error: cannot write staged slice <directory>/<slice>.tar.age: No space left on device (os error 28)
```

The half-built stage set's files are removed, and the next tapectl command
marks its stage set `failed`. `tapectl stage list` shows what staging
holds; a bare `tapectl stage clean` frees what failed stages left and what
is already on tape.

#### Why a failed stage starts over

A stage that fails part-way — a full disk, a source file that changed, a
stop, a crash — is staged again from the start: `stage create` makes a new
stage set, and tar reads the whole unit again. Keeping the slices already
encrypted and finishing only the rest is not possible while staging writes no
plaintext (ADR-0012, 2026-10-06, item 4):

- tar writes the stream once, to standard output, and tapectl cuts, compresses
  and encrypts each slice as it passes; no copy of the stream stays on any
  disk. The rest of the stream can only come from tar again, and tar makes a
  stream only from its beginning.
- A second tar run is a different stream: the files may have changed since the
  first, the file table's offsets and every slice's hashes belong to one run, and
  tapectl checks each stream as it passes against the list and the per-file sha256s
  of its own run. So the first run's slices cannot be joined to the end of a
  second run's.

What tapectl does instead is keep a late failure from happening, and keep
what is already finished when it can:

- the staging-space check counts what other stages running at the same time
  may still write, so a full disk is asked about before tar starts, not found
  at slice 120 of 130;
- every catalog write a stage makes waits out a busy catalog (another stage
  or command writing) for up to ten minutes rather than failing, and a set
  that reached `staged` is never removed;
- tar's exit 1 (a file changed while tar read it) is refused as that, by
  name, with what to do;
- each encrypted slice is on disk (synced) before it is recorded.

A restart costs that one unit's staging; every other unit's stage set is
kept. `tapectl report failed-stages` lists every stage attempt that failed,
with the hours each lost (ADR-0012 item 91), so the cost can be judged from
the record.

### The source changed after the snapshot

`stage create` checks every file against the snapshot. A file missing or at
another size is refused before tar starts; the content is hashed as it passes in
tar's stream, so a file whose content changed at the same size (BITROT suspected)
is refused when the stream reaches it:

```text
error: DIRTY: source file size changed: <path> (expected <n> bytes, found <n> bytes) — a real edit (size and
content both differ) since the snapshot was taken. Take a new snapshot (`tapectl snapshot create <unit>`) and
stage that instead.
error: source file missing: <path>
error: BITROT suspected: <path> — sha256 differs at an unchanged size (<n> bytes): baseline=<sha>,
current=<sha>. Refusing to stage; investigate before re-staging (`tapectl unit check-integrity <unit>` checks
every file against its recorded baseline).
```

A file that changes while staging reads it refuses the stage too. Each file is
read once from disk, by tar. Before tar starts, tapectl notes each file's device,
inode, size, modification time and change time; after tar has exited, every file
must still have them, **whatever tar said** (and a file changed in the last moments
before the scan is hashed again from disk and must hash as the stream did). A file
that changed in between is refused:

```text
error: DIRTY: source file changed while staging read it: <path> (it changed while tar read it). Its recorded
sha256 would not be of the bytes archived, so nothing was staged. Stage again once the source is quiet; if
the change is real, take a new snapshot (`tapectl snapshot create <unit>`) and stage that.
```

tar itself notices some changes: it exits 1 for a file that changed as it read it,
and `stage create` reports that as:

```text
error: DIRTY: a source file of unit "<unit>" changed while tar was reading it (tar exit 1), so it is refused rather than archived half-changed. tar said:
<tar's lines>
Nothing was staged. Stage again once the source is quiet; if the change is real, take a new snapshot (`tapectl
snapshot create <unit>`) and stage that.
```

Stop whatever is writing to the source (a sync client, a download, an
editor's autosave), then stage again. For a source that is always in use, stage
from a read-only filesystem snapshot: the operator guide's "Staging a source
that is in use" gives the LVM, btrfs and ZFS commands, and, for `collection run`,
a hook that takes and drops the snapshot (see
[its refusals](#collection-run-snapshot-hook-refusals-120)).

For a real edit or a deleted file, take a new snapshot and stage that:

```bash
tapectl snapshot create <unit>
tapectl stage create <unit>
```

`BITROT suspected` is different. The file has the same size as before but
different contents, which is what silent disk corruption looks like. Check
the rest of the unit with `tapectl unit check-integrity <unit>`, and before
you archive the new contents, compare the file against a copy you trust.
`check-integrity` compares against the checksums of the newest staged version
and names that version, so a file you edited on purpose since an older
version is not reported.

From 1.9.0 it tells an edit from damage by the file's modified time. A file
whose content differs at its recorded size is `CHANGED` when its modified time
moved since the snapshot (an edit), and `BITROT` only when it did not. From
1.9.1 the modified time decides a size change the same way: `CHANGED` when it
moved, `SIZE_MISMATCH` when it did not:

```text
integrity check for "family/photos" against v1's checksums:
  OK:            1
  BITROT:        1
    p.jpg — BITROT
```

It exits 1 when a file is `BITROT`, `MISSING` or a `SIZE_MISMATCH`, and 0 when
every file matches. A check that finds only `CHANGED` files exits 0 and ends
with a note naming the snapshot that would archive them (`1 file changed since
the snapshot (v1): tapectl snapshot create family/letters archives it`). A
version recorded with no modified times cannot tell the two apart: its report
says `CHANGED or BITROT (no mtime recorded)` and counts the file with `BITROT`,
and calls a size change `SIZE_MISMATCH`.

### The tar stream is refused

```text
error: the tar stream is refused at byte <n>, "<path>": <rule>: <what was seen>
```

tapectl reads the stream tar writes as it passes and checks it against the exact list
of files it gave tar (ADR-0012 items 253, 270, 274; `volume-format-v3.md` §3.2). A
refusal names the byte, the path and the rule, and nothing is staged; the source is
untouched. The rules:

| Rule | What it means | What to do |
|---|---|---|
| a member is not the next entry of the list | tar wrote a different path, kind or size than the list says | a file changed or appeared under tar: stage again once the source is quiet, or from a snapshot |
| a header names a user or group, or an extended header names one | a name was written where only a number belongs: the tar is below the floor, or ignored `--numeric-owner` | use a GNU tar at the floor ([tar is missing, or below the writer's floor](#tar-is-missing-or-below-the-writers-floor)) |
| an ACL entry's qualifier is not a number | the same cause, in an ACL record | the same |
| a file's ACL is missing from the stream | the file had an ACL on disk and the stream carries none: a GNU tar 1.35 built without ACL support | build tar with `libacl1-dev` installed; `config check` names the tar |
| a global extended header, a GNU long-name member, a header whose checksum is wrong, a member of a type the reader does not know, something other than zeros after the end, the stream is cut short, a listed entry is not in the stream, a hard link's target is not a file listed earlier, a header that cannot be read | tar wrote something tapectl's reader will not carry | note the line and stage again; if it repeats on the same unit, keep the message and report it |

A tar that exits non-zero after a good stream is refused too (`tar failed archiving
unit "<unit>" (exit <status>)`).

**What the snapshot leaves out.** tar is given the exact list, so there is one matcher,
tapectl's: an exclude pattern is applied when the snapshot is taken, and `snapshot create`
reports what each pattern left out. If an exclude pattern (the unit's `[excludes]`, or
`[defaults] global_excludes`) changed after the snapshot, take a new snapshot (`tapectl
snapshot create <unit>`) and stage that. A socket is always left out and reported with the
reason "socket", because tar writes none.

**A file name that is not valid UTF-8** is carried by its bytes, and shown escaped
everywhere (`caf\xe9.txt`): in `catalog list`, in the file table, in `RESTORE.sh --list`.
Type a name the same way to `restore file --file` or `RESTORE.sh --path`.

### `collection run`: snapshot hook refusals (1.2.0)

A `[[collections]]` table with a `snapshot_command` makes `collection run` take a
snapshot first and read each unit from it (the operator guide's
[snapshot hook](operator-guide.md#a-snapshot-hook-for-collection-run)). When
the hook is wrong the run is refused **before any unit is read**: nothing was
snapshotted or staged, no tape was touched, and the exit code is 2. Every
refusal has this shape, naming the collection and the command:

```text
collection "media": snapshot_command `/usr/local/bin/tapectl-snap-take` failed (exit status: 1).
Its stderr:
<what the command wrote to stderr>
The run is refused before any unit is read; nothing was snapshotted or staged.
```

The words after `` snapshot_command `…` `` say what was wrong:

| It says | What happened and what to do |
|---|---|
| `could not be started: <error>` | `sh -c` could not run the command line. Check the path and its execute bit, as the user tapectl runs as. |
| `failed (exit status: N)` | The command exited non-zero. Its stderr is printed under `Its stderr:`. Run the command by hand with `TAPECTL_COLLECTION` and `TAPECTL_COLLECTION_ROOT` set, as the same user (a script that calls `sudo` needs its sudoers entry). |
| `printed nothing on stdout (it must print one line: the absolute path of a read-only snapshot of <root>)` | The command succeeded and printed no path. Print the snapshot's path with `echo`. |
| `printed N lines on stdout (it must print exactly one: … send everything else to stderr)` | A snapshot tool wrote its own messages to stdout (`btrfs subvolume snapshot` does). Redirect them with `>&2` and print only the path. |
| `printed output that is not valid UTF-8` | The command's stdout is not text. Print the path only. |
| `printed "<line>", which is not an absolute path` | A relative path. Print an absolute one. |
| `printed <line>, which cannot be read: <error>` | The path does not exist, or the user tapectl runs as cannot reach it. A snapshot mounted for root alone is the usual cause. |
| `printed <line>, which is not a directory` | The path names a file or a device node. Mount the snapshot and print its mount point. |
| `printed <line>, which is inside the live root <root>: the snapshot must be somewhere else …` | The snapshot lives under the root it is a snapshot of. For ZFS this is `<root>/.zfs/snapshot/<name>`: clone the snapshot, or bind-mount it read-only elsewhere, and print that path. |

Once the snapshot is accepted, a unit that has no folder in it is refused by the
run's snapshot or stage step, and the run stops there (units before it in name order
may already be staged, and stay staged):

```text
unit "media/Alien.1979" (/srv/media/Alien.1979) has no folder in the snapshot: /mnt/tapectl-media/Alien.1979 is not a directory
```

The snapshot does not mirror the root's layout. A unit registered after the
snapshot was taken is not in it either; run `collection run` again. If the root is
a subdirectory of the snapshotted filesystem, print the matching subdirectory of
the snapshot, not the snapshot's top. (A unit that is not under the collection's
root at all says `unit path <path> is not under the collection root <root>, so it
has no counterpart in the snapshot at <snapshot>`; a unit is never read from its
live path in a run that was promised a snapshot.)

A snapshot that could not be released is a warning, not a refusal. The run's work
stands:

```text
warning: collection "media": release_command `/usr/local/bin/tapectl-snap-drop` failed (exit status: 1).
Its stderr:
<what the command wrote to stderr>
The snapshot at /srv/.snap-tapectl-media may still exist; remove it by hand.
```

Remove it with the tool that made it (`btrfs subvolume delete`, `umount` and
`lvremove`, `zfs destroy`). `--json` carries the same text as
`snapshot_release_warning`. A `release_command` also does not run if tapectl is
stopped by a second signal or killed with SIGKILL. Nothing prints a warning then, so
look for a snapshot your script named.

Two refusals come earlier, when the config loads (and in `config check` and
`config set`):

```text
~/.tapectl/config.toml: collections[0] ("media").snapshot_command is empty (leave the key out for no command)
~/.tapectl/config.toml: collections[0] ("media").release_command is set but snapshot_command is not: nothing takes a snapshot, so there is nothing to release
```

An empty `release_command` is refused the same way as an empty `snapshot_command`.
`config check` runs neither command, so the first time either runs is in a real
`collection run`: try the hook on a small collection, or run the two scripts by hand,
before a long batch.

### A unit larger than one cartridge

A unit larger than one cartridge is not refused, and nothing is set to allow it (ADR-0012
items 288 and 289): every unit continues on the next cartridge at the drive's early
warning. Its slices run across tapes, each tape ending its run at the warning, and
restoring it takes every one of those tapes, in slice order. Do not split a unit by hand.

What does refuse a unit is the staging area. A unit is staged whole before its first
tape is written, and its slices are released only as each tape gives them their copies, so
one larger than the staging area is refused:

```text
error: unit "<unit>" is <size> bytes (<size, binary>), larger than the staging area: the filesystem of staging directory <directory> is <total> bytes (<total, binary>) in all. A unit is staged whole before its first tape is written, and its slices are released only as each tape gives them their copies (ADR-0012 items 85 and 129), so it cannot be staged here; nothing was staged. It can be once the staging area is larger than the unit: grow that filesystem, or point `[staging] directory` in config.toml at a larger one.
```

The staging area is the staging filesystem's total size, not its free space
(free space is the [staging-space check](#staging-space-asked-about)'s).
`collection plan` and `collection run` refuse the same unit in the same words,
saying it "can never be batched" where `stage create` says "nothing was staged".

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
tapectl volume identify
```

If this is the wrong tape, swap it. If you mean to overwrite it (a stale
volume, a tape erased with `mt weof`), re-run with `--force`:

```bash
tapectl volume init L6-0003 --force
```

When the cartridge's chip serial matches a cartridge the catalog knows,
re-initialising it records the displacement: the volume it held is marked
`erased`, and tapectl warns about any unit that now has fewer copies:

```text
warning: cartridge <barcode> previously held volume "<old>"; it is now marked erased because these bytes are
being overwritten (ADR-0010).
         *** unit "<unit>" [<unit status>] now has ZERO copies ***
```

### File 0 could not be read

```text
error: refusing to write volume "<label>" (uuid <uuid>): File 0 of the loaded cartridge could not be read (tape
I/O error: read: Input/output error (os error 5)), and the tape is not provably blank — it does not end at its
beginning. ...
```

The drive's driver returns the same I/O error for three different things: a
blank tape, a recorded tape it cannot read right now (a drive that needs
cleaning, a damaged first block), and a tape written in another block size
(LTFS, `tar`). Only the first is safe to write, so tapectl asks the tape
itself: it spaces forward one filemark from the beginning, and calls the tape
blank only if the drive answers that the recorded data ends right there.
`volume init`, `volume write` and `volume resume` all refuse otherwise.

Clean the drive, reseat the cartridge, check it is the one you meant, and run
the command again. If you know what the cartridge holds and mean to overwrite
it, `volume init` and `volume write` take `--force`.

`--force` is not accepted when the catalog binds this cartridge (by its chip
serial) to a live volume: then the unreadable File 0 is that volume's, and the
message names it. If those bytes really are gone, say so first with
`tapectl volume retire <label>` or `tapectl cartridge mark-erased <barcode>`;
to reuse the cartridge, erase it in the drive.

`volume resume` has no `--force`; it refuses, writes nothing, and leaves the
session `interrupted`, so the same `volume resume` can run again once the
drive reads. A resume whose seal is already recorded is not refused: it only
re-runs the read-back.


A cartridge no drive has ever written has no end-of-data mark, so a brand-new
cartridge used to need `--force` here. `volume init` now reads the cartridge's
lifetime totals: when they say 0 MiB were ever written it proceeds and prints
`blank proven by the chip: 0 MiB ever written` (ADR-0012 item 57). A chip that
reports anything else, or cannot be read (a virtual tape library reports none),
keeps this refusal.

### The cartridge already carries a SEALED volume

```text
error: refusing to write volume "<label>": the loaded cartridge already carries a SEALED volume — a valid seal
marker parses at tape position <n>. ADR-0003: sealed volumes are immutable, there is no append, and --force
cannot override this. <the way on>
```

This is a Tier-3 refusal. A sealed tape is never appended to or written over,
and no flag changes that ([ADR-0003](adr/0003-sealed-volumes-immutable-no-append.md)).
`volume init` finds the seal through the tape's own File 0, so it catches a
sealed tape of any label. From 1.9.0 it then looks that volume up in this
catalog by its uuid, and `<the way on>` says which case this is:

```text
It is this catalog's live copy: <volume>, a copy of <n> units. Leave it as it is: eject it (`tapectl drive
eject`) and load a blank cartridge.

It is this catalog's volume <volume> (<status>, condition <condition>), which the catalog has not given up.
Leave it as it is: eject it (`tapectl drive eject`) and load a blank cartridge.

This catalog holds it as volume <volume>, <status>. To reuse the cartridge: `tapectl cartridge erase` erases
the tape in the drive, reads it back and records it. Never degauss or bulk-erase an LTO cartridge: that
destroys its factory servo tracks, and the cartridge with them.

This catalog holds no volume of that identity (`tapectl volume identify` shows what the tape says). To reuse
the cartridge: `tapectl cartridge erase` erases the tape in the drive, reads it back and records it. Never
degauss or bulk-erase an LTO cartridge: that destroys its factory servo tracks, and the cartridge with them.
```

- **This catalog's live copy, or a volume it has not given up** (quarantined,
  say): the wrong cartridge is in the drive. Eject it and load a blank one.
  Never erase a copy to make room; `volume info <volume>` shows what it holds.
- **A volume this catalog holds `retired` or `erased`, or one it has never
  seen** (a 1.0.0 tape after a fresh start, a test home's volume): steps 2
  and 3 below reuse it, once you are sure its data is wanted nowhere.
- A write to a volume already initialised does not look the tape up. Its
  message ends ``If this cartridge should be reused: retire its current volume
  if this catalog holds it (`tapectl volume retire <label>`), then `tapectl
  cartridge erase`, …``: steps 1 to 3 below.

How to erase by hand left the message in 1.9.0; it is in step 2 below.

If you really mean to reuse the cartridge:

1. If this catalog holds the volume, retire it. The command shows which units
   lose a copy, and it refuses outright if this is a unit's last copy. A tape
   whose volume this catalog has never seen (a 1.0.0 tape after a fresh
   start, a test home's volume) needs no retire:
   ```bash
   tapectl volume retire <old-label>
   ```
2. Erase it **in the drive**, with tapectl. It reads the tape first and
   prints what it holds, refuses a volume the catalog still holds, asks you
   to type the label it read from File 0, erases, reads the tape back, and
   records it (the cartridge registered by its chip serial if the catalog did
   not know it, or its retired volume marked erased):
   ```bash
   tapectl cartridge erase --dry-run   # what it holds, and the label to confirm
   tapectl cartridge erase             # asks for that label
   ```
   Unattended, `--confirm-label <label>` gives the label instead; `--yes` does
   not. The default erase writes one filemark at the start of the tape and
   takes seconds: the old data stays on the tape past it, unreachable to a
   normal read but not overwritten. `--long` overwrites the whole tape and
   leaves it blank; it takes hours on real LTO, and is for a cartridge that
   leaves your hands.

   > [!WARNING]
   > Never degauss or bulk-erase an LTO cartridge. LTO tape carries servo
   > tracks written at the factory; a bulk eraser (degausser) wipes them, and
   > no drive can use the cartridge after that. Erase it in a drive.

   Without tapectl, erase it in the drive with `mt` (package `mt-st`). A
   filemark at its start takes seconds; the long erase takes hours on real LTO:
   ```bash
   mt -f "$TAPE" rewind; mt -f "$TAPE" weof 1   # the short erase
   mt -f "$TAPE" erase                          # the long erase
   ```
   Either does the same to the tape as `cartridge erase`, but checks nothing
   first and records nothing: retire the volume first if this catalog holds it,
   then follow it with `tapectl cartridge mark-erased <barcode>`.
3. Initialise it again:
   ```bash
   tapectl volume init <new-label>
   ```
   After a long erase the tape is blank and `volume init` needs nothing more.
   After `tapectl cartridge erase`'s short erase `volume init` needs nothing more
   either: it prints `blank proven by this catalog's verified erase on <date>`
   (ADR-0012 item 57). Only a by-hand `mt weof 1`, which records nothing, still
   wants `--force` (see the previous entry):
   ```bash
   tapectl volume init <new-label> --force
   ```
   To initialise it and write the next copy in one command, `tapectl volume
   write <new-label> --init --full-confirm --move-to <place>` (the same
   contact check, and the same `--force` after a by-hand erase).

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
tapectl volume init L6-0003 --cartridge L6-0003
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
policy — --force does not override it. Load an <drive-gen>-writable cartridge, or write this one in a drive that
can.

If this drive is not really an <drive-gen>, `generation` in the [[backends.lto]] block named "<name>" (<device>)
is wrong — correct it with `tapectl config set 'backends.lto[<name>].generation' LTO-N` (N is the drive's
own generation; `tapectl config show` prints the file) and run `tapectl config check`.
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
volume on another cartridge: `tapectl volume write <new-label> --init
--full-confirm --move-to <place>` initialises it and writes the copy in one
command. The last
one may instead name a `volume verify` followed by a `volume resume`, which
re-confirms a sealed session without writing anything.

### `no staged data to write`

```text
error: no staged data to write — run `tapectl stage create` first
```

Nothing is in staging. Snapshot and stage the units first
([walkthrough](walkthrough.md#7-stage)). `volume write --init` reports it before
it initialises the cartridge: nothing is written to the tape and no volume row is
made (#445).

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
`stage clean` releases it. That is how the same staged data becomes the
second copy on the next tape. It also means leftovers ride along. No flag
picks a subset: if the list is wrong, stop, run
[`stage clean`](cli/stage.md#tapectl-stage-clean) to release what is
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
error: volume "<label>" failed pre-write validation: capacity exceeded: the layout needs <needed> bytes on tape plus a <reserve>-byte end-of-tape reserve, <over> bytes (<over, binary>) over the <available> bytes this volume may be filled to; the fill ceiling is 97% of this volume's 2.50 TB capacity (ADR-0012). Write less to this volume, or, if you know this cartridge can take it, raise the ceiling for this write with --fill-ceiling
```

tapectl has no end-of-tape salvage, so the capacity gate is its only defence
against running out of tape. It compares the whole planned layout with the
capacity decided **at `volume init`**: the cartridge's detected generation,
times the **fill ceiling** (`fill_ceiling`, 97% by default), less the
`enospc_buffer` reserve. The planned layout does not fit on this cartridge, by the
amount the message names. Nothing has been written. A write plans the queue only as far
as the cartridge takes it and leaves the rest staged for the next cartridge (ADR-0012
items 288 and 289), so a batch that is merely larger than one tape is not this refusal.

The ceiling exists because usable capacity is not fixed: a worn cartridge, or an
irregular feed, costs tape the plan assumed it had, and a real end of tape aborts
the whole session (see below). `--fill-ceiling 0.99` (or `99%`) lifts it for one
write, for a cartridge you have reason to trust; the drive's `fill_ceiling` changes
it for every write. Every completed write records the share of capacity it used —
`tapectl report events` lists them as `write_capacity_used` — which is the data the
default is to be tuned from.

Size the batch before you write:

```bash
tapectl volume plan
tapectl stage list
```

`volume plan` plans as many copies as the largest copy floor among the staged
units, and says whose (`--copies N` asks for another number). It needs no drive
attached.

Release what is already on enough tapes (`stage clean`), so that less rides
along. Or split the work across cartridges. The
[Collection](cli/collection.md#tapectl-collection) commands batch
folder-per-unit archives to fit.

### A drive with no `post_warning_bytes`

No refusal names the key: a unit that would pass the fill ceiling is not refused on a drive
whose `[[backends.lto]]` entry lacks `post_warning_bytes` (ADR-0012 item 289). On a drive
that declares it, the plan reaches past the fill ceiling to the cartridge's nominal capacity
and the unit that carries it past is planned to its end, so the drive's early warning ends
the tape inside it. Without the key the plan stops at the last slice that fits the fill
ceiling, inside a unit if that is where it falls, and the rest continues on the next
cartridge; nothing is written past the ceiling.

`tapectl config check` says so, once per such drive:

```text
note: drive "<name>" has no post_warning_bytes, so a write's plan stops at the fill ceiling (97% of the cartridge) instead of using the room past the drive's early warning (ADR-0012 item 113), and no slice cap keeps a slice and the seal inside that room (item 100). scripts/lto-profile.sh measures it (docs/lto-profiling.md); then `tapectl config set 'backends.lto[<name>].post_warning_bytes' <measured>`
```

Give the drive its measured figure
([`post_warning_bytes`](configuration.md#backendslto), which
`scripts/lto-profile.sh --emit` prints) to let a tape fill to the early warning. With one
drive configured the bare key names it; with several,
`backends.lto[NAME].post_warning_bytes`, where `NAME` is the `name` of the drive's table:

```bash
tapectl config set post_warning_bytes 99008M
tapectl config check
```

`99008M` is home2's HP LTO-6 figure; use your own drive's. On a host installed
by `scripts/first-run.sh`, put the figure in the host profile's
`POST_WARNING_BYTES` instead, and step 8 writes it onto the drive
([install.md](install.md)).

### A span would continue onto another LTO generation

```text
error: refusing to write: volume "<label>" is <generation>, and this tape would continue "<unit>" v<n> (slices <a>-<b>) from its range on volume "<other>", which is <generation>. One copy's span stays on one LTO generation, so one drive reads every tape of it (ADR-0012 item 86). Nothing was written and the drive was not touched. Write it onto a cartridge of <generation>
```

The slices a span still owes continue the range on `<other>`, and the
cartridge you initialised is of another generation. Write onto a cartridge of
the generation the message names last.

The same refusal comes from the other side (ADR-0012 item 300): a run that
stops short of the version's last slice, as a compaction of one tape of a
version carries, must be of the generation of the range that begins just after
it.

```text
error: refusing to write: volume "<label>" is <generation>, and this tape would hold "<unit>" v<n> (slices <a>-<b>), the run that comes just before its range on volume "<other>", which is <generation>. One copy's span stays on one LTO generation, so one drive reads every tape of it (ADR-0012 items 86 and 300). Nothing was written and the drive was not touched. Write it onto a cartridge of <generation>, or carry every tape of the version into the same destination
```

To move a version that spans tapes onto a newer generation, `volume
compact-read` every tape that holds a range of it, then `volume compact-write`
once to the one destination: the runs join into the whole version, which
adjoins no range. `volume rewrite` reuses the source's own cartridge, so it
keeps the source's generation and meets the refusal only for a version a
catalog from before the rule already holds on two generations; it refuses at
its rehearsal, before it erases anything.

### A slice a span owes was released from staging

```text
error: refusing to write: "<unit>" v<n> (stage set <id>) is planned on this tape with its slice <s>, whose staged file was released from staging. A range is one unbroken run of a unit's slices, so the slice is never skipped. Nothing was written and the drive was not touched. Stage it again, then write: tapectl volume read-slices <unit> --from <holder> --version <n>
```

Staging is released slice by slice for a unit held across cartridges, once
each slice has its copies. This tape needs one of those slices again, for
example to make a further copy. Run the command the message ends with: it
reads the slice back into staging from a tape that holds it. When no tape
holds it, the message ends with the remedy for an incomplete staged set
instead.

### The escrow key cannot open a staged set

```text
error: volume "<label>" failed pre-write validation: stage set <id> for unit '<unit>' was encrypted without the
current escrow recipient (<reason>) — its slices cannot be recovered with the escrow key; re-stage it: tapectl
stage clean --force (releases the old set), then tapectl stage create <unit> --version <snapshot version>
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
tapectl volume resume L6-0003
```

`volume resume` explains itself when there is nothing it can pick up:

- ``has a `planned` write session, not an interrupted one: … nothing was ever
  written to tape``: clear it with `tapectl volume abort <label>` and run
  `tapectl volume write <label>` again, under the same label. A write that
  stopped this way says the same at once: ``write session was planned and
  never started, so nothing reached the tape and there is nothing to resume.
  Clear it with `tapectl volume abort <label>`, then run `tapectl volume write
  <label> …` again``, with the flags the write was given. A later `volume
  write` replaces an attempt abandoned before its seal. A write error whose
  session is not left open ends ``has no write session left open, so there is
  nothing to resume: run `tapectl volume write <label>` again`` instead of
  naming `volume resume` (1.9.0).
- ``has an `in_progress` write session … ANOTHER PROCESS IS WRITING THIS TAPE
  RIGHT NOW``: tapectl turns crashed sessions into `interrupted` whenever it
  opens the database, so a row still `in_progress` means a writer is live.
  Find it and do not start a second one.
- `has no write sessions at all` / `its write sessions are all resolved`: use
  `volume write`.

Under `scripts/first-run.sh`, re-run step 13 with the same label
(`--from 13 --label <label>`): it finds the interrupted session, offers the
resume, which keeps the write's full readback and move, and then runs the
audit, shelf and Heir Kit steps the first run never reached (`docs/install.md`,
§5).

After a power cut the same holds: resume first, never re-initialise the
cartridge. A cut after the seal leaves the seal on the medium (it is written
with a synchronous filemark, which flushes the drive's buffer before the
catalog records it), and `volume resume` re-confirms it. What each point of
failure costs is tabled in
[design/threat-model.md §3](design/threat-model.md#3-power-the-baseline).

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
takes moments. Hours in `confirm` are the readback of the whole tape that
`--full-confirm` asks for; without the flag confirm reads the front index and
the seal marker back and takes a minute or two.

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
**not** run `stage clean` in between.

Ctrl-C, a SIGTERM (a shutdown) and a SIGHUP (a dropped ssh session) all stop
any long operation the same way: at the next slice, file or tape entry, with an
`error: stopped by a signal: …` line saying where it stopped and what to run
next — `volume resume` after a write or confirm, the same command again after a
stage, verify, read-slices or restore. A second signal stops at once (exit
130); the next command's startup recovers the session as after a crash.

A **full readback** stopped this way (a write's `--full-confirm`, or a full
`volume verify`) is not lost: every file it read back clean is recorded as it
goes. After a write the message names `tapectl volume resume <label>`, which
continues it; after a verify, running the same `volume verify` again continues
it. Either way the next full readback of that volume reads File 3 and the seal
marker again, skips the files already read back clean, and reads the rest. It
skips nothing if the front index on the tape no longer hashes to the one those
files were checked against, or if anything else verified the volume in between
(only the volume's latest readback is continued).

From 1.9.0 the write records what it was asked to do after the seal
(`--full-confirm`, `--move-to`, `--for-location`, `--keep-staging`,
`--keep-staging-for-locations`) in its session directory, and a bare `volume
resume` keeps it, saying so first:

```text
volume "<label>": resuming with the interrupted write's --full-confirm --move-to <place>
```

A resume never turns a full confirm into a quick one. A `--move-to` on the
resume that names another place than the write's is refused, naming both. A
session written by 1.8.1 recorded nothing, so a resume of it is as before: it
runs the quick confirm unless you pass `--full-confirm`, and leaves the full
readback to `volume verify`.

`volume write --prewrite-hash` stops too, inside the full hash of the staged
slices (`… the full hash of the staged slices stopped at file N of M, before
the tape was opened`). Nothing was written and no session was recorded, so
there is nothing to resume: run the same `volume write` again. A `volume
resume --prewrite-hash` stopped the same way says so (`volume resume: … the full
hash of the staged slices stopped at file N of M, before anything was
written`), writes nothing and leaves the session as it was; run the resume
again.

Output to a pipe whose reader has gone (`tapectl … | head`, or a `| tee`
killed by the same Ctrl-C) is dropped rather than ending the command: it
finishes, and exits with its own status.

### A read or drive error during the write

```text
error: volume "<label>" write interrupted: execute stopped at position <n>: staged source read error: read source: <OS error>. The
tape is left unsealed and the session is `interrupted`, not aborted: ...
```

```text
error: volume "<label>" write interrupted: execute stopped at position <n>: tape I/O error: write: Input/output error (os error 5). ...
```

A staged file the disk could not read, or a drive error other than a full
tape, stops the write the way Ctrl-C does: the files before position `<n>`
are whole on the tape, and the session stays resumable. A `staged source read
error` is the staging disk (check `dmesg`, the RAID, the mount); a `tape I/O
error` is the drive or the cartridge (clean the drive, reseat the cartridge).
Fix the cause, keep the same cartridge loaded, and run:

```bash
tapectl volume resume <label>
```

Resume writes position `<n>` again from its start. If the tape turns out to
hold fewer files than the catalog recorded (a power loss can lose the last
few), resume continues from what the tape holds and says so in the session
log. Releases without issue #408's fix aborted the session for good on both.

### `warning: Logical Block Protection is not available on <device>: …`

From 1.7.2 every open of the drive arms its Logical Block Protection: each
block tapectl writes carries a CRC the drive checks before it writes it
(ADR-0012 items 161-166). When the open cannot arm it, a write prints this
once per drive per command and goes on without it:

```text
warning: Logical Block Protection is not available on <device>: <why>. tapectl writes without it; the write is
as strong as one by tapectl 1.7.1 (its sha256 checks are unchanged), and the catalog records that LBP did not run.
```

Nothing is wrong with the tape. The write is checked as every write before
1.7.2 was, `volume info` shows `Write check: none (Logical Block Protection
did not run: <why>)`, and `report verify-status` shows `write check: no LBP
(<why>)`. A read that cannot arm it says so in the session log only
(`Logical Block Protection is not available on <device> for this read:
<why>`). There is no config key that turns LBP off. To have it, fix what
`<why>` names:

| `<why>` begins | Means | Fix |
|---|---|---|
| `no SCSI generic node is bound to <device>` | the kernel's `sg` module is not loaded | `sudo modprobe sg` (and add it to `/etc/modules`) |
| `sg_modes could not be run` or `sg_wr_mode could not be run` | sg3-utils is not installed | `sudo apt install sg3-utils` (`scripts/first-run.sh` step 2 checks for both tools) |
| `READ POSITION (short form, service action 00h) on <sg>: sg_raw could not be run` | sg3-utils is not installed (`sg_raw` is in it); asked only after the drive refused a block | `sudo apt install sg3-utils`, then `tapectl volume resume <label>` |
| `sg_wr_mode could not open <sg> for writing` | the user tapectl runs as cannot write the drive's sg node | add it to the node's group (usually `tape`, as for the `nst` node) and log in again |
| `the drive has no Control Data Protection page` or `the drive refused Reed-Solomon LBP (method 01h)` | the drive has no LBP, or not the Reed-Solomon kind tapectl arms (LTO-5 and later do; mhvtl's TD7 and later speak only CRC32C) | none: the write is as strong as one by 1.7.1 |
| `the drive's page did not take` | the drive accepted the setting and reads back another | none; report it with `sg_modes --page=0x0a,0xf0 <sg>`'s output |
| `st refused a 524292-byte block size` or `st cannot carry a 524292-byte block on this host's SCSI path` | the host's st driver or HBA cannot move the 512 KiB block and its 4-byte CRC in one command (mhvtl cannot either) | none short of another HBA; the write is as strong as one by 1.7.1 |

### The drive refused a block: `writing on again from the start of tape file <n>, once`

With Logical Block Protection armed, a write error is how the drive refuses a
block whose CRC does not match the bytes that reached it: the block changed
between tapectl and the drive (memory, the HBA, the cable or the drive's
buffer). st cannot tell that from any other write error, so every one counts
as a refusal. The drive writes nothing for a refused block, and the tape's
layout fixes where every file starts, so tapectl reads where the drive
stopped, goes back to the start of the file that holds the first block the
drive did not take, and writes on from there, from staging, with fresh
hashes and CRCs (ADR-0012 item 174). It reads the position with READ
POSITION's short form on the drive's sg node (`sg_raw`, item 185), because the
HP LTO-6 refuses the form st's `MTIOCPOS` and `mt tell` send. It prints, and
writes to the session log:

```text
the drive refused a write with Logical Block Protection armed in tape file <n>; its position (logical object
<lba>) is in tape file <m>: writing on again from the start of tape file <m>, once (ADR-0012 item 174)
```

The catalog records a `write_lbp_refusal` event and a `write_lbp_retry` event
naming the file. If the rest of the write goes through, the session seals and
exits 0: every block on the tape was checked by the drive, the re-sent ones
too. Its confirm is the full readback even without `--full-confirm` (item 186).
The drive reports a refusal late (on the HP, a few blocks after the bad one),
so a file before the one being written may be short, and the readback proves
every file before the tape counts as a copy. The
kernel log names each refusal `logical block guard check failed` (`sudo
dmesg`); one is worth a look at the HBA and the cable even when the retry
worked.

There is **one retry per run**. A second refusal in the same run is not
retried (next entry). A `volume resume` is a new run, so it gets one retry of
its own.

### `… the second write the drive refused with Logical Block Protection armed in this run …` (exit 4)

A second refusal in one run stops the write, as any other drive error does.
Nothing is sealed, so the tape is unsealed and nothing on it counts as a copy;
the session is `interrupted`, not aborted, and exits 4. The error reads:

```text
error: volume "<label>" write interrupted: the write of tape file <n> stopped: tape write refused with Logical Block
Protection armed: write: Input/output error (os error 5). With Logical Block Protection armed this is how the drive
refuses a block whose CRC did not match the bytes it received: a block changed between tapectl and the drive
(memory, the HBA, the cable or the drive's buffer). A drive or medium error reads the same through st, and the
refused block may be in an earlier file, since st and the drive write behind. The kernel log names a refusal
"logical block guard check failed". This is the second write the drive refused with Logical Block Protection armed
in this run; the first was retried (ADR-0012 item 174), and a second is not. A write the drive refuses with Logical
Block Protection armed is a block that changed between tapectl and the drive: check the host-to-drive path (the
HBA, the SAS cable and its connectors, the drive itself) before you resume. The tape is left unsealed and the
session is `interrupted`, not aborted: ...
```

When the second refusal surfaces at the seal, the error begins `tape write
refused with Logical Block Protection armed: the write of tape file <n>
stopped:`, names the filemark after the file instead of `write:`, and ends
`— volume "<label>"'s write session stopped part-way and is left resumable:
…`; it exits 4 as well. When tapectl cannot tell from the drive's position
which file to go back to, the first refusal already stops the run, with `It
was not retried: <why>.` and the same advice, and exits 4 too.

Two refusals in one run point at the path between the host and the drive,
not at the cartridge. Check `sudo dmesg` for `logical block guard check
failed`, reseat the SAS cable at both ends, look at the HBA (its firmware, and
whether another card or slot behaves), and clean the drive. Then, with the
same cartridge loaded:

```bash
tapectl volume resume <label>
```

Resume writes again from the start of the first file the drive's position
says is not whole, with one retry of its own, and its confirm reads every file
back whatever `--full-confirm` says (item 186). If refusals keep coming, write
the staged data from another host or through another HBA.

### `note: the drive behind <device> (<sg>) had its Logical Block Protection page armed when tapectl opened it`

tapectl clears the drive's Logical Block Protection page before it lets the
drive go. A tapectl that could not (killed with SIGKILL, a second Ctrl-C, a
host power loss) leaves it armed, and the next tapectl command that opens
the drive finds it and says so (ADR-0012 item 164):

```text
note: the drive behind <device> (<sg>) had its Logical Block Protection page armed when tapectl opened it: a tapectl
that crashed or was killed, or a hand-run sg_wr_mode, left it so. Until it is cleared, other programs' reads of
this drive (RESTORE.sh, dd) fail with I/O errors, never wrong data. tapectl has cleared it.
```

An open that arms the page itself ends the note with `tapectl clears it when
this command closes the drive.` Either way there is nothing to do: the
command goes on, and the page is clear when it exits.
[`tapectl drive status`](cli/drive.md) shows the page without changing it:
`LBP page:  ARMED (<sg>), and no program has the tape node open: …`, or
`LBP page:  not armed (<sg>)`.

If a close cannot clear the page, it says so:

```text
warning: could not clear the Logical Block Protection page of the drive behind <device> (<why>): until it is
cleared, other programs' reads of this drive (RESTORE.sh, dd) fail with I/O errors, never wrong data. Run any
tapectl tape command, which clears it first, or power the drive off and on.
```

Run any tapectl command that opens a tape in the drive (`tapectl volume
identify` will do), or switch the drive off and on: a power cycle clears the
page too. Without tapectl, as for an heir with only RESTORE.sh, the power
cycle is the way ([`RESTORE.sh`: every read fails with an I/O
error](#restoresh-every-read-fails-with-an-io-error)).

### A real end of tape during the write

The drive's early warning is not this. Since 1.6.0 a write that reaches the
warning finishes the slice in flight, seals the tape short and exits 0; the unit
it interrupted is a `range` on that tape and continues on the next cartridge, and a unit
planned after the warning has no row and stays staged (ADR-0012 items 288 and 289). This section is
the medium's hard end past the warning (more than 16 refusals of one block in
a row), a refused filemark, or a warning before the first data slice.

If the drive reports that it is out of space, the session ends as a clean
abort. The fill ceiling (above) is what keeps this rare; if it happens on a
cartridge whose plan was under the ceiling, look at the wear figures `volume write`
printed before it started and at `tapectl cartridge info <barcode>`: a WARNING there
means the drive itself flagged the medium, and the cartridge should not be written
again. Write the same staged data to another cartridge. The error reads:

```text
error: volume "<label>" write aborted: execute failed at position <n>: tape full: write: No space left on device (os error 28)
```

The same shape covers a slice whose hash no longer matches staging
(`hash mismatch at position <n>: …`). Any other failure while streaming a
file interrupts the write instead
([above](#a-read-or-drive-error-during-the-write)).

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
tapectl stage clean --force --unit <unit> --version <n>
tapectl stage create <unit> --version <n>
tapectl volume write <label>
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
- **The next `volume write` replaces it.** When the new session is planned,
  the aborted session's `writes`/`write_positions` rows and its staging
  session directory are removed, and a `write_session_superseded` event names
  them; the abort itself stays in the `write_aborted` event. (Releases without
  issue #401's fix kept the old rows, and the retry failed with `UNIQUE
  constraint failed`.)
- **`volume abort` is not needed.** The session is already aborted.
  [`volume abort`](cli/volume.md#tapectl-volume-abort) is for a `planned`
  session, or an interrupted one you know cannot be resumed.

Running into the physical end means the capacity decided at `volume init` was
more than this cartridge really holds. Typical causes are a `capacity_override`
or a registered `--capacity` set too high, or a short or worn tape. A new
`volume write <label>` starts again from the beginning of the same tape with
**every** staged set, so it will run out again unless the batch shrinks.
Before you retry, correct the cause, check the batch with `volume plan`, and
release what is already safe with `stage clean`. Use another cartridge if
this one is suspect.

### Confirm could not complete, or the volume was quarantined at write

After sealing, the write reads the tape back ("confirm"): by default the
front index and the seal marker, under `--full-confirm` every file. There are
two failure outcomes:

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

### `volume rewrite`

- **A source that holds a range** (a unit's slices that the tape took before its early
  warning) is carried like any other volume: the rewrite writes the slices the source
  holds, and a set it never took has no row (its slices stay staged until their version is
  whole elsewhere; `--with-staged` carries them). `volume compact-read`, `volume
  compact-finish` and `volume compact` carry it the same way. There is no refusal to look
  for.
- **"refused: some of this volume's live data has no copy on any other volume"**: expected when
  the volume holds the only copy. Give `--only-copy-on-staging <SOURCE_LABEL>` (or type the label
  at a terminal) if you accept the window in which the staging disk holds the only copy.
- **"the staged copy of unit ... is not what the catalog recorded"**: the staging disk changed;
  nothing was destroyed and staging was put back. Check the disk, then run it again.
- **"the new volume ... would not fit its cartridge"**: the live slices fit but the volume's own
  files (envelopes, index, catalog.db, backups, padding) do not; nothing was destroyed. Compact onto
  a larger generation, or raise `--fill-ceiling` only if you know the cartridge takes it.
- **"The rewrite is NOT finished: its staged slices are held"** (after the erase): load the same
  cartridge (the message names the chip serial if another is in the drive) and run
  `tapectl volume rewrite --resume NEW`. `stage clean` prints "held by the rewrite of volume ..."
  and `--force` does not release them.
- **`--resume` says the staged slices were released**: every unit has a fully read back copy on
  another volume. The record is closed and the source's cartridge is free: `volume init <LABEL>`
  and a normal write reuse it.
- **"sealed but was not fully read back"**: run `tapectl volume verify NEW --full`, then
  `--resume`.
- **The cartridge is unusable after the erase**: `volume init OTHER` on a blank cartridge, then
  `volume compact-write --to OTHER --full-confirm`, which lifts the hold when it passes.
  Do not run `--resume` (it would use the old cartridge); then `tapectl stage clean`.

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
    tapectl volume compact-write --to L6-0010
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
staged; to finish, load the destination and run `tapectl volume compact-write --to L6-0010`, then
`tapectl volume compact-finish L6-0001`
```

Without `--to`, the prompt asks for the destination's volume label. Ctrl-D,
or Enter with no label typed, stops with the destination left as `<DEST>`:

```text
error: no destination label provided — the live slices of "L6-0001" are staged; to finish, load the destination
and run `tapectl volume compact-write --to <DEST>`, then `tapectl volume compact-finish L6-0001`
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
volume "<label>" QUARANTINED: <n> of <m> failures prove the medium is bad. Its status is untouched (ADR-0012,
2026-09-17) — only its condition changed, from "<old>" to "quarantined" — but it no longer counts as a copy, so
`volume retire` will no longer refuse it as the last one — salvage what still reads off it first (`volume
read-slices <UNIT> --from <label>`).
```

The volume's **condition** becomes `quarantined` (its **status** stays
`sealed`), and it stops counting as a copy. `audit` will now report the
missing copies. A volume that was already quarantined and fails again prints
`volume "<label>" was ALREADY quarantined; this verify confirms it — <n> of
<m> failures prove the medium is bad.` and exits 2 as well. Salvage what
still reads, write it to a new cartridge, then retire the volume:

```bash
tapectl volume read-slices family/letters --from L6-0001
```

(`--version N` reads one version; without it, every version on the tape except
reclaimable or purged ones. A read that does not finish removes what it wrote.)

Then load a blank cartridge and write the salvaged slices to it, with the full
readback, and move it where the lost copy was:

```bash
tapectl volume write L6-0009 --init --full-confirm --move-to <place>
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

**A read the Logical Block Protection check refused** (1.7.2). With LBP armed
for a read, the drive sends its CRC with every block and tapectl checks it. A
block that fails the check is a read error, listed as `content_unreadable (not
medium evidence)`, so the verify exits 3 and the volume is left alone:

```text
    position <n>: content_unreadable (not medium evidence) — expected file readable, found read failed: tape I/O
error: Logical Block Protection check failed on block <b> of this file: the drive sent CRC <X>, the block tapectl
received computes to <Y>; the bytes changed between the drive and tapectl, not on the medium
```

The bytes changed on their way from the drive to tapectl, as a refused write
means on the way to it ([above](#writing-a-volume)): check the HBA, the cable
and the drive, then verify again.

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
label` (a label reused after a retire), `wrong tape: the loaded tape's File 0
names volume "<on tape>", but this catalog knows that uuid (<uuid>) as
"<label>"` (the catalog knows the tape under another label), and `wrong
cartridge: …` when the chip serial disagrees. `volume verify` and `restore`
with no label find the volume from File 0's uuid and refuse the same way; a
refusal after the drive was opened is recorded as a `failed` contact, as a
labelled one is. `volume identify` shows what is loaded, and needs no key. It
prints File 0, then a `catalog:` line on stderr (`this tape is volume "<label>"
(status <status>)`, or `no volume with that uuid` for a tape this catalog has
never seen) and exits 0. When the tape contradicts the catalog, including the
wrong-tape cases above, it prints a `warning: this tape contradicts the
catalog.` block and exits 2:

```bash
tapectl volume identify
```

### A unit held across cartridges

A version of a unit may be held as ranges, runs of its slices on several
tapes: every unit continues on the next cartridge when a tape reaches the
drive's early warning. `restore unit` and `restore file` read it across
cartridges, and **may start on any tape that holds part of it**: they take the
files that tape holds whole, ask for each further cartridge by label, sticker and
chip serial, and feed the members to one `tar` run. The answer "skip" at the
prompt (I do not have it), or nobody at the terminal, restores the files this tape
holds whole and lists the rest with the tapes that hold them. `catalog locate
<unit>` shows which tapes hold which slices (its `Holds` column).

A slice no live volume holds is not a refusal (ADR-0012 item 266 and D17): the
restore takes the files the live tapes hold whole and lists every file in that
slice in its INCOMPLETE report, each with the reason:

```text
unit "<unit>" version <n>: slices <a> to <b> of it are on no live volume, so the files in them are left out.
Where the catalog saw them: "<label>" is given up, retired or erased for slices <a> to <b>. A tape this catalog
does not know that holds them is brought into it by `tapectl catalog rebuild --from-volume`.
```

When those slices are still staged, the reason says so instead: an ordinary
`tapectl volume write` writes them onto another cartridge, and a later restore
can then read them.

No one at the terminal (a script, a timer, a pipe on stdin), where the version is
held across cartridges, is not a refusal either: tapectl swaps no cartridge without
a person, so it logs a `WARN` line and restores the files this tape holds whole.
The line reads:

```text
WARN unit "<unit>" version <n> is held across cartridges (slices <a> to <b> from "<label>" (<where>), then
slices <c> to <d> from "<label>" (<where>)), and tapectl swaps no cartridge without a person at the terminal
(--yes cannot load one): the files this cartridge holds whole are restored and the rest are listed, with the
cartridges that hold them. Run it from a terminal and it asks for each cartridge in turn.
```

and the restore ends INCOMPLETE, listing the files on the other cartridges.

Run it from a terminal. A version a single tape holds whole restores from that
tape alone, with no prompt. `restore volume` plans each unit it holds a range of
as `restore unit` does: across cartridges at a terminal, and otherwise the files
this tape holds whole, with the rest listed. RESTORE.sh has the same prompt: press
ENTER when the cartridge is loaded, type `done` and ENTER if you do not have it,
`quit` to stop.

### The destination is not empty

```text
error: refusing to restore into <dest>: it is not empty (it holds "<entry>", and maybe more). tar would keep every
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

A destination that is not empty is refused before the tape is read (above), so a
file that already exists in `--to` is not the usual cause. If one appeared in
`--to` while the restore ran, `tar` keeps the old one, and the restore's own check
of every restored file against the unit's file table names it as damaged or
missing (see "The restore is INCOMPLETE: damaged files" below). Restore into an
empty directory, or pass `--overwrite`.

### `restore volume`: some units were not restored

```text
restored "family/letters" v1 (1 slice) to /srv/restore/family/letters
FAILED "family/photos" v2 (12 slices) to /srv/restore/family/photos: slice 3 checksum mismatch on tape: expected <sha>..., got <sha>...
restored "mike/notes" v4 (1 slice) to /srv/restore/mike/notes
2 of 3 units restored from L8-0002
error: 1 of 3 units not restored from L8-0002
```

`restore volume` reads the tape once, front to back, and one unit's failure
does not stop the others. The report lists the units in the order they lie on
the tape, and the command exits 2, as for any error, when any unit failed.
Each unit has its own `restores` row. The units that were restored are
complete. For each failed one, read its error in the matching section on this
page, empty its directory, and restore it with `restore unit` (from another
copy if the tape is damaged; `catalog locate` lists them).

With `--fail-fast` the first failure ends the pass, and the units after it
are reported `not attempted`.

A unit that cannot be planned at all (see the sections above) is known before the
drive is opened. It is listed `FAILED` with its reason, first in the report, and
never read, and the other units are restored (#573); with `--fail-fast` the pass
stops before any unit is read and the others are reported `not attempted`. With units named by `--unit`, one that would be
refused refuses the whole set instead: each was asked for. Before it opens the
drive, `restore volume` also refuses the whole set when one unit's directory
would lie inside another's:

```text
error: unit "<a/b>" would be restored inside unit "<a>"'s destination (<dir>/a/b is inside <dir>/a). Restore them
separately, each to its own --to. Nothing was read from tape.
```

Name the units with `--unit` so that no two of them nest, and restore the
remaining one with `restore unit`.

### `restore file`: the file is not in the catalog

```text
error: unit "<unit>" version <n> has no file "<file>" in the catalog's record of what it archived, so the tape was
not touched. A path is relative to the unit's root and matched exactly; `tapectl catalog search "<words of the name>"
--all-versions` finds one and names the versions that hold it (without `--all-versions` it searches each unit's newest
version only), and `tapectl catalog list <unit>` lists the newest version's files.
```

`--file` is checked against the version's file list before the drive is
opened. Give the path relative to the unit's root, with no leading `/` or
`./`, exactly as `catalog list` prints it:

```bash
tapectl catalog search "letter mum"
tapectl catalog list family/letters
```

A directory is refused the same way (`"<path>" is a directory …`):
`restore file` restores one file. Restore the unit and take the directory
from it.

### Not enough disk space for the restore

```text
error: not enough disk space in <dir> for this restore: it needs about <size>, and <size> is free. Nothing was read
from tape. <why>. Free space there, choose a larger disk with --to, or put the scratch directory on another disk
with --scratch DIR. (--no-space-check skips this check, for a filesystem that holds more than it reports free,
such as a compressed or thin-provisioned one.)
```

`<why>` names which of two restores this is, because they need different room:

- **A unit** (`restore unit` or `restore volume`): each slice is decrypted (and
  decompressed) in memory off the tape and its members are fed to `tar`, so only
  about the unit's size is needed in `--to` ("bytes of stream" in `stage info`,
  compressed or not); no slice is written to disk.
- **`restore file`**: the file is extracted in the scratch directory and moved
  into place, so it needs the file's size on the scratch disk and in `--to`. Only the
  slices that hold it are read.

A repair of a damaged file from another copy extracts that copy's file in the scratch
directory first, inside `--to` or inside `--scratch DIR` when you name one. RESTORE.sh,
the heir's path, asks for the units' size plus about two slices in its work directory
(`--work DIR`).

`restore volume` checks the whole set at once: every unit it restores stays.

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

When the restore runs as anyone but root, tapectl prints one line on stderr,
once, before the restore:

```text
note: restoring as a non-root user, so the restored files are owned by you, not by the owners the archive recorded
```

The contents are exact, and the files come back by the numeric ids the stream
recorded; `restore` prints the unit's id map (each id with the name the writing host
gave it, or none) and never acts on it. Only ownership differs. If the original owners
matter, run the restore as root. The service user of a first-run install is
not root. On a host with a home pointer, a command run as root is refused unless
`--home` or `TAPECTL_HOME` names the home
([refusing to run as root](#refusing-to-run-as-root)).

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
cannot help here: it registers a public key only. Or give the key for this one
command: `restore unit|file|volume --key FILE` (repeatable, 1.4.0) adds an age
identity file to the home's keys without copying it in. When none of them opens the
slice, the refusal ends `(none of the identities in the --key file(s) given — a.key —
nor the home's own keys opens this slice)`. A file that is not an identity is refused
before the drive is opened: `error: encryption error: --key PATH is not an age X25519
identity file: <why>`. `catalog rebuild --key` takes the same flag. Otherwise recover
with the operator or escrow key. See [keys-and-recovery.md](keys-and-recovery.md).

### A slice fails its checksum on the way back

```text
error: slice <n> checksum mismatch on tape: expected <sha>..., got <sha>...
```

The ciphertext read off the tape differs from the checksum recorded when it
was staged. tapectl does not stop there. If another cartridge holds
an identical copy of the slice (`catalog locate` lists them), it asks for that
cartridge by label, sticker and chip serial (see "A damaged slice: tapectl asks
for another cartridge"). Only chunks that authenticate are ever fed to `tar`, so
nothing damaged reached it. If no copy can give the slice whole, the members in
its bad 64 KiB chunks are left out, **never zero filled**, and the restore ends
INCOMPLETE (see below); in a compressed slice nothing after the bad chunk decodes,
so the members from the chunk to the slice's end are left out. Run a full
`volume verify` on the damaged volume.

### A damaged slice: tapectl asks for another cartridge

```text
Slice <n> of unit "<unit>" v<v> is damaged on <label>: slice <n> checksum mismatch on tape: …
<label2> holds an identical copy of it (cartridge <sticker>, chip serial <serial>).
Unload the cartridge in <device> (`tapectl drive eject`), load <label2>, and press Enter. Type "skip" to leave out the members the damage touches, or "stop" to give up (what is restored so far stays).
```

Load the cartridge named and press Enter. tapectl checks that it is that volume
(File 0, chip serial, `requires`) and says so if it is not, then asks again.
That cartridge keeps serving while it holds what is wanted, so several bad
slices in a row cost one swap. `skip` leaves out the members in the damaged chunks (never zero filled) and
carries on. `stop` ends the restore; what was restored stays. A volume marked
"last resort" is quarantined (a verify proved its medium bad); it is asked for
only when nothing else holds the slice.

### `slice <n> of unit … cannot be read from <label> (…), and an identical copy of it is held by …`

tapectl swaps no cartridge without a person at the terminal, and `--yes` cannot
load one. Run the restore from a terminal and it will ask for the cartridge, or
restore directly from one of the holders it lists with `--from`. Nothing further
was written; what was restored before that slice stays.

### The restore is INCOMPLETE: holes and damaged files

```text
restore of unit "<unit>" v<v> into <dest> is INCOMPLETE
  slice <n> on <label>: <k> hole(s), <bytes> byte(s) in all, at plaintext byte(s) <a>..<b> — …; the members they touch are left out, and the slice as a whole is unproven, …
  slice <n> on <label>: unproven, not shown to be the slice the catalog recorded — …
  <m> file(s) damaged or missing:
    <path> (<why>)
```

No copy could give those chunks, so the members they touch were **left out**: nothing
is filled in with zeros, and `tar` was never given a damaged byte. The per-file check
(each restored file hashed on disk against the sha256 in the unit's file table) names
what is missing or differs. Everything else is whole. tapectl first tries to fill the
listed paths from another copy of the same version (a different stage set), through
that copy's file table; the lines "does not match the version's recorded checksum … not
kept" and "another copy could not be used" say what happened. Restore the lost paths
from another cartridge, or replace the file from the source. All lost paths are in the
session log and in `.tapectl-damaged-files.txt` in the restored directory.

### A slice is "unproven"

A line that says a slice is unproven means tapectl could not show it to be the
slice the catalog recorded. A chunk's tag proves only that it was encrypted for
one of your keys; any other age file written for the same key authenticates
just as well. So a slice is whole only when its checksum matches the one
recorded at staging or, if that does not match, when its decrypted (and
decompressed) contents match the plain checksum recorded for it. It stays unproven
when it has holes, or when the catalog records no plain checksum for it.

Unproven is not lost. The files in the slice are checked one by one against
the sha256 recorded for each in the unit's file table, and the restore ends whole
only if they all match. Trust the file list, not the slice. To have the slice
itself proven, or another copy tried in its place, restore again from another
cartridge that holds it.

### `slice <n> of unit … cannot be completed: the cartridge in the drive holds a different age file for it`

A slice is one age file, and it is read from one file only: chunks of two files
are never mixed, because each authenticates under its own key and neither
vouches for the other. This copy's file has a different header and nonce from
the part already read. If the message says "this copy's checksum matches the
catalog, so the part already read is not the slice", the first read was the
wrong file (a wrong tape position or a wrong catalog entry). Restore again from
that cartridge directly with `--from`. Otherwise neither copy can be shown to be
the slice; restore from another cartridge.

### `… file(s) can be filled from another copy of this version, held by …`

The fill needs a cartridge swap and there is no terminal. Run from a terminal,
or restore the lost files from the holder named. Another version of the unit is
never used as a source, even where the file is unchanged.

### `<path>: the copy on <label> does not match the version's recorded checksum (…); not kept`

The other copy's file differs from the sha256 staging recorded, so it was not
written over the damaged one. The destination keeps what `tar` wrote.

### `restore of unit … stopped at the operator's request while waiting for <label>`

You answered `stop` at the swap prompt, or the input ended. What was restored
before stays; run the restore again.

### RESTORE.sh: not enough disk space, or OUT OF DISK SPACE

```text
FATAL: unit <unit> needs its files, and the work directory one slice twice: /restore has 8589934592 bytes
free and this needs about 53687091200. Nothing has been read from a slice yet. Free space there, or choose a
larger disk with --to or --work DIR; --no-space-check skips this check.
```

RESTORE.sh (`--restore`) reads one slice at a time, decrypts it (and decompresses it,
for a compressed unit) into a work directory and gives it to `tar`. `--to` needs
about the units' size ("bytes of stream" in `--list` and `--find-envelope`) and the
work directory about twice the largest slice. The work directory is made inside `--to`
(`.tapectl-work.` and some letters) for a restore, and under `$TMPDIR` (default `/tmp`)
for the modes that only read; `--work DIR` puts it on another disk. The script measures
this after it has picked the version and before it reads any slice. Free space, or
choose a larger disk with `--to` or `--work`. `--no-space-check` skips the check, for a
compressed or thin-provisioned filesystem that holds more than `df` reports.

If the disk fills anyway, the message says the work directory "cannot take it: it is
full, or not writable" and that nothing on the tape is wrong. That is not a tape or
key problem: free space (or give `--work DIR` on a larger disk) and run the same
command again with `--overwrite`, since `--to` now holds part of the unit.

**The work directory also holds the envelopes.** Everything else the script decrypts
goes there too: the operator's `catalog.db` and each unit's file table, which grow
with the number of files on the tape (a unit of millions of files has a table of
hundreds of MiB), and every envelope an `--all` run opens. `/tmp` can be a small
RAM disk, so for `--list`, `--find-envelope`, `--where` and `--find`, which keep it
under `$TMPDIR`:

```bash
TMPDIR=/mnt/big ./RESTORE.sh --list --key your-key.age.key
```

### RESTORE.sh: a slice is left out, and the restore is INCOMPLETE

```text
RESTORE INCOMPLETE: <n> path(s) are damaged or left out; restore them from another copy (the list is the one repair: give it to --paths-from on a tape that holds the same version).
```

A slice that is not on this tape (a short tape, or a unit continued on another
cartridge) or fails a check is left out; its files are listed, and everything else is
restored. **Nothing is ever filled in with zeros.** After the restore, every regular
file is hashed on disk against the unit's file table: that check, never `tar`'s exit
status, decides what was restored. The files that fail it, or were left out, are in
`.tapectl-damaged-files.txt` in the restored directory. To restore them, load another
tape that holds the same unit and version (`--where UNIT` names the tapes) and run:

```bash
./RESTORE.sh --restore --paths-from /dest/UNIT/.tapectl-damaged-files.txt --key your.age.key --to /dest/UNIT
```

Only the listed paths are restored (and overwritten). A tape of another version is
never a source. A unit continued on another cartridge asks for the next cartridge at
its own prompt, where `done` goes on with what the cartridges so far hold.

### RESTORE.sh: `NOTE: this cartridge is a tapectl volume of Layout 2`

```text
NOTE: this cartridge is a tapectl volume of Layout 2 (magic ...), label <label>; this RESTORE.sh reads Layout 3 only, so nothing is taken from it. Read it with the tapectl that wrote it or with the RESTORE.sh in its own File 2.
```

A cartridge written by tapectl 1.x has Layout 2, and Layout 3's script and tapectl
read Layout 3 only. Read it with the RESTORE.sh in its own File 2 (the bytes of that
tape's own script), or with tapectl 1.9.2, the last release that reads it.

### RESTORE.sh: `--to … is not empty`

```text
FATAL: --to /restore is not empty. Restore into an empty or new directory, or give --overwrite to replace files that are there (to finish a restore that stopped).
```

A restore over an earlier one that stopped part way would leave its truncated files
beside the new ones, so the script refuses a directory with anything in it. Restore
into an empty or new directory; to finish an earlier restore of the same unit, run the
same command with `--overwrite`. The check after the restore (every file hashed on disk
against the file table) fails the run as `INCOMPLETE` and names any file that is still
wrong. The repair, `--paths-from LIST`, refuses a directory whose damaged-file list is
not that unit's and version's.

### RESTORE.sh: every read fails with an I/O error

If every read of a tape fails at once with `Input/output error` (RESTORE.sh,
`dd`, `tar` from the drive), on a drive and cartridge that read fine before,
and tapectl 1.7.2 or later crashed or was killed on this machine just before,
the drive's Logical Block Protection check is probably still on: tapectl arms
it for its own session and clears it on every exit it gets to make (ADR-0012
item 164). While it is on, the drive expects every reader to take a CRC with
each block, so a reader that does not fails; it never returns wrong data. A
read at the start of the tape may also hang before it fails (mhvtl hangs
there for over a minute); treat that the same way.

Switch the tape drive off and on, then try again: a power cycle clears it.
Nothing on the tape is harmed. With tapectl at hand, any tapectl command that
opens a tape in the drive clears it too (`tapectl volume identify`), and
`tapectl drive status` shows whether it is on (`LBP page:  ARMED`) without
changing it. The Heir Kit's cover sheet says the same.

### RESTORE.sh: `missing required tool`

```text
FATAL: missing required tool: tail
```

RESTORE.sh checks for its tools when a mode starts, before it reads the tape, and
names the first one it does not find. Install it (the name is a command; `tail`
and `head` are in coreutils, `mt` is the package `mt-st`) and run again.
The script checks only the tools the mode you ran uses, so a host can read and
hash a tape with fewer tools than it needs to restore one:

| Mode | Tools checked (and `mt`) |
|---|---|
| `--info`, `--verify` | `dd`, `sha256sum`, `head` |
| `--find-envelope`, `--list`, `--where`, `--find`, `--restore` (whole units) | those, and `age`, `tar` |
| `--restore --path`, `--restore --paths-from` | the whole-unit set, and `tail` |

A unit stored compressed also needs the tool for its algorithm: `gzip`, `xz` or
`zstd`, checked when the unit is chosen. A bare `RESTORE.sh`, `--help` and a mistyped
mode check nothing.

### RESTORE.sh: `NOTE: only '<unit>' was restored`

```text
>>> NOTE: only 'family/letters' was restored: no --unit or --all said what to restore, so the
>>>       first envelope this key opened decided. This key opened 2 other envelope(s)
>>>       on this tape, which list 5 other unit(s) besides it.
```

You gave `--restore` no `--unit` and no `--all`, and the first envelope your key
opened lists one unit, so the script restored that one. On a tape written by 1.2.0 or
later it then reads the remaining envelopes forward (before any slice is read) and
says here what else your key opened. An operator or escrow key opens every tenant's
envelope. Look first (`./RESTORE.sh --list --key …`), then restore everything into
a new, empty directory (`./RESTORE.sh --restore --key … --all --to /another/destination`).
The note is silent when the other envelopes add no units. The script skips the look,
and says so, when its work directory could not hold the largest remaining envelope
(`Not looking at the other envelopes: the work directory has … free`); set `TMPDIR`
to a larger disk to see it. `--no-space-check` does not override that skip.

### RESTORE.sh: `this tape requires feature(s) …`

```text
FATAL: this tape requires feature(s) short-seal, which this script does not know
```

A tape written by a newer tapectl says in File 0 (and its seal marker) what a
reader must understand, as `requires = [...]`. A RESTORE.sh or tapectl that
does not know a feature refuses rather than misread the tape. Use the
RESTORE.sh on that tape (file 2), which knows, or a newer tapectl. tapectl
itself refuses the same way, before a scratch directory, a key or a slice:
`restore raw-volume` and `catalog rebuild`, and from the release after 1.2.0
(#476) also `restore unit`, `restore file`, `restore volume`, `volume verify`,
`volume read-slices`, `volume compact-read`, `volume compact` and `volume
resume`. `volume verify` exits 3 and records nothing for a File 0 it refuses,
and reports a seal that needs an unknown feature as unreadable. Either way it
is inconclusive, never a reason to quarantine the cartridge.

### RESTORE.sh: an envelope is `DAMAGED`

A key matched the envelope (age got past its header) but the payload would
not decrypt, or the envelope's bytes do not match the front index. That is
damage, not a wrong key: another key will not help. (A RESTORE.sh from a tape
written by 1.2.0 or later says "a key matched" only when one did. When the envelopes
could not be read or failed their hash and no key opened any of them, it says so
and suggests other keys: it is then damage, a key that is not a recipient, or
both.) The operator envelope has
a backup copy on every tape, which an operator or escrow key opens
(`--find-envelope` tries it); otherwise use another copy of the volume. Since 1.1.0
every tenant envelope has a backup too.

`catalog rebuild` treats a pair that is unreadable in both copies by kind (#451).
The operator envelope's pair lost: the rebuild refuses (it names every unit and
carries catalog.db). A tenant envelope's pair lost on a tape whose catalog.db
names each unit's owner (written after 2026-09-11): the rebuild goes on, warns
naming the envelope and its copies' positions, and records every unit (`--json`:
`unreadable_tenant_envelopes`). On an older tape, or one with no catalog.db, the
tenant envelopes are the only record of ownership, so the rebuild refuses; use
another copy of the volume. In every case RESTORE.sh (tape file 2) with the
owner's key or the escrow key restores what still opens.

### RESTORE.sh: `this tape is layout_version N`

```text
FATAL: this is a tapectl volume of Layout 2 (magic ...), label <label>. This RESTORE.sh reads Layout 3 only.
       Read it with the tapectl that wrote it, or with the RESTORE.sh in its own File 2: ...
```

A RESTORE.sh reads the layout it was written with. Every tape carries its own
script at file 2, so read that one: `mt -f <dev> rewind && mt -f <dev> fsf 2`,
then `dd if=<dev> bs=512k | tr -d '\0' > RESTORE.sh`. A tape whose File 0
states no layout_version at all (a damaged ID thunk) only warns, and is read
as a Layout 3 tape.

### `catalog rebuild`: `the catalog's stage set is not the one on this tape`

```text
error: slice <n> of stage set <id>: the catalog records ciphertext sha256 <sha> but this tape carries
<sha> — the catalog's stage set is not the one on this tape; the rebuild was rolled back
```

A Version staged twice (for example, re-staged for a later copy) has two stage
sets, and because each staging encrypts afresh, their slices differ byte for
byte even though the content is the same. `catalog rebuild` tells them apart by
the first slice's ciphertext hash: a tape whose first slice matches no stage
set of that Version gets a stage set of its own. This refusal means the first
slice matched but a later one did not, so the catalog's record of that stage set
contradicts the tape. The rebuild's own changes are rolled back: no volume,
stage set, write or file row is added or changed. Two things stay, both
recorded before the rebuild began: the drive contact, and — when the volume is
already in the catalog, bound to a cartridge with no serial recorded — the
serial the drive reported, learnt onto that cartridge (the `note: cartridge
... learnt <serial>` line says so). Verify the tape (`tapectl volume verify <label>`), which
checks the tape against its own front index; if it passes, the catalog's rows
are the ones in doubt.

### `catalog rebuild`: `THE SEAL MARKER COULD NOT BE READ`

```text
  warning: volume "<label>": THE SEAL MARKER COULD NOT BE READ, which is not the same as no seal: <why>
    recorded as an initialized volume with its writes "aborted" ... and QUARANTINED ...
```

The seal position failed to read (a read error, or a medium whose end is not
known), which is not the same as the tape having no seal: it may carry a sealed
volume that the drive could not read just then (ADR-0012 #277). The rebuild
records the volume the way it records any tape it cannot call sealed (`initialized`, writes
`aborted`, positions `pending`, so nothing counts it as a copy), and also
quarantines it, with a `seal_unreadable` event, so `volume write` and
`volume resume` refuse it and say why; no flag passes that refusal. Do not write
over the tape. What works:

- read it again, with another drive or after cleaning this one, and run
  `tapectl catalog rebuild --from-volume` again into the same catalog: a rebuild
  that reads the seal cleanly finishes the record (ADR-0012 item 302). The
  volume becomes `sealed`, its quarantine is cleared, its writes `completed` (or
  `range`) and its positions `written`, as a first rebuild of the sealed tape
  records them, and a `seal_promoted` event says so. Do this before anything
  else touches the row;
- `tapectl volume verify <label>` checks the tape against its own front index; a
  clean full verify clears the quarantine, and does not seal the row. It is an
  event on the volume, so a verify run first puts the row out of the rebuild's
  reach (see below): rebuild first, verify after;
- restore what it holds: `tapectl restore unit`, or RESTORE.sh at tape file 2 with a
  tenant key or the escrow key;
- only if the tape is to be given up: `tapectl volume retire <label>`, then
  `tapectl cartridge erase`, which asks for the tape's label.

A later rebuild that reads the seal cleanly finishes that record, and only that
one: the volume's row must still be what the earlier rebuild left (`initialized`
and quarantined, with its `seal_unreadable` event), no event on the volume may
have come after it but a rebuild's (a write, resume, verify, retire or edit each
log one), and its writes must still be `aborted` and its positions `pending`.
Every other row a rebuild finds is left alone (a rebuild never changes a status
it finds, #158, #360). When the row has been changed since, the rebuild warns
that the volume is still `initialized` with `aborted` writes: `volume resume`
refuses that state, since no seal was ever recorded, and `volume write` is not
the way on, because the tape carries a sealed volume. Rebuild into a catalog
that has no row for the label to record it as `sealed`.

### `catalog rebuild`: `NO VALID SEAL MARKER`

```text
  warning: volume "<label>" has NO VALID SEAL MARKER: <why>
    recorded as an unsealed volume: status "initialized", its writes "aborted" and its slice positions "pending" ...
```

The rebuild reads the seal marker before it says a tape is sealed (ADR-0012,
#458). The seal must read at File 0's `seal_marker` position, parse, name this
volume, and bind the front index beside it. Without one the tape is an
interrupted or aborted write: its envelopes and front index are laid down before
its slices, so the catalog can describe slices that were never written. The rows
are recorded as the catalog records any aborted write (`initialized`, writes
`aborted`, positions `pending`), so the data is findable and nothing counts it as
a copy in `audit` or `report copies`. A full `volume verify` cannot seal it.
Three ways out (the seal was read and is absent, so the tape holds no sealed
volume; a seal that did not READ is the section above, and none of these):

- write over it: `tapectl volume write <label>` starts again at the beginning of
  the same tape;
- reuse the cartridge another way: `tapectl volume retire <label>` (no copy is
  lost; it prompts for nothing), then `tapectl cartridge erase`;
- read what it holds: RESTORE.sh at tape file 2 with a tenant key or the escrow
  key.

Do not `volume verify` it first: a verify records its failures against those
positions, and `volume write` will not delete positions that hold results. A seal
marker that requires a feature this tapectl does not know is not "unsealed": the
rebuild refuses it, by name, and the tape is read with a newer tapectl or its own
RESTORE.sh. `--json` carries `seal_valid`, `unsealed_reason` and
`recorded_as_unsealed`.

---

## Audit findings

[`audit`](cli/audit.md#tapectl-audit) compares the catalog with each unit's
resolved policy. It is advisory: it never blocks anything. It exits 0 when
clean, 1 when there are only warnings, and 2 when there is any violation.
Before any tape, with two units staged and one only snapshotted:

```text
$ tapectl audit
VIOLATIONS (3):
  [copy_count] family/invoices: has 0 copies, needs 2: never written to tape (v1 snapshotted, not yet staged)
  [copy_count] family/letters: has 0 copies, needs 2: never written to tape (v1 staged, awaiting a write)
  [copy_count] family/photos: has 0 copies, needs 2: never written to tape (v1 staged, awaiting a write)
audit: 3 violations, 0 warnings (exit 2)
for the fixes: tapectl audit --action-plan
```

`--action-plan` adds an `ACTION PLAN` block after the findings: one `fix:`
line per distinct remedy, built from the current state of your archive, naming
the units it is for. A remedy several units share is one line (1.9.0):

```text
$ tapectl audit --action-plan
VIOLATIONS (3):
  …
ACTION PLAN (2 fixes):
  fix: tapectl snapshot create family/invoices && tapectl stage create family/invoices && tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <location> — for family/invoices
  fix: tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <location> — for 2 units: family/letters, family/photos
audit: 3 violations, 0 warnings (exit 2)
```

After a first tape, each unit reads `has 1 copy, needs 2`, and once a volume is
sealed the audit warns until the Heir Kit is made:

```text
WARNINGS (1):
  [escrow_kit_missing] archive: 1 sealed volume exists but no heir kit has ever been generated — nothing off-site can decrypt it
```

`--json` gives the same findings as objects with `severity`, `unit`, `check`,
`message` and `action`.

The per-unit checks cover every unit, whatever its status (`active`,
`tape_only` or `missing`), except `dirty`, which looks only at `active` ones
(and `tenancy` reads the dotfile of an `active` unit only). The archive-wide checks (`compaction_candidate` and every row after it
below) run only for a whole-archive audit, not with `--unit`. The `short_seal` family's findings name a volume, as
`volume:<label>`.

| Check | Severity | Message | Meaning and fix |
|---|---|---|---|
| `copy_count` | violation | `has <n> copies, needs <m>` (`has 1 copy` for one); at 0 copies with no current version on tape, it ends with what there is instead: `: never archived`, `: never written to tape (v<n> snapshotted, not yet staged)`, `(v<n> staged, awaiting a write)` or `(v<n> was staged, and its staging released unwritten)` | Fewer sealed, in-service copies of the unit's current Version than its copy floor (the larger of its `min_copies` and its `copies_at` sum). A unit is as covered as its least-covered live Version. From 1.7.0 a Version held across cartridges is as covered as its least-covered slice: a tape holding a range of it counts only together with tapes holding its other slices (ADR-0012 item 127). For a tape-only unit `<m>` is the copy floor times `[compaction] tape_only_safety_multiplier`, and the message ends `(tape-only <k>x)` (ADR-0012, 2026-10-12). From 1.9.0 a unit no tape holds yet is reported here once, worded as above, and `no_archive` stays quiet. Fix: write another volume. For a unit never written, the `fix:` line is the one copy loop, `tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <location>`, after a `snapshot create` and `stage create` where nothing is staged; for a further copy it is `volume init` then `volume write`, and it re-stages or `read-slices` first when the staged data is gone. |
| `location_presence` | violation | `no copy at required location <name> (policy requires <list>; copies are in <n> locations)` (`locations <names>` for several) | A location the policy names in `required_locations` holds no copy of the unit's current Version (checked by name; a name that is not a registered location counts as missing). Fix: write another copy and move it there. The `fix:` line ends `&& tapectl volume move <OTHER-LABEL> --to <location>`, and says to repeat it, one new copy each, when several locations are missing. For a tape-only unit each named location needs `[compaction] tape_only_safety_multiplier` copies; one short of it reads `has <n> copy at required location <name>, needs <m> (tape-only <m>x) (policy requires <list>)`. |
| `location_count` | violation | `has copies in <n> locations, needs <m> ([defaults] min_locations)` | A tape-only unit's current Version sits in fewer distinct places than `[defaults] min_locations` (the fewest across its current Versions; a recorded warehouse deposit is a place; the multiplier never raises it, so there is no `(tape-only Nx)` note). Fix: write another copy, or move one to a place that holds none, as the `fix:` line spells out; it ends `&& tapectl volume move <OTHER-LABEL> --to <OTHER-LOCATION>`. |
| `warehouse_copies` | violation | `has <n> warehouse deposits, needs <m>` | The policy asks for cold-cloud deposits (`warehouse_copies > 0`). Fix: the deposit procedure in the [operator guide](operator-guide.md#warehouse-copies-cold-cloud), then `volume deposit add`. |
| `encryption` | violation | `<n> unencrypted stage sets on tape, policy requires encryption` | Something on tape was written unencrypted (only possible with data from very old versions). Fix: re-stage and rewrite, as the `fix:` line spells out. |
| `policy_unresolvable` | violation | `policy could not be resolved (<why>); copy_count/location_presence/verify_age/encryption checks were SKIPPED for this unit` | The unit's policy chain (dotfile > archive set > `[defaults]`) is broken, so **no** check ran for it. The `fix:` line names the layer at fault: the unit's `.tapectl-unit.toml`, its archive set, or `[defaults]`. |
| `dirty` | warning | `source has drifted since last archive (<a> added, <r> removed, <m> modified)` | The source moved on after its last snapshot. This is routine. Fix: `snapshot create`, `stage create`, `volume init`, then `volume write`, as the `fix:` line spells out. |
| `dirty` | violation | `dirty scan could not run (<error>)` | tapectl could not even read the source to compare it (permissions, a missing path). Fix the access or the unit's dotfile. |
| `no_archive` | warning | `never archived`, or `never written to tape (…)` as `copy_count` words it | A unit no tape holds, reported here only where `copy_count` cannot judge it: a copy floor of 0, or a unit whose policy could not be resolved (`policy_unresolvable`). Any other unit never written is `copy_count`'s, at 0 copies, so it is reported once (1.9.0). Fix: as the `fix:` line spells out, the one copy loop after a snapshot and a stage where nothing is staged. |
| `verify_age` | warning | `not verified within <d> days (last: <date or never>)` | Always: 365 days unless the unit's archive set or `[defaults] verify_interval_days` says otherwise (1.2.0; `tapectl config check` prints the value in force). A unit with a copy and no full readback at all shows `(last: never)`, and so does every unit just written with a quick confirm: that is expected, and it clears when a full `volume verify` passes (or the write used `--full-confirm`). Only a full readback counts, dated by when it started (a continued one by its oldest checkpoint). Fix: `volume verify <LABEL>` on a volume holding it. Only a readback of a volume that still counts as a copy (sealed, condition ok, with a completed write of the unit, or from 1.7.0 a range) refreshes the clock (#454). From 1.7.0, for a Version held across cartridges, each slice is dated by the newest full readback among the volumes holding it and the Version by its oldest slice, so a range read yesterday says nothing about the slices on another tape: verify every tape of the span. |
| `escrow_coverage` | warning | `volume <label> (stage set <id>): <reason> — the current escrow key cannot recover it` | That copy cannot be opened by the escrow key. `coverage unknown …` means a catalog rebuilt from a tape with no recipient list; `catalog rebuild --key <escrow key>` attests it. Otherwise re-stage and rewrite, or accept that only the original recipients can open it. |
| `tenancy` | warning | `the catalog says tenant "<t>" owns this unit, but <dir>/.tapectl-unit.toml names tenant "<other>"` | The unit's dotfile names another owner than the catalog. The catalog is the authority: set `tenant` under `[unit]` in the dotfile to the catalog's tenant. The dotfile is not content, so the edit creates no new version. |
| `tenancy` | warning | `volume <label> (stage set <id>, v<n>) is encrypted to none of tenant "<t>"'s keys — …` | The catalog holds keys for that tenant, and none of them is a recipient of that copy. A retired key of the tenant counts as one of its keys. Two causes: the copy was encrypted before the unit moved to its current tenant (`tenant reassign`), or the catalog was rebuilt from tape and the tenant's key that opens the copy was never imported. Either way the operator and escrow keys still open it. Fix: if the tenant has a key the catalog does not list, [`key import`](cli/key.md#tapectl-key-import) its public key and run `audit` again. Otherwise re-stage and write a new copy if the tenant's key holders need it, or accept it. A tenant the catalog holds no key for at all (a rebuilt catalog before any `key import`) is not judged, so this check reports nothing for its copies. |
| `compaction_candidate` | warning | `live data is <n>% of the archive data on this volume (<live> live, <r> in reclaimable/purged snapshots), below the <t>% compaction threshold` | Part of the volume's data belongs to snapshots marked reclaimable or purged, and the live share has fallen below `[compaction] utilization_threshold`. Only data slices count, never the fixed per-volume metadata, and a volume with nothing reclaimable is never a candidate. Fix: [compaction](operator-guide.md#compaction), starting with `volume compact-read <label>`. |
| `escrow_kit_missing` | warning | `<n> sealed volumes exist but no heir kit has ever been generated — nothing off-site can decrypt them` (`1 sealed volume exists …` for one) | Fix: `tapectl key escrow-kit <dir>`, then do the paper steps it lists ([keys-and-recovery.md](keys-and-recovery.md)). |
| `escrow_kit_stale` | warning | `` <n> volumes were sealed after the last heir kit (<date>): the kit's escrow secret still opens them (it is a recipient of every tape), but the kit's encrypted catalog (catalog.db.age) does not list them — regenerate the kit, or rebuild the catalog from those tapes with `tapectl catalog rebuild --from-volume` `` | Only the kit's catalog (and the cover sheet's list of places) is behind; its secret opens every tape. Two causes, one finding: a volume sealed after the kit, or a volume moved with `volume move` / `cartridge move` after it; for a move the message adds `<n> volumes were moved to another place after the last heir kit` (#448). Fix: `tapectl key escrow-kit <dir>`, reprint COVER.txt, correct its "Tapes are at" box, and replace the stored copies. The escrow secret itself does not change. |
| `escrow_identity_mismatch` | warning | `<n> stage sets the current escrow key cannot open all name a recipient this catalog does not recognise: <key> — …` | Typical after a disaster rebuild that created a **new** escrow identity. No command replaces a registered escrow identity. The fix is to re-initialise a fresh home with `tapectl init --escrow-public-key <original>` (from the heir kit) and run `catalog rebuild` again. |
| `no_full_verify` | warning | `volume "<label>" is sealed with no full readback recorded — its write confirmed the front index and seal only` | A write's confirm reads back only the front index and the seal marker unless `--full-confirm` was given (ADR-0012, 2026-10-06), so this volume counts as a copy with none of its data read back yet. Fix: `tapectl volume verify <label>` (full, the default); a passing one clears it, a `--quick` one does not. |
| `volume_verify_age` | warning | `volume "<label>" (cartridge <barcode>, at <place>) was last read back in full on <date>, <n> days ago: past the <d>-day verify interval of the unit on it with the shortest one, whatever the unit's other copies show` | The per-volume clock (#454): `verify_age` is satisfied by the newest readback of any copy, so a fresh home copy would hide a stale offsite one. The interval is the shortest resolved `verify_interval_days` among the units on the volume (an archive set's, else `[defaults]`, 365 days when that is absent). A volume never read back in full is `no_full_verify`'s alone. Fix: fetch the cartridge from the place named and run `tapectl volume verify <label>`. |
| `tape_alert` | warning | `volume "<label>": the drive raised a TapeAlert — <n> <flag>; … — at <time> during "<command>" with cartridge <barcode> loaded on drive <serial>` | A contact with this volume, or with the cartridge it is on (a [`drive poll`](operator-guide.md#the-daily-drive-poll) included), read page 0x2E with a flag raised, not before the volume's last passed full verify started (an alert raised during a readback, its own included, is not answered by it). Retired, erased and quarantined volumes are not named; an `initialized` one an interrupted write left is. A warning, never a violation: a flag may be the medium ("Media", "Read failure") or only the drive ("Cleaning required"), and the volume still counts as a copy. A cleaning flag (TapeAlert 20-23) is printed with its action, `<n> <flag> => clean the drive: …` (22 and 23 name the cleaning cartridge instead); the fix column is still the verify, which is what clears the warning. Fix: `tapectl volume verify <label>` — a pass clears it; a failure quarantines the volume and the copy checks take over. `report health` lists every sighting. |
| `completed_past_ending` | warning | `` volume "<label>" ended after tape file <k>, but <n> writes on it recorded `completed` have a slice past that (up to file <m>): those units are not on this tape whole, yet they count as copies `` | A `short_seal` invariant (1.6.0). A unit with a slice past where the tape ended is not on it whole, but the catalog counts it as a copy, so its copy count is too high. Confirm, `catalog rebuild` and the adoption of a lost write never leave this, nor any of the rows below: each means the catalog was edited around tapectl, or a bug. Fix: `tapectl volume verify <label>` (the `fix:` line): a full verify reads the tape's seal and names any disagreement with the catalog (`ended_after_disagrees`); report the finding. |
| `completed_without_every_slice` | warning | `` volume "<label>" has <n> writes recorded `completed` that lack a `written` position of some slice of their set: a `completed` write is a copy of every slice, 1 to N, so those units count copies that do not exist `` | A `short_seal` invariant (ADR-0012 item 288: a set is `completed` only when every slice of it is on the tape). Fix: as for `completed_past_ending`. |
| `range_written_past_ending` | warning | `` volume "<label>" ended after tape file <k>, but <n> writes recorded `range` on it record a slice `written` past that (up to file <m>): those slices are not on this tape, yet they count as copies of those slices `` | A `short_seal` invariant (1.7.0). Confirm records a range's slices past the tape's end as `pending`, never `written`. Fix: as for `completed_past_ending`. |
| `range_not_one_run` | warning | `` volume "<label>" has <n> writes recorded `range` whose `written` slices are not one unbroken run: a volume holds at most one unbroken range of a stage set, so this catalog does not say which slices the tape holds `` | A `short_seal` invariant (1.7.0, ADR-0012 item 122). Fix: as for `completed_past_ending`. |
| `range_without_written_slice` | warning | `` volume "<label>" has <n> writes recorded `range` with no slice `written`: a range is the slices the tape holds, and a write whose tape holds none of its set has no row `` | A `short_seal` invariant (1.7.0). Fix: as for `completed_past_ending`. |
| `span_in_progress` | warning | `` v<n> is a span still in progress: slices <runs> of <total> are on no sealed volume yet, and <label> (slices <a>-<b>) holds the rest. The version stays `staged`, and counts no copy, until every slice is on a sealed volume `` | From 1.7.0 (ADR-0012 item 128). A Version of a unit that spans cartridges is on tape in part: some of its slices are on sealed volumes, others are not yet on any. That is the normal state between two tapes of a span, not damage. Until the last slice is on a sealed volume the Version stays `staged`, `copy_count` does not count it, and the retire floor protects the tapes that hold its only copy of a slice. With several holders the message lists each with its run and says they "hold the rest"; with none, "no sealed volume holds a written slice of it". Fix: load a fresh cartridge and run `tapectl volume write <LABEL>`, as the `fix:` line says: it writes the slices the span still owes first. |

---

## What staging holds: `status` and `stage list`

A stage set stays in staging after its writes on purpose, so it can become the
next copy. From 1.9.0 each write releases every unit whose copy floor is then
met, unit by unit, and keeps the rest; `--keep-staging-for-locations` keeps a
unit until every `copies_at` target is met too. So a unit still in staging after
a write is one that still owes a copy, or one a write was told to keep.

[`status`](cli/status.md) is the home screen. It counts the units by state and
names the next command:

```text
$ tapectl status
home: ~/.tapectl (named by TAPECTL_HOME)
session logs: ~/.tapectl/logs
running: nothing
…
units: 3 active
  State          Units
  pending        0
  staged         2, awaiting a write
  under-copied   3, below their copy floor
volumes due a verify: none
next:
  tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <a short location>
```

[`stage list`](cli/stage.md#tapectl-stage-list) lists the stage sets held, with
the copies of each set's version against its unit's copy floor; `--owing` lists
only those that still owe a copy, and `--all` adds the released and failed ones:

```text
$ tapectl stage list
Stage Set   Unit             Version   Status   Slices   Size      Copies   Staged
1           family/letters   v1        staged   1        1.8 KiB   0 of 2   2026-11-05 02:38 UTC
2           family/photos    v1        staged   1        1.9 KiB   0 of 2   2026-11-05 02:38 UTC
```

A set that is held and owes nothing was kept on purpose (`--keep-staging`, a
location target, a rewrite's hold); a bare `tapectl stage clean` releases what
is already on enough tapes and keeps the rest.

`status` replaced `report summary`, and `stage list --owing` replaced `report
pending`; both old spellings are refused by name
([renamed](#tapectl-old-was-renamed-in-190-use-tapectl-new)). A script that read
`report summary --json` reads `status --json` now, a different document: the
home, the running sessions, `units` by state, `due_verify` and `next`. The
operator guide lists every `--json` field that changed, under
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
the next, except the profile: a run without `--profile` on a host that recorded
one refuses and names it ([install.md §2](install.md#2-the-one-command)). Without `--profile` or
`--device`, the drive is lost. Step 6 is the
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
- **Step 13, `staging failed (see <file>)`.** Step 13 stages every unit still
  to stage with one `stage create`, `[staging] jobs` at a time. The file it
  names holds that command's output; the error at its end names the unit that
  failed, the units staged before it stopped (they stay staged) and any not
  started. When the staging directory may be too small for a unit
  ([Staging space](#staging-space-asked-about)), `stage create` asks on your
  terminal before it goes ahead; run without a terminal on stdin, the
  question cannot be asked and the stage is refused, with the figures above
  the stop. To accept them, stage that unit yourself, then re-enter with the
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
  correct tool version was reported as the wrong one. Fixed. Such checks now read
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
