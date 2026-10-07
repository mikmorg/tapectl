# Installing tapectl — the runbook

Written for a **reinstall from a bare machine** (ADR-0012, 2026-09-24 amendment,
item 6: "installation is a formal, repeatable procedure, so a reinstall is
easy"). It says what the one entry point creates on the host, how to resume it,
how to re-run it over a home that already exists, how to get the catalog back,
how to move to another host, and how to take it all off again.

It does not repeat the operator's manual: `docs/operator-guide.md` is what to
do *with* an installed tapectl, and [Keys and recovery](keys-and-recovery.md) is
the authority on catalog and data recovery (the operator guide's "Disaster
Recovery" section is its short form) — this document points at them rather than
restating them. `CONTEXT.md` is the vocabulary.

Every `tapectl` flag below was checked against the binary's own `--help` on
2026-09-29. Every path below is the default; `first-run.sh --home`, `--user`
and `--backup-dir` move the ones they name, and the script prints the
effective values at its start.

Facts specific to one machine live in its **host profile** (§2, "Host profiles"):
the production host, home2, is described by `contrib/hosts/home2.profile` and prepared
by `contrib/hosts/home2-prep.sh` (ADR-0012, 2026-09-28 amendment).

---

## 1. Prerequisites

| you need | why | how it is checked |
|---|---|---|
| a Debian/Ubuntu host with `sudo` | only steps 1 and 4 run without it. Root: packages (2), `/usr/local/bin` (3), the service user (5), group membership (6, and 12 for you), the staging directory (7), ACLs (11), the drive's MAM read (12), systemd (14), and handing the kit directory to the service user and back (9, 13). Steps 5–11 and 13 run every `tapectl` command through `sudo -u tapectl -H`, and the pre-step-1 config check reads the home that way once the service user exists; step 12 runs the installed binary as you | the script uses `sudo` per command, never runs as root itself |
| the tape drive attached, visible under `/dev/tape/by-id/` | step 6 finds it **by serial**; `/dev/nstN` numbers move across reboots on any host with more than one SCSI device (`docs/lto6-drive-passthrough.md`) | `ls -l /dev/tape/by-id/` |
| a clone of this repository, on a filesystem the build can use | tapectl is built from source; the toolchain is pinned in `rust-toolchain.toml` (1.94.1) | step 1 |
| `rustup` (installed by step 1 if absent) | the distro `cargo` cannot build this crate | step 1 |
| `dar` >= 2.6 (2.7.20+ recommended), `mt-st`, `sg3-utils`, `acl`, `python3`, `lsscsi` | archives, drive control, MAM and health pages, ACL grants, the scripts' JSON parsing, device discovery | step 2 offers `sudo apt install dar mt-st sg3-utils acl python3 lsscsi` |
| `age` (the CLI) — optional | not used by the binary (it uses the `rage` crate); needed by the on-tape `RESTORE.sh` and so by the step-12 rehearsal | step 2 offers the upstream v1.3.2 release into `/usr/local/bin` |
| **paper and a pen** | step 7 prints the escrow *secret* exactly once and stores it nowhere (ADR-0005) | the script asks "paper ready?" before `init` |
| on a **reinstall**: the original heir kit's cover sheet | so `init` adopts the original escrow public key instead of minting a new one — no command replaces a registered escrow identity | step 7 asks for it, but only without `--auto`: no flag or profile variable carries the key, so an `--auto` run mints a new identity (§6) |
| a second disk for catalog backups | the ruling says "a timer-driven `db backup` to a second disk" (ADR-0012, item 5) | step 14 warns when the backup dir shares a filesystem with the home |
| a test cartridge you are willing to erase | the step-12 rehearsal is required before the first production write (ADR-0012, 2026-09-23) | step 12 asks for its medium serial and refuses any other cartridge |
| free space for staging | a tape is written in one session, so staging must hold everything one tape will carry (2.5 TB for a full LTO-6) | step 7 asks for the directory, then shows what is free there |

Staging and backups need thought on any host whose root filesystem is small. Step 7
proposes the directory `config.toml` already names when the service user can write
it, and after a fresh `init` that is `<home>/staging`, on whatever filesystem holds
the home. Only when that directory is missing or not writable (a regenerated config,
a home from an older tapectl) does it propose `/scratch/tapectl-staging`, if
`/scratch` exists. Pass `--staging` (or set `STAGING_DIR` in a profile) to put it on
a big disk; that wins over both. See §7 for `--backup-dir`.

---

## 2. The one command

```bash
cd ~/git/tapectl
scripts/first-run.sh
```

Interactive, in fourteen steps, each explained before it runs. Every step
detects whether it is already done and says so; every tape-touching command
is confirmed by name; `--from N` resumes and `--to N` stops. Before step 1,
on every run, the script prints the effective settings and checks an existing
home's `config.toml` for the two `[defaults]` keys this version renamed (§6).
`--help` prints the step list and every option. Flags you will want:

```bash
scripts/first-run.sh --device /dev/tape/by-id/scsi-<SERIAL>-nst   # skip the interactive pick
scripts/first-run.sh --backup-dir /mnt/backup/tapectl               # the second disk (step 14)
scripts/first-run.sh --from 7 --device /dev/tape/by-id/scsi-<SERIAL>-nst          # resume at step 7
scripts/first-run.sh --from 12 --to 12 --device /dev/tape/by-id/scsi-<SERIAL>-nst  # just the rehearsal
scripts/first-run.sh --no-service-user                              # run tapectl as yourself (single-user box)
```

A run that starts after step 6 and reaches step 8, 12 or 13 needs `--device`
(or a profile that sets `DEVICE`): the drive is not remembered between runs (§5).

`--auto` takes the default for every *non-destructive* prompt; a destructive
prompt under `--auto` counts only when the word it asks for came from a flag
(`--label`, `--barcode`) — a default is never consent to erase. The script logs
to `~/.local/state/tapectl/first-run.log` (yours, never inside the service
user's home); step 7's `init` output is deliberately not logged.

### Host profiles

A host's answers — its drive, where the home, staging and backups go, which dar,
its locations, tenants and collections — can live in a profile instead of on the
command line:

```bash
scripts/first-run.sh --profile contrib/hosts/<host>.profile
```

The profile is a bash file of variables, sourced before the flags are parsed, so
any flag still wins. Beyond the flags' own values (`DEVICE`, `OPERATOR`,
`BACKUP_DIR`, `KIT_OUT`, `LABEL`, `TEST_BARCODE`, …) it can set:

| Variable | Used in | What it does |
|---|---|---|
| `WORK_DIR` (`--work-dir`) | 3, 4, 12 | the build lock and the rehearsal's throwaway homes; default `/scratch` if it exists and is writable, else `~/.cache/tapectl` |
| `STAGING_DIR` (`--staging`) | 7 | the default for `staging.directory` |
| `DAR_BIN` (`--dar`) | 2, 4, 7, 12 | the dar that is checked, tested against, and written into `[dar] binary` |
| `SVC_HOME_WANT` | 5 | the service user's home; step 5 refuses if an existing account's home differs |
| `LOCATIONS=("name\|description" …)` | 10 | every shelf; tape 1 is recorded at the first |
| `TENANTS=("name\|description" …)` | 11 | non-operator tenants, created if missing |
| `COLLECTIONS=("name\|root\|tenant\|depth" …)` | 11 | granted by ACL, written as `[[collections]]`, then `collection sync` |
| `CONTENDER_UNITS=(…)` | 13 | the systemd units `host check` treats as contenders; unset means the dev VM's homorg timers |

**home2** (the production host, ADR-0012's 2026-09-28 amendment) has a one-time
preparation first, dry-run by default:

```bash
contrib/hosts/home2-prep.sh            # prints what it would do
contrib/hosts/home2-prep.sh --apply    # does it, asking before each part
tmux new -s tapectl
scripts/first-run.sh --profile contrib/hosts/home2.profile --to 6
scripts/first-run.sh --profile contrib/hosts/home2.profile --from 7 --to 12   # paper ready for the escrow secret
tmux clear-history                     # the secret is in the scrollback
# unload the test cartridge, load a production one
scripts/first-run.sh --profile contrib/hosts/home2.profile --from 13
```

The prep's last part installs `/etc/sudoers.d/tapectl-operator`, which lets the
operator run commands *as the service user* (and only as it) without a password.
Every `tapectl` call in `first-run.sh` outside the step-12 rehearsal goes through
`sudo -u tapectl`, and on a host whose sudo asks for a password the verify that
follows a days-long write would otherwise wait at a prompt until sudo gives up.


---

## 3. What each step creates on the host

Steps 1–4 run as you. Steps 5–11 and 13 run every `tapectl` command as the
service user through `sudo -u tapectl -H`; the script has one seam for that
(`as_svc`) and `--no-service-user` turns it off. Step 12 runs as you: the
lifecycle suite needs your login to open the drive, and it runs the installed
binary (`TAPECTL_BIN`), so what it proves is what step 13 uses.

| step | creates | owner, mode | check |
|---|---|---|---|
| 1 Rust toolchain | `~/.cargo/` for **you** (rustup, default profile, `--no-modify-path`), the pinned toolchain on first `cargo` run | you | `rustup show active-toolchain` |
| 2 runtime tools | apt packages `dar mt-st sg3-utils acl python3 lsscsi`; optionally `/usr/local/bin/age` and `age-keygen` | root, 0755 | `dar --version`, `age --version` |
| 3 build | `target/release/tapectl` in the repo, then **`/usr/local/bin/tapectl`** — the binary's home; the service user cannot execute anything under your home | root, 0755 | `tapectl --version` |
| 4 tests | nothing on the host (`cargo test`, ~2–3 min, needs only `dar`) | — | the script stops if red |
| 5 service user | system account **`tapectl`**, home `/var/lib/tapectl`, shell `/usr/sbin/nologin`, comment "tapectl archival service" | — | `id tapectl` |
| 6 the drive | `tapectl` added to the group owning `/dev/nst*` and `/dev/sg*` (usually `tape`) via `usermod -aG` | — | `sudo -u tapectl -H mt -f <by-id> status` |
| 7 the home | **`/var/lib/tapectl/.tapectl/`** (§4), the escrow identity (secret printed once), the operator tenant and its keys (`init --operator <name>`: run as a system account such as the service user, `init` refuses to guess the operator name from the account); the staging directory (asked; the proposal is `--staging`/`STAGING_DIR` when set, else the directory `config.toml` names if the service user can write it — `<home>/staging` after a fresh `init` — else `/scratch/tapectl-staging` when `/scratch` exists), written into `[staging] directory`, created with `sudo mkdir -p` when missing and offered a `chown tapectl` when the service user cannot write it. With `OPS_GROUP` in the profile: that group (`groupadd --system`), the service user and you added to it, and `[ops] group` in `config.toml` (issue #393: members run `tapectl --home <home> status` without sudo) | home 0700 `tapectl` (0710, group `OPS_GROUP`, with an ops group; `logs/` 2750); staging `tapectl` | `sudo -u tapectl -H tapectl config check`, `db fsck` |
| 8 backend | a `[[backends.lto]]` table appended to `config.toml`: `device_tape` (by-id), `device_sg`, `generation` (from the drive's INQUIRY product id, never from the loaded cartridge — ADR-0010) | in `config.toml` | `tapectl config show` |
| 9 heir kit | `~/heir-kit/` (**yours**; `--kit-out` moves it): `COVER.txt`, `escrow-kit.html`, `catalog.db.age` | you, 0700 | print `COVER.txt`, hand-write the secret on it, seal, two failure domains |
| 10 location | a `locations` row (e.g. `home-rack`) | in the catalog | `tapectl location list` |
| 11 tenants, units | tenant rows and keys under `keys/`; per unit: a POSIX ACL grant (`setfacl -R -m u:tapectl:rX` on the tree, the same as a default ACL so new files inherit it, `rwX` on the top directory only, `x` on each ancestor) and a **`.tapectl-unit.toml`** dotfile in the unit's top directory | files keep their owner and mode | `getfacl <dir>`, `tapectl unit list` |
| 12 rehearsal | the lifecycle suite's run directories under `<work dir>/tapectl-lifecycle` (`--work-dir`: `/scratch` when it is writable, else `~/.cache/tapectl`; home2's profile sets the latter); on green, the marker **`~/.local/state/tapectl/rehearsal-ok-<sha256 of the binary, 16 hex>`**, which step 13 requires for that exact binary. May add **you** to the `tape` group (the suite runs as you) | you | the marker exists |
| 13 first tape | the volume, its cartridge row (auto-registered from the chip serial), the write and verify records, a refreshed heir kit; per-run capture files under `~/.local/state/tapectl/` | — | `tapectl volume list`, `report verify-status` |
| 14 timers, wrapper | via `scripts/install-systemd.sh` (§7): `/etc/systemd/system/tapectl-audit.{service,timer}`, `/etc/systemd/system/tapectl-backup.{service,timer}`, `/usr/local/lib/tapectl/tapectl-scheduled-{audit,backup}.sh`, the backup dir (**`/var/backups/tapectl`** by default), **`/usr/local/bin/tapectl-op`** | units root 0644, scripts root 0755, backup dir `tapectl` 0700, wrapper root 0755 | `systemctl list-timers 'tapectl-*'` |

Nothing is written to a tape before step 12, and step 12 erases only the
cartridge whose serial you typed.

---

## 4. The tapectl home

tapectl resolves its home from `$HOME` — `--home DIR` or `TAPECTL_HOME=DIR`
override it (`src/startup.rs`; the operator guide's "Working on a different
archive"). For the service user that is **`/var/lib/tapectl/.tapectl`**, mode
0700, re-tightened on every run (issue #41):

```
/var/lib/tapectl/.tapectl/
├── config.toml          dar path, [[backends.lto]], [staging], [defaults], [discovery], [[collections]]
├── tapectl.db           the catalog (SQLite, WAL mode: tapectl.db-wal / -shm appear while open)
├── keys/                every PRIVATE key: <tenant>-<alias>.age.key, and the .age.pub beside it
├── catalogs/            dar catalogues: <first 8 chars of the unit uuid>/s<stage set id>/, one per stage set, made by its own dar run
├── stage-reports/       stage reports: one text file per stage set (slice sizes and hashes)
├── staging/             created 0700 by init as the default [staging] directory; stays empty once step 7 points staging elsewhere
├── logs/                created by ensure_dirs; may stay empty
├── tmp/                 0700: a stage's dar slice template and a write's catalog.db, each removed once used (never under staging)
├── locks/               stage-<id>.lock: one lockfile per stage set, held while its stage create runs (`staging clean` reclaims them)
├── config.toml.pre-rename-<stamp>   only after the pre-step-1 check renamed old [defaults] keys (§6)
└── config.toml.superseded-<stamp>   only after step 7 regenerated a config this version could not load
```

The home's 0700 is what keeps it private: `tapectl.db`, each private key
(`.age.key`) and each stage report are 0600 as well, while `config.toml` and
the `.age.pub` files take the creating process's umask.

A home made by an older tapectl has `receipts/` where `stage-reports/` is now;
the first command run on that home renames it (if both already exist, both are
left alone and new reports go to `stage-reports/`).

Outside the home: the **staging directory** (`[staging] directory` in
`config.toml`; ephemeral encrypted slices, re-creatable by `stage create`;
inside the home only when step 7 kept `<home>/staging`),
the **heir kit** under your home, the **first-run state** under
`~/.local/state/tapectl/` (log, rehearsal marker, capture files), and the
**backups** (§7).

The database holds public keys and fingerprints only; every secret is a file
under `keys/` (ADR-0009). That is what makes `tapectl.db` safe to copy to a
backup disk and safe to escrow — and it is why a database backup alone does
not bring the keys back (§8).

---

## 5. Resuming

`--from N` starts at step N; `--to N` stops after it. Steps skipped by
`--from` print `skipped (--from N)`. A step that finds its work done says so
and moves on (`already initialised — not re-running init`, `a [[backends.lto]]
entry already exists`, `location X already exists`, `keeping the existing
kit`, `unchanged` from the installer). Facts a later step needs from an
earlier one are recomputed, not remembered. Steps 8, 12 and 13 need the drive,
so any run that reaches one of them without passing step 6 needs `--device`
(or a profile that sets `DEVICE`), whatever its `--from`: `--from 7` gets as
far as step 8 and stops there with `no device chosen — pass --device, or run
with --from 6`. Every `--from 7`, `--from 12` and `--from 13` below assumes it.

Where the script stops on purpose, the last line says why; most also name the
re-entry, and this list gives it for each:

- before `init` when you answered no ("Re-run with --from 7 and answer y
  when the paper is ready");
- on a config this version cannot load (`--from 7`, after fixing the key it
  named, or let it regenerate — §6);
- `✗ stage failed for <unit>` in step 13. That line names no re-entry; the
  refusal above it says why. When the staging directory *may* be too small
  for that unit, `stage create` asks about the case, but step 13 runs it
  without a terminal, so it refuses with the figures (`stage unit "<unit>"
  refused: non-interactive session …`). Free space and re-run `--from 13`, or
  accept the risk for that unit by hand and then re-run `--from 13`, which
  skips a unit that is already staged:
  ```bash
  sudo -u tapectl -H tapectl --yes stage create <unit>
  ```
  When the figures prove the unit cannot fit (`not enough space in staging
  directory …`), no flag overrides the refusal: free space first;
- `volume init` refused in step 13. The script tells three kinds apart:
  - a reason no flag overrides: a sealed cartridge (ADR-0003), `device_sg`
    no longer bound to `device_tape`, or anything else the refusal names (no
    cartridge loaded, a generation the drive cannot write, a retired
    cartridge, …). The last line names `--from 13`, to run after acting on
    the refusal;
  - a stale or foreign tape (File 0 identifies a different volume that is
    not sealed) or an empty File 0. These are the only refusals `--force`
    overrides, and the script offers it where it stands: type the label to
    overwrite the cartridge. Under `--auto` it stops instead, with `volume
    init refused under --auto; not forcing`. Check the tape with `tapectl
    volume identify --device <by-id>`, then re-run `--from 13` without
    `--auto`;
  - no readable medium serial. An interactive run asks you to name the
    cartridge (the barcode on its sticker) and carries on. Under `--auto` it
    stops and says to re-run `--from 13` interactively;
- a red rehearsal, which ends `✗ rehearsal RED — do not write real data until
  this is understood` and names no re-entry: `--from 12` once understood.
  Never write real data over a red rehearsal.

Step 13 is re-entrant (issue #414). A unit that was already staged by hand is
skipped, and re-running `--from 13 --label <label>` with the label of a volume
an earlier run left behind picks up where that run stopped, by what the
catalog says the volume is (`tapectl volume info <label>`):

- **initialised, nothing written** — straight to the write;
- **an interrupted write** (a Ctrl-C, a dropped session, a crash) — the
  snapshot and staging phases are skipped and the step offers `volume resume`
  on the same cartridge, after the quiet-host check. A tape whose seal was
  written but whose confirm did not finish is re-confirmed, never rewritten;
- **sealed** — only the post-write block is left: `volume verify --full`,
  `audit`, the move to the shelf, the Heir Kit;
- **a write in progress right now**, or **a session planned that never
  reached the tape** — it stops and names the remedy (let the live writer
  finish; `tapectl volume abort <label>`, then re-run with a new label,
  `--from 13 --label <new>`: an aborted label cannot be written again; the
  new label's `volume init` asks before overwriting the cartridge's File 0,
  which still names the old one, and marks the old volume `erased`);
- anything else (quarantined, retired, erased) — it stops and asks for a new
  label.

---

## 6. Re-running over a home that already exists

This is the reinstall case: the machine was rebuilt, or the home was restored
from a copy (§9), and you run `scripts/first-run.sh` again.

**Before step 1, the renamed `[defaults]` keys.** The copy requirement every
unit starts from is `[defaults] min_copies` and `min_locations` (ADR-0012,
2026-09-29 amendment). A home initialised by an older tapectl has them as
`min_copies_for_tape_only` and `min_locations_for_tape_only`, which this
version refuses by name. On every run, whatever `--from` says, the script
looks for the old names in the home's `config.toml` (once the service user
exists, or under `--no-service-user`), and when it finds them it offers to
rename them in place. It keeps the original as
`config.toml.pre-rename-<stamp>`. The meaning and values are unchanged. If
you decline, every `tapectl` command refuses the config until you rename the
two lines by hand.

**Step 7 detects `tapectl.db` and does not run `init`.** It then probes whether
*this* binary can load the existing `config.toml` (`config show` must succeed;
unknown keys are hard errors, ADR-0012). If it cannot, it prints `config check`'s
message naming the key, and offers to back the file up as
`config.toml.superseded-<stamp>` and write a fresh default one — generated
through tapectl's own writer in a throwaway home (`init --no-escrow --operator
config-template`, whose database is discarded), never a template. The
database, keys, tenants and tapes are untouched; the `[[backends.lto]]` and
`[[collections]]` tables are **not** carried over — step 8 re-adds the drive,
collections are yours to re-add. The smallest fix is usually to edit the
named key by hand instead (delete it, or rename it as the message says).

**A rebuilt home with no database — adopt the original escrow identity.** When
step 7 does run `init` interactively, it asks for an existing escrow **public**
key to adopt (`age1…`, or a `.pub` path). Answer with the one on the heir kit's
cover sheet and `init` registers it instead of minting a new identity. **Under
`--auto` there is no such prompt**, and no flag or profile variable carries the
key, so an `--auto` run's `init` mints a new identity. On a reinstall, run step 7
without `--auto`, or run `init` by hand before step 7 (which then finds the
database and leaves it alone):

```bash
sudo -u tapectl -H tapectl init --operator <name> --escrow-public-key age1…
```

There is exactly one escrow identity per catalog and **no command replaces
it** (ADR-0005; `key import --escrow` and `init --escrow-public-key` both
refuse while one is registered). If a plain `init` already minted a fresh one
on the rebuilt machine, remove the empty home and start again — before
importing or rebuilding anything into it. `docs/operator-guide.md`,
"Disaster Recovery", step 1, has the full reasoning.

**Step 8** finds the existing backend and only cross-checks it: if the
config's `device_tape` is not the drive you chose in step 6, it says so and
leaves the edit to you (there is no `backend edit`). `device_sg` is the node
most likely to have moved on a rebuilt host; `config check` warns about a
tape/sg pairing that names two different drives, and `volume write` refuses
it. Fix `[[backends.lto]] device_sg` by hand, then `config check`.

**Step 11** finds a `.tapectl-unit.toml` already in a directory and adopts it
instead of re-creating it — the dotfile carries the unit's uuid — by adding
the path to `[discovery] watch_roots` and running `tapectl unit discover`. The
ACL grant is re-applied first; ACLs are per host and do not travel with a
copied tree. Before it appends a profile's `[[collections]]` table it deletes a
`collections = []` line, which an older `init` wrote and which cannot sit
beside that table.

**Step 12** must be earned again: the marker is per binary hash and lives in
your state directory, not the home.

**Step 14** is idempotent by construction (§7): run it again and it reports
each installed file `unchanged`, or rewrites only what differs.

---

## 7. The timers and the operator wrapper

`scripts/install-systemd.sh` is what step 14 runs; it can be run alone at any
time, and it never touches a tape or a device node.

```bash
scripts/install-systemd.sh --dry-run                       # print the plan, change nothing
scripts/install-systemd.sh --backup-dir /mnt/backup/tapectl # install (or re-render) for this host
scripts/install-systemd.sh --uninstall                     # take the units and wrappers off again
```

Options: `--user NAME` (default `tapectl`; must already exist — the script
refuses to create an account), `--home DIR` (that account's home), `--tapectl-home DIR`
(a tapectl home that is not `<home>/.tapectl`; sets `TAPECTL_HOME=` in the
units), `--backup-dir DIR`, `--keep N`, `--tapectl PATH` (default
`/usr/local/bin/tapectl`), `--no-wrapper`, `--no-start`.

It renders `contrib/systemd/` for the host — `User=`, `HOME=`, `TAPECTL_BIN=`,
`TAPECTL_BACKUP_DIR=`, `TAPECTL_BACKUP_KEEP=`, `ReadWritePaths=` — installs
only the files whose bytes differ, creates the backup directory (0700, owned by
the service user), `daemon-reload`s, enables and starts both timers, restarts a
timer whose unit changed, and prints `systemctl list-timers --all 'tapectl-*'`.
**Edit the installed units by re-running the script**, not by hand.

| unit | schedule | runs | exit status |
|---|---|---|---|
| `tapectl-audit.timer` → `tapectl-audit.service` | **weekly**, Monday 09:00, `Persistent=true`, up to 30 min random delay | `tapectl-scheduled-audit.sh`: `tapectl audit`, then `tapectl report verify-status` | 0 clean, 1 warnings (**success** — ADR-0004, advisory), 2 violations (failure) |
| `tapectl-backup.timer` → `tapectl-backup.service` | **daily**, 03:00, `Persistent=true`, up to 15 min random delay | `tapectl-scheduled-backup.sh`: `tapectl db backup --to <dir>/tapectl-<UTC stamp>.db`, SQLite-header check on the copy, `tapectl db fsck` on the live catalog, prune to the newest **14** | nonzero if the copy is not a database or fsck found problems |

Both services run as the service user with `PrivateDevices=true` (no
`/dev/nst*`, ever), `ProtectSystem=strict` with only the tapectl home and — for
the backup — the backup directory writable, and `NoNewPrivileges=true`. The
home must be writable even for a read: the database is in WAL mode.

**The backup directory.** Default `/var/backups/tapectl`; retention
`TAPECTL_BACKUP_KEEP=14`; private keys **not** included (issue #40 — a key copy
makes the destination a key-escrow point; `TAPECTL_BACKUP_INCLUDE_KEYS=1` in
the installed service turns it on for a destination you keep as secret as the
home). The directory must be outside the tapectl home (refused) and should be
on a **second disk** (warned, never refused — ADR-0012, item 5). The
default, `/var/backups/tapectl`, is usually on the same filesystem as the home; the
script says so at install time, and `--backup-dir` is how you move it to a second
disk.

**After a session.** A timer cannot know when a write session ended, and the
ruling asks for a backup "after every session":

```bash
sudo systemctl start tapectl-backup.service && journalctl -u tapectl-backup.service -n 30
scripts/first-run.sh --from 9 --to 9      # regenerate the heir kit (answer y); then reprint COVER.txt
```

Step 9 on its own is the kit refresh; it needs no `--device`, and a host
installed from a profile passes the same `--profile` (home2's puts the kit in
`~/tapectl-heir-kit`). It does what a bare `sudo -u tapectl -H tapectl key
escrow-kit --out ~/heir-kit` cannot: the kit directory is **yours**, 0700
(§3, row 9), so the service user cannot write into it and that command fails
with `Permission denied`. The step hands the directory to the service user for
the write and takes it back afterwards. By hand, the same thing is:

```bash
mkdir -p ~/heir-kit                                          # as you; the service user cannot create it
sudo chown -R tapectl ~/heir-kit
sudo -u tapectl -H tapectl key escrow-kit --out ~/heir-kit
sudo chown -R "$USER" ~/heir-kit
```

This relies on the traverse-only ACL (`u:tapectl:x`) that step 9 put on each
ancestor of the kit directory the service user could not already enter. A kit
directory somewhere new is created by the `mkdir` above, and each of its
ancestors the service user cannot enter needs `sudo setfacl -m u:tapectl:x
<ancestor>` before the `chown`. Under `--no-service-user` it is plain `tapectl
key escrow-kit --out ~/heir-kit`.

**During a write session.** A backup may land in the middle of an overnight
`volume write`. Opening the database runs the startup sweep, which leaves
a live session alone: it tells a crashed session from a running one by the
volume's lock, which the kernel releases when a process dies. The backup
itself is a single-step online copy that never blocks the writer. If another
command holds the catalog's write lock longer than the backup will wait,
`tapectl` exits 75 ("catalog busy") and the wrapper logs "catalog busy, no
backup this run" instead of a failure. To move the time, edit `OnCalendar=`
in `contrib/systemd/tapectl-backup.timer` and re-run the installer.

**The wrapper.** `/usr/local/bin/tapectl-op` is

```sh
exec sudo -u tapectl -H /usr/local/bin/tapectl "$@"
```

installed by `install-systemd.sh` (so by step 14), rendered with the user and
binary you gave it, and skipped when the service user is the account running
the installer. `tapectl-op audit`, `tapectl-op report summary`, and so on.

Checking: `systemctl list-timers --all 'tapectl-*'`,
`systemctl status tapectl-backup.service`, `journalctl -u tapectl-audit.service -n 50`,
`sudo ls -l /var/backups/tapectl` (the directory is the service user's, 0700, so
a plain `ls` as you is refused).

---

## 8. Getting the catalog back

Three sources, from cheapest to last resort. Which one you take depends on
what you hold, not on how bad the loss is. [Keys and recovery](keys-and-recovery.md)
(runbooks C and D) is the authority; this is the short form with the ordering
trap spelled out.

**Rule for all three: the home first, then the keys, then the rows.** `db
import` replaces the *entire* live database, so anything you rebuilt or
registered before importing is gone afterwards.

### 8a. From a `db backup` copy (the daily timer, §7)

The home still exists and holds the keys; only the catalog is damaged or lost.

```bash
sudo ls -l /var/backups/tapectl/                                # newest tapectl-<stamp>.db (the dir is 0700, the service user's)
sudo -u tapectl -H tapectl db import /var/backups/tapectl/tapectl-20260924T030000Z.db
sudo -u tapectl -H tapectl db fsck                              # every import is followed by this
sudo -u tapectl -H tapectl audit                                # what the restored catalog says is overdue
```

`db import` is an ADR-0008 Tier-2 gate: it states that it overwrites the live
database and asks; `-y` answers for a scripted run; `--dry-run` previews. It is
a raw page copy and checks no foreign keys of its own — hence the `fsck`. A
copy made with `TAPECTL_BACKUP_INCLUDE_KEYS=1` has a `tapectl-<stamp>.keys/`
directory beside it; copy its files into `keys/` (owner `tapectl`, mode 0600)
if the home's own `keys/` is what was lost.

### 8b. From the heir kit

The home is gone. You hold the printed cover sheet (escrow public key, the
hand-written secret) and the kit's `catalog.db.age`. In this order:

1. Build a home that carries the **original** escrow identity: run
   `scripts/first-run.sh --to 8`, interactively (under `--auto` there is no
   adoption prompt and `init` mints a new identity, §6), and answer step 7's
   adoption prompt with the sheet's `age1…` — or by hand, `init --operator
   <name> --escrow-public-key age1…` (§6; as the service user `init` refuses
   without `--operator`). Stop after step 8. Step 9 writes a new kit, by
   default into `~/heir-kit`, and overwrites the `catalog.db.age` you are
   about to decrypt if it is there (the step asks first only when a
   `COVER.txt` is there too). Steps 10–13 would register locations, tenants,
   units and a first tape in a database that step 3's import replaces. Step
   8's drive entry is in `config.toml`, which the import leaves alone.
2. Put the private keys back into `keys/` from a copy of the home (§9). The
   kit does **not** contain them: the database stores public halves only. If
   no copy exists, the escrow secret still opens every tape (it is a recipient
   of every slice), so the data is recoverable — through `RESTORE.sh --key`
   or `catalog rebuild --key <escrow secret file>` — but the tenants' own key
   files are not, and new tapes for those tenants will be encrypted to keys
   you generate afresh.
3. Decrypt and import the bundle, then fsck. Run this in the directory that
   holds the kit's `catalog.db.age`. The service user cannot read a kit
   directory (yours, 0700, §3 row 9), and usually not your home either, and
   `db import` reports a file it cannot reach as `import source not found`,
   so the decrypted copy goes to it through its own home:
   ```bash
   age -d -i escrow.age.key -o catalog.db catalog.db.age      # escrow.age.key: the secret, alone on one line
   sudo install -o tapectl -m 0600 catalog.db /var/lib/tapectl/catalog-import.db
   sudo -u tapectl -H tapectl db import /var/lib/tapectl/catalog-import.db
   sudo -u tapectl -H tapectl db fsck
   sudo rm /var/lib/tapectl/catalog-import.db
   ```
4. Rebuild the tapes sealed after the kit was made. The imported catalog has
   no row for them, so neither `volume list` nor `audit` can name them; the
   shelf is the list. The simplest route is `catalog rebuild --from-volume
   --device <by-id> --key <operator or escrow secret key> [--label <L>]` on
   **every** cartridge you hold, once each, any order: it inserts what is
   missing, and changes a row it finds only to bind the cartridge (recording a
   chip serial the row lacked), to mark a volume displaced by a re-initialised
   cartridge `erased`, or, with the escrow key, to attest escrow coverage —
   printing a line for each ([Keys and recovery](keys-and-recovery.md), runbook C,
   step 7). Running it on a cartridge the catalog already has is safe. The narrower route: `volume list` shows every
   volume the kit's catalog knew (the cover sheet's "Sealed cartridges known
   at generation time" counts the sealed ones), `volume identify --device
   <by-id>` prints the label of the cartridge that is loaded, and only the
   cartridges missing from the list need the rebuild.
5. Regenerate the kit and finish the install: `scripts/first-run.sh --from 9
   --device /dev/tape/by-id/scsi-<SERIAL>-nst`. Step 9 is the kit refresh of
   §7, "After a session" (answer y), and §6 says what the later steps do over
   a home that already has rows. Until the kit is regenerated, what `audit` says
   about the kit on this catalog (`escrow_kit_stale`, `escrow_kit_missing`) is
   not a list of tapes to rebuild: the kit's catalog was copied before the kit
   recorded its own generation, so the imported catalog measures against the
   kit *before* this one (or, for a first kit, against none), and its stale
   count can include tapes it already lists.

### 8c. From the tapes alone

No home, no kit, but a key that opens the operator envelope — the operator's
or the escrow secret — and the cartridges. The rebuild does not recover the
escrow *identity* from the tape; it compares what it finds against the one
the catalog has registered, so register it first:

- you hold the escrow **secret**: its public half is `age-keygen -y
  escrow.age.key`; `init --operator <name> --escrow-public-key` with that, as
  in 8b;
- you hold only the operator key and do not know the escrow public key:
  `init --operator <name> --no-escrow` now, and `key import --escrow age1…`
  the day you find it (it refuses only while one is already registered). Until then escrow
  coverage cannot be confirmed for anything, and `audit` does **not** say so:
  with no escrow identity registered it skips its escrow checks, so a clean
  `audit` says nothing about escrow, and `catalog locate` shows `-` in its Escrow
  column ([Keys and recovery](keys-and-recovery.md), runbook C, step 3).

Then `catalog rebuild --from-volume --device <by-id> --key <key>` per
cartridge. A tenant with only their tenant key needs no catalog at all:
`tapectl volume identify --device <by-id>` and the tape's own `RESTORE.sh`
(operator guide, "If you hold a tenant key").

---

## 9. Moving to a new host

What travels is the **home**, the **keys** (inside it) and the **config**
(inside it). What does not travel is everything that is a fact about the old
host.

**Copy** — with no tapectl running (WAL sidecars; check `systemctl list-timers
'tapectl-*'` and that no `volume write` is in flight), preserving ownership:

```bash
# old host
sudo tar -C /var/lib/tapectl -cpf /media/usb/tapectl-home.tar --exclude='.tapectl/staging/*' .tapectl
sudo -u tapectl -H tapectl db backup --to /var/lib/tapectl/tapectl-catalog.db   # a second, independent copy of the catalog
sudo mv /var/lib/tapectl/tapectl-catalog.db /media/usb/
# the heir kit directory (~/heir-kit) and the paper stay yours either way
```

The `--exclude` keeps the empty `staging/` directory and leaves out its
contents: after a fresh `init` the staging directory is `<home>/staging`
(§4), and after a write it still holds that tape's encrypted slices, up to a
full tape's worth. A staging directory outside the home is not in the tarball
at all. The catalog copy is written into the service user's home first and
moved onto the medium as root, because a stick mounted by root is not
writable by the service user.

Treat that tarball as the keys themselves: it is every private key in the
archive. Wipe the medium when the move is done.

**Do not copy**: the staging directory (ephemeral; re-stage), the first-run
state under `~/.local/state/tapectl/` (the rehearsal marker is proof about one
binary on one host — earn it again), the systemd units (re-render them for the
new host), the ACLs (per filesystem; step 11 re-grants), the `tape` group
membership (step 6), and `device_sg` (re-derived; §6).

**On the new host**:

1. `scripts/first-run.sh --to 5` — toolchain, tools, binary, tests, the
   service user (its uid may differ; ownership is fixed in the next step).
2. Put the home in place:
   ```bash
   sudo tar -C /var/lib/tapectl -xpf /media/usb/tapectl-home.tar
   sudo chown -R tapectl:tapectl /var/lib/tapectl/.tapectl
   sudo chmod 0700 /var/lib/tapectl/.tapectl
   ```
3. `scripts/first-run.sh --from 6` — the drive by serial; step 7 finds the
   home initialised and probes the config (§6); step 8 cross-checks the
   backend — fix `device_sg` if it moved; step 9 asks before regenerating the
   kit; step 11 re-grants the ACLs and adopts the dotfiles; step 12 rehearses
   this host's binary; step 14 installs the timers with `--backup-dir` on the
   new host's second disk.
4. `sudo -u tapectl -H tapectl config check && sudo -u tapectl -H tapectl db fsck && sudo -u tapectl -H tapectl audit`.

The escrow identity travels inside `tapectl.db` and needs nothing from you;
the escrow *secret* is on paper only and is not in the tarball.

---

## 10. Uninstall

Two layers. `install-systemd.sh --uninstall` removes what step 14 added and
nothing else; the rest is deliberate, by hand, in this order.

```bash
# 1. the timers, their wrappers and tapectl-op (disables and stops the timers,
#    removes the four units and any drop-ins, /usr/local/lib/tapectl/, the wrapper,
#    daemon-reloads)
scripts/install-systemd.sh --uninstall

# 2. the binary
sudo rm -f /usr/local/bin/tapectl

# 3. the ACL grants on every unit tree (per tree; -b strips ALL ACLs from the
#    tree — use -x u:tapectl to remove only tapectl's entries)
sudo setfacl -R -x u:tapectl /data/alice/photos
sudo setfacl -R -d -x u:tapectl /data/alice/photos
#    ancestors got a traverse-only entry:
sudo setfacl -x u:tapectl /data/alice /data
#    so did each ancestor of the kit directory the service user could not enter
#    (step 9) — for ~/heir-kit, your home. Do this before step 6: once the user
#    is gone the entry shows as a bare uid and u:tapectl no longer names it.
sudo setfacl -x u:tapectl ~
#    the unit dotfiles, if the trees are leaving tapectl for good:
sudo find /data -name .tapectl-unit.toml -delete

# 4. the staging directory (encrypted slices only; nothing here is the sole copy
#    of anything). [staging] directory names it; when that is inside the home
#    (a fresh init's <home>/staging), step 6 removes it and there is nothing to do
sudo sed -n '/^\[staging\]/,/^\[/s/^directory *= *//p' /var/lib/tapectl/.tapectl/config.toml
sudo rm -rf /scratch/tapectl-staging  # the directory printed above, when it is outside the home

# 5. the backups — ONLY once you hold a heir kit or another catalog copy you trust
sudo rm -rf /var/backups/tapectl

# 6. the operator's sudoers rule, on a host prepared by contrib/hosts/home2-prep.sh,
#    then the service user and its home — the home holds every private key; the
#    heir kit and the paper are what remain of the archive's identity after this
sudo rm -f /etc/sudoers.d/tapectl-operator
sudo userdel --remove tapectl          # removes /var/lib/tapectl

# 7. your own state (log, rehearsal markers) and the kit files on disk
rm -rf ~/.local/state/tapectl
rm -rf ~/heir-kit                      # the PRINTED, sealed kit is the artifact; this was the source

# optional, and left alone by everything above: rustup (~/.cargo), age
# (/usr/local/bin/age, age-keygen), the apt packages, your membership of the
# tape group (sudo gpasswd -d $USER tape)
```

Tapes are not affected by any of this. A sealed volume is self-describing: with
`mt`, `dd`, `age`, `dar`, `tar` and a key, `RESTORE.sh` off the tape restores it
on a machine that has never seen tapectl (operator guide, "Disaster Recovery").

---

## 11. After an install — the five-minute check

```bash
tapectl --version                                   # note it; the rehearsal marker is per binary
sudo -u tapectl -H tapectl config check             # config loads; the two device nodes name one drive
sudo -u tapectl -H tapectl db fsck
sudo -u tapectl -H tapectl audit                    # 0 clean, 1 warnings, 2 violations — all fine on day one
systemctl list-timers --all 'tapectl-*'             # both timers, next elapse shown
sudo systemctl start tapectl-backup.service && sudo ls -l /var/backups/tapectl
ls -l ~/.local/state/tapectl/rehearsal-ok-*         # the step-12 marker for THIS binary
```

If `tapectl-op` is installed, the three `sudo -u tapectl -H tapectl` lines are
`tapectl-op …`.
