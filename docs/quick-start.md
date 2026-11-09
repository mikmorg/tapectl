# Quick start: one drive, your own Linux box

For someone with **one LTO tape drive** on a Linux machine who wants their folders on
tape, encrypted, in two copies, in two places. tapectl runs as your own account: no
service user, and nothing runs as root except the few lines `tapectl setup` prints
for you at the end.

A host run by a dedicated service user (the layout home2 uses) is installed by
`scripts/first-run.sh` instead: see [Installing tapectl](install.md).

## 1. Install

Download the static binary for your machine and `SHA256SUMS` from the latest
release, <https://github.com/mikmorg/tapectl/releases/latest>. Check the binary
against `SHA256SUMS`, then put it on your `PATH` as `tapectl` (in `~/.local/bin`,
or `/usr/local/bin` with sudo):

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

Then `tapectl --version` names the release.

No release binary for your machine? Build it (Rust from <https://rustup.rs>):

```bash
cargo install --locked --git https://github.com/mikmorg/tapectl
```

tapectl also needs `dar`, `sg3-utils` and, for reading a tape back without tapectl,
`age`, `mt-st` and `tar`. You need not install them first: `tapectl setup` checks
and prints the install line for what is missing.

## 2. Run `tapectl setup`

```bash
tapectl setup
```

It asks one question at a time; Enter takes the answer in brackets. Each step says
what it is about to do, then what it did. In order: where the catalog lives
(`~/.tapectl`); a staging directory, on a big disk that is not the one your data is
on; the drive, found under `/dev/tape/by-id/` (only its identity is read; the tape is
never opened); your keys and the **escrow identity**; two places to keep cartridges;
your first collection, a folder whose subfolders become the units; weekly
catalog-backup and audit timers; a check of the config.

A run of setup on a machine where mhvtl's virtual library stands in for a real drive,
trimmed (`~` is the account's home; `…` marks lines cut; the answers typed are shown
after each question, and Enter alone took the default):

```text
tapectl setup: each step says what it is about to do, then what it did.
Enter takes the answer in [brackets]. Ctrl-C between steps is safe: run setup again to finish.

1. Home: where the catalog, config and keys live
  the home will be ~/.tapectl (~/.tapectl, the default: nothing named another home)
  (to put it elsewhere, stop here and run `tapectl --home DIR setup`)
  Make the home there? [Y/n]:
  made ~/.tapectl: config.toml, the catalog tapectl.db, keys/

2. Staging: where archives wait between staging and the tape
  staged slices are the encrypted archives waiting for the tape; give them a big disk that is not the one your data is on
  Staging directory [~/.tapectl/staging]: /srv/staging
  /srv/staging does not exist. Create it? [Y/n]:
  made /srv/staging
  /srv/staging is writable, 50.4 GiB free
set staging.directory = "/srv/staging" (was "~/.tapectl/staging") in ~/.tapectl/config.toml

3. Drive: the tape drive tapectl writes with
  found 4 drives:
    1) /dev/tape/by-id/scsi-XYZZY_A1-nst → /dev/nst0: IBM ULT3580-TD8, LTO-8
    2) /dev/tape/by-id/scsi-XYZZY_A2-nst → /dev/nst1: IBM ULT3580-TD8, LTO-8
    3) /dev/tape/by-id/scsi-XYZZY_A3-nst → /dev/nst2: IBM ULT3580-TD6, LTO-6
    4) /dev/tape/by-id/scsi-XYZZY_A4-nst → /dev/nst3: IBM ULT3580-TD6, LTO-6
  Which drive (its number or its path): 1
  generation LTO-8, read from the drive attached at /dev/tape/by-id/scsi-XYZZY_A1-nst (model IBM ULT3580-TD8)
  adding drive "lto8" (LTO-8) at /dev/tape/by-id/scsi-XYZZY_A1-nst
drive "lto8" added to ~/.tapectl/config.toml (LTO-8, tape=/dev/tape/by-id/scsi-XYZZY_A1-nst, sg=/dev/sg2 (derived from device_tape))
verify it with: tapectl config check

4. Keys: the operator's keys and the escrow identity
  The operator's name [alice]:
  made the operator "alice", with a primary and a backup key pair in ~/.tapectl/keys
  the escrow identity is the one key that reads every tape if everything else is lost; setup mints a new one unless you already have one to adopt
  Mint a new escrow identity? [Y/n]:

  ==========================================================================
  THE ESCROW SECRET: MOVE IT OFFLINE, THEN DELETE IT FROM THIS MACHINE
  ==========================================================================
  The escrow identity is the one key that reads every tape tapectl writes
  if everything else is lost: this machine, the catalog, the operator's
  keys. Every tape is encrypted to it as well as to its tenant.

  Its secret was written to:
      ~/.tapectl/escrow-secret.age.key

  Move it offline now: print it, or copy it to a USB stick kept in a
  safe, in two places if you can. Then delete it from this machine:
      rm ~/.tapectl/escrow-secret.age.key
  Whoever holds it can read every tape: keep it as you keep a will.
  `tapectl key escrow-kit DIR` writes the Heir Kit to keep beside it.

  Its public key (safe to keep anywhere): age1r6mm0yx9…
  ==========================================================================

5. Locations: the places cartridges are kept
  two places, so one copy survives what happens to the other (a fire, a flood, a burglary)
  The first place [home]:
  The second place, away from the first [offsite]:
location "home" added (id=1, kind=shelf)
location "offsite" added (id=2, kind=shelf)

6. First collection: a folder whose subfolders are the units
  a collection is a folder whose subfolders are the units, one folder, one unit: /media/photos with 2019-italy/ and 2020-garden/ in it makes two units
  The collection's folder, e.g. /media/photos: /media/photos
  The collection's name [photos]:
  The tenant its units belong to [alice]:
added collection "photos" (root /media/photos, tenant alice, a unit per folder 1 level below the root) to ~/.tapectl/config.toml
its sync, as `tapectl collection sync` would run it now:
collection "photos" (dry run): would create 2; 0 refused · 0 moved, 0 reactivated, 0 missing · 2 pending (2 never archived)
  Register these units now? [Y/n]:
collection "photos": 2 created; 0 refused · 0 moved, 0 reactivated, 0 missing · 2 pending (2 never archived)

7. Backups and audit: user timers
  a weekly copy of the catalog to another disk, and a weekly `tapectl audit`, as systemd user timers that run as alice
  Directory for the catalog backups, best on another disk [~/tapectl-backups]: /mnt/usb/tapectl-backups
  …
  wrote ~/.config/systemd/user/tapectl-backup.service
  wrote ~/.config/systemd/user/tapectl-backup.timer
  wrote ~/.config/systemd/user/tapectl-audit.service
  wrote ~/.config/systemd/user/tapectl-audit.timer
  turn them on (setup writes the files only):
    systemctl --user daemon-reload && systemctl --user enable --now tapectl-backup.timer tapectl-audit.timer

8. Check: `tapectl config check`
config: valid
home: ~/.tapectl (~/.tapectl, the default: nothing named another home)
dar: 2.7.13 at '/usr/bin/dar' (meets minimum 2.6)
staging: '/srv/staging' exists and is writable
…
  config check: ok

9. What needs root
  setup never runs sudo; copy these and run them yourself:

    sudo loginctl enable-linger alice

  loginctl: so the timers run while you are logged out

10. Next: the first two copies
  see how the units pack into tapes:
    tapectl collection plan
  copy 1: load a blank cartridge, then
    tapectl collection run photos --label L8-0001 --init --full-confirm --move-to home
  copy 2: load the next blank cartridge, then
    tapectl volume write L8-0002 --init --full-confirm --move-to offsite
  where things stand, and the next command, at any time:
    tapectl status
  …
```

Every question has a flag (`tapectl setup --help` lists them). `--yes` takes every
default and refuses only a question that has none, naming its flag; `--dry-run` says
what each step would do and changes nothing:

```bash
tapectl setup --yes --staging /srv/staging --collection-root /media/photos
```

Setup is safe to run again. A step already done is skipped, so a run you stopped is
finished by the next one, and a run on a finished home changes nothing and says so.

## 3. The escrow secret

Do this before the first copy. The escrow secret is the one key that reads every tape
if this machine, its catalog and your own keys are all lost. Setup writes it to
`~/.tapectl/escrow-secret.age.key` and never prints it. Print it on paper, or copy it
to a USB stick you keep in a safe (two places if you can), then delete the file:

```bash
rm ~/.tapectl/escrow-secret.age.key
```

Then write the Heir Kit, the papers that let someone else find and read your tapes,
and keep it with the secret:

```bash
tapectl key escrow-kit ~/heir-kit
```

[Keys and recovery](keys-and-recovery.md#the-escrow-recipient) explains every key
and what losing each one costs.

## 4. The root block

Setup never runs `sudo`. It prints only the lines that apply to your machine. The
account above was already in the drive's group and had every tool, so it got one
line; a fresh machine gets up to three:

```text
    sudo usermod -aG tape alice
    sudo apt install dar sg3-utils age mt-st
    sudo loginctl enable-linger alice
```

`usermod` puts your account in the drive's group (log out and back in after it);
`apt install` names the Debian/Ubuntu packages of the missing tools (another distro
has its own names); `enable-linger` keeps the timers running while you are logged
out. Run them, then turn the timers on with the `systemctl --user` line from step 7.

## 5. The first two copies

Load a blank cartridge and write copy 1. `--init` labels the cartridge, the
collection's units are staged and written, `--full-confirm` reads the whole tape back,
and `--move-to` records the shelf:

```bash
tapectl collection plan
tapectl collection run photos --label L8-0001 --init --full-confirm --move-to home
```

Then load the next blank cartridge and write copy 2 from the same staged slices, for
the other place:

```bash
tapectl volume write L8-0002 --init --full-confirm --move-to offsite
```

Put the stickers `L8-0001` and `L8-0002` on the cartridges, and take the second one
to its place. `tapectl status` shows where things stand and the next command at any
time.

The labels follow the drive's generation (`L6-…` on LTO-6). The last lines of setup
give them for your drive.

**Optional, with a spare cartridge:** `scripts/lto-profile.sh` in the tapectl
repository measures your drive and gives its profile. It erases that cartridge and
takes about 4.5 hours on LTO-6. Without it, each copy's full readback
(`--full-confirm`) is the proof that the tape is good, and planning stays at the fill
ceiling. See [LTO profiling](lto-profiling.md).

## Where next

- [The walkthrough](walkthrough.md): the whole life of an archive, step by step,
  with each command's output: audits, restores, a Heir Kit, a disaster rehearsal.
- [The operator guide](operator-guide.md): day-to-day work.
- [Keys and recovery](keys-and-recovery.md): the keys, the Heir Kit, and every
  recovery runbook.
- [Troubleshooting](troubleshooting.md).
