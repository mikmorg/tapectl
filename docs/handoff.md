# Handoff: tapectl 2.0.0 on home2, and what only you can do

Rewritten for tapectl 2.0.0 (Layout Version 3). The earlier versions, which carried the
1.x installs and upgrades of home2 (the fresh start on 1.1.0, the in-place upgrades to 1.5.0
and 1.8.0, and the fresh start on 1.8.1), are in git history (`git log -- docs/handoff.md`),
and what each run did is in `docs/runs/`. The file answers one question: **which remaining
work needs a person, and which does not?** What 2.0.0 changes is in
[releases/2.0.0.md](releases/2.0.0.md); what each step creates on the host is in
[install.md](install.md).

## The state, in one paragraph

tapectl 2.0.0 writes Layout Version 3 and reads and writes nothing else (ADR-0012 item 242;
ADR-0014 gathers the rulings). Production is **home2**
(`contrib/hosts/home2.profile`); its drive is not on the development VM. Until the acceptance
run below passes, **the cartridges L6-0001 and L6-0002 and home2's catalog are test articles**
(items 235 to 238): they hold Layout 2 volumes of `keepsake` written by 1.9.2, and home2 runs
1.9.2. **There is no upgrade from 1.x** (item 258): 2.0.0 reaches home2 by a fresh start (item
244), and the order of the acceptance run is item 268. Production is gated on the whole of the
build, the drill and the reads (items 248 and 262); the real-drive runs are the operator's, and
`docs/design/v3-implementation-plan.md` (Stage 7) is the plan this file carries out on home2.

## Who does what

| | Step | Who |
|---|---|---|
| 1 | Tag `v2.0.0`, once the build is complete and proven on test cartridges; CI builds the static binary | the coordinator, on the CTO's word |
| 2 | The fresh start on home2: the host's tar, the old state aside, `first-run.sh` | **you** (sudo, the escrow paper, the cartridges) |
| 3 | Erase L6-0001 and L6-0002 with the typed label, and write both at real scale | **you** |
| 4 | The drill: a whole-tape restore at real scale with the Heir Kit's keys, and File 1's steps on a real cartridge; record it | **you** |
| 5 | The verdict: a pass makes the cartridges production and ends the on-tape authorization (item 268) | the CTO |

## Before you start

- **Work inside tmux, on home2, with `v2.0.0` checked out.** `git fetch --tags && git checkout
  v2.0.0`; `git describe --tags` must read `v2.0.0`.
- **The escrow paper.** The escrow identity is kept if the paper passes its check: the secret
  written on it gives, through `age-keygen -y`, the public key in
  `~/.config/tapectl/home2.local.profile` (`ESCROW_PUBLIC_KEY=age1…`, kept out of git by
  choice). Check it before anything is deleted, against the old home's own file:

  ```bash
  grep ESCROW_PUBLIC_KEY ~/.config/tapectl/home2.local.profile
  sudo cat /srv/archive_meta/tapectl/home/keys/mikmorg-escrow.age.pub
  ```

  The two must be identical, and the paper's secret must give that key. No command replaces an
  escrow identity later (ADR-0005). To mint a new one instead, move the local profile aside and
  leave step 7's question empty: the new secret is handwritten and sealed in two failure
  domains, and the old paper and kits stay until the old tapes are erased.
- **Nothing owed to the old tapes.** L6-0001 and L6-0002 can be rewritten whenever (item 245):
  from the erase until two new copies seal and verify, `keepsake` has no copy on tape, and its
  originals stay on disk. The same holds for the old catalog and keys: they open only the
  erased tapes.
- **Keep tapectl 1.9.2** (`/usr/local/bin/tapectl-1.9.2`, or the `v1.9.2` checkout) until you
  are sure no Layout 2 tape you still need is unread. 2.0.0 knows such a tape by its label and
  reads nothing else of it (item 269).
- **Quiet vm-desk1.** The development VM shares home2's CPUs: no builds there while a tape
  write or its read-back runs. Tell the coordinator ahead of each write.

## The fresh start (item 244)

In this order. Never run step 7 of `first-run.sh` under `script` or `tee`: if you mint an
escrow identity, its secret prints there once.

1. **Stop the timers:** `scripts/install-systemd.sh --uninstall`, then `pgrep -a -u tapectl`
   (nothing may be running). Step 14 installs them again.
2. **Build the host's tar** (items 274, 275). home2's tar is GNU tar 1.30, below the writer's
   floor of 1.35 built with ACL support. `contrib/hosts/home2-prep.sh --apply` builds the
   upstream 1.35 into `/usr/local`, checks that it writes ACL entries by number and leaves
   `/bin/tar` alone (most of its other parts report "already done"); `home2.profile` names
   `TAR_BIN=/usr/local/bin/tar`, which step 7 writes into `[tar] binary`.
   [install.md](install.md) ("The writer's tar") says what the build proves.
3. **Move the old state aside, not deleted,** until the new copies read back in full. The home
   is `/srv/archive_meta/tapectl/home` and holds the old catalog, keys and config; the daily
   backups are in `/srv/local_backup/tapectl` (and `/var/backups/tapectl` for the 2026-10 ones).
   The directories stay, their contents move:

   ```bash
   sudo mv /srv/archive_meta/tapectl/home /srv/archive_meta/tapectl/home-1.9.2
   for d in /srv/local_backup/tapectl /var/backups/tapectl; do
     sudo mkdir -p "$d-old" && sudo find "$d" -mindepth 1 -maxdepth 1 -exec mv -t "$d-old" {} +
   done
   sudo find /srv/acache/tapectl-staging -mindepth 1 -delete
   ```

   The move is required: a 1.x catalog is refused by name, and `first-run.sh` runs `init`
   when the home has no catalog.
4. **Delete the keepsake dotfiles,** so that `collection sync` mints new unit identities. A
   `[policy]` table in one is a setting made by hand (`collection sync` writes none); note
   it before deleting, because the key it sets may be gone (`allow_spanning` is):

   ```bash
   D=/srv/keepsake/original-data
   sudo find "$D" -name .tapectl-unit.toml | wc -l
   sudo find "$D" -name .tapectl-unit.toml -exec grep -l '^\[policy' {} +
   sudo find "$D" -name .tapectl-unit.toml -delete
   ```

5. **Install 2.0.0 and build the new home:**

   ```bash
   scripts/first-run.sh --profile contrib/hosts/home2.profile --to 12
   ```

   - **step 2** checks the tar against the floor and refuses an older one;
   - **step 3** builds the release binary from the `v2.0.0` checkout and installs it;
     `tapectl --version` reads `tapectl 2.0.0 (v2.0.0, <date>)`;
   - **step 5** creates the service user's home, **step 7** adopts the escrow key from
     `ESCROW_PUBLIC_KEY`, mints the new operator key, writes `[tar] binary`, and with
     `OPS_GROUP` writes `[ops] group` again;
   - **step 8** registers the drive with `post_warning_bytes = 99008M` from the profile;
   - **step 9** generates the Heir Kit in `~/tapectl-heir-kit` (answer yes if it finds the
     old one: its `catalog.db.age` is the old catalog), with this release's static binary;
   - **step 11** mints new keys for `parents` and `family`, registers the `keepsake`
     collection and its folders as new units;
   - **step 12** is the rehearsal on the test cartridge EW7VWMVKF6, with the installed binary:
     type its serial. It is required before the first write.

   Then check where it went: `cat /etc/tapectl/home`, and `tapectl-op status` (units: 27
   registered).

## The cartridges (step 3 of the plan)

Each cartridge is erased with `cartridge erase` and its typed label, then written. They are
written one at a time, with the drive-poll timer stopped, so `keepsake` always has a copy on
tape as soon as the first one reads back.

1. **Copy 1.** Unload the test cartridge, load L6-0001, and run:

   ```bash
   sudo systemctl stop tapectl-drive-poll.timer
   scripts/first-run.sh --profile contrib/hosts/home2.profile --from 13 --label L6-0001 --location home2-rack
   sudo systemctl start tapectl-drive-poll.timer
   ```

   Step 13 snapshots every keepsake unit, stages them (two at a time), shows `stage list` and
   `volume plan`, checks that the host is quiet and asks you to type `L6-0001` before it
   writes. Its `volume init` refuses the Layout 2 volume on the tape, which it names ("a
   tapectl volume of Layout 2, label L6-0001"), and offers `cartridge erase`: type
   `L6-0001`. The erase registers the cartridge by its chip serial and the `volume init`
   after it gives it the volume's label as its barcode. The write ends with `sealed and
   confirmed: …; full readback passed` and moves the tape to `home2-rack`. Every unit stays
   staged: one copy is short of the copy floor of two.

   **Every unit continues on the next cartridge at the drive's early warning** (item 288), so
   `keepsake`, at 1.2 TiB, may or may not fit one LTO-6: when it does not, the unit the early
   warning ends is a range on this tape and its tail is written first on the next. That is
   not a failure: the seal says where the tape ended, and `tapectl-op volume list` and
   `stage list` show what is owed.
2. **Copy 2.** Eject L6-0001 (`tapectl-op drive eject`), load L6-0002, tell the coordinator
   first, and run the same command with `--label L6-0002 --location offsite`. Step 13 notes
   `1 volume(s) already in the catalog`: answer yes to writing another tape. Its
   `volume init` offers the erase for the old volume as for copy 1. If the refusal instead
   names a sealed volume of THIS catalog, L6-0001 is still in the drive: nothing is erased,
   so eject it, load L6-0002 and run the command again. This write completes the copy policy,
   so it releases each unit's staging.
3. **Finish.**

   ```bash
   tapectl-op audit                          # keepsake: 2 copies, 2 places (and drill_age, until step 4)
   tapectl-op cartridge list                 # L6-0001 and L6-0002, each with its chip serial
   sudo systemctl start tapectl-backup.service
   ```

   Copy 2's step 13 regenerated the Heir Kit with both tapes in its catalog. Print its
   `COVER.txt` and replace the one in each envelope, with the new `catalog.db.age` and
   `tapectl-2.0.0` on the media; the handwritten secret stays. A new escrow identity also goes
   into the local profile for the next rebuild.

## The drill (step 4 of the plan; #548)

A whole-tape restore at real scale with the kit's keys, and the steps of File 1 followed on a
real cartridge: `RESTORE.sh` from the tape, with the stock tools alone (`age`, `tar`, `mt`,
`dd`, `sha256sum`). Run `RESTORE.sh --info`, `--verify`, `--find-envelope`, `--list` and a
`--restore --all` into a scratch destination, and compare. Then record it:

```bash
tapectl-op drill record L6-0001 --note "whole-tape restore at real scale"
```

The record names the volume and the sha256 of the `RESTORE.sh` it carries, and starts the
yearly clock: `audit` warns (`drill_age`) when the newest record is older than `[defaults]
drill_interval_days`. If anything fails, read it before the next step writes the same
cartridge (a harness FAIL is evidence), and send the coordinator the output.

## The verdict (item 268)

If the drill passes, the cartridges are production and the on-tape authorization ends there:
after it, a failing byte pin in `tests/on_tape_golden.rs` is a CTO decision, never an agent's
re-pin. If it fails, they are test articles again, the fix is a new release (never a re-pushed
tag; the minor moves if on-tape bytes move), and the run is repeated by another fresh start
with new keys and a new kit.

**Tidy, once the new L6-0002 has read back in full:**

- delete the old state:
  `sudo rm -rf /srv/archive_meta/tapectl/home-1.9.2 /srv/local_backup/tapectl-old /var/backups/tapectl-old`;
- delete the rollback binaries of 1.x that no catalog survives for;
- give `parents` and `family` their new key files, since the old ones open only the erased
  tapes;
- with a new escrow identity, destroy the old paper.

Send the coordinator the session's output (step 7's excepted) for the run record.

## Decisions already ratified: do not re-litigate

- ADR-0012 and its amendments, whole, and ADR-0014, which gathers items 239 onward into the
  record of Layout Version 3. The items a handoff most needs: 242 (no reverse compatibility),
  244 (a production run is a fresh start), 245 (the cartridges can be rewritten), 248 and 262
  (production waits for the whole build, the drill and the reads), 263 and 268 (the first
  production run is the acceptance, and its order), 269 (a Layout 2 cartridge), 273 to 275
  (the standard is not lowered to home2's toolchain: home2 gets a tar that meets the floor),
  281 (the design is not sized to `keepsake`) and 288 and 289 (every unit continues at the
  early warning, and the queue is filled to it).
- The process rules: `.claude/skills/unattended-run/SKILL.md` and the autopilot policy block.

## Records

| What | Where |
|---|---|
| What 2.0.0 changes, for an operator | [releases/2.0.0.md](releases/2.0.0.md) |
| The format, byte by byte | `docs/design/volume-format-v3.md` |
| The build, task by task, and the acceptance run | `docs/design/v3-implementation-plan.md` (Stage 7) |
| The coordinator's own entry | `docs/coordinator-handoff.md` |
| Hardware facts from the first LTO-6 session | `docs/lto6-session-journal-2026-09-10.md` |
| The forensics record: every contact sweeps and journals verbatim | ADR-0013 and its amendments |
| The install procedure | `scripts/first-run.sh`; [install.md](install.md) |
| What each 1.x run did on home2 | `docs/runs/` |

## Hazards to keep in mind

- **Device numbers move across reboots.** The real drive is
  `/dev/tape/by-id/scsi-HUJ808A5L4-nst`, and the mhvtl drives are `scsi-XYZZY_A*`. Every
  harness and `first-run.sh` refuse to default to a device.
- **The drive belongs to home2.** Lending it to the development VM means attaching
  `contrib/hosts/home2-lto6-hostdev.xml` there. The two hosts share no tape lock, so detach it
  again before any production contact.
- **The rehearsal and the writes run on the production host.** Run them only while no
  production tapectl process runs, and keep the rehearsal on EW7VWMVKF6. Nothing enforces
  that: the scripts' tape lock (`/tmp/tapectl-tape.lock`) is taken by scripts only, and the
  binary's drive lock lives under each catalog's home. Check `tapectl-op drive status` (it
  names a holder's pid) and stop the drive-poll timer, which opens the sg node daily, for the
  duration. The audit and backup timers never open the drive.
- **A sealed tape is erased in the drive with `tapectl cartridge erase`,** never with a
  degausser, and a Layout 2 tape is erased with its typed label like any other.
- **MAM's "remaining capacity" is not host-writable space.** The drive stops host writes at
  its early-warning point with about 107 GB still "remaining". tapectl plans against the
  generation table times the fill ceiling (`fill_ceiling`, 97% by default), and past it to the
  early warning on a drive that declares `post_warning_bytes`.
- **A bursty host feed costs tape** (1.48 native bytes per data byte measured); a steady one
  does not. Keep the host quiet (`tapectl host check`).
- **The write's confirm is quick by default.** `first-run.sh` step 13 writes with
  `--full-confirm`, whose read-back of every file is the tape's first full verify.
