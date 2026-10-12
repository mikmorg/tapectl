# Handoff: where tapectl stands, and what only you can do

Rewritten 2026-10-12 for the fresh start on 1.1.0 (ADR-0012, amendment "2026-10-12",
items 36–48). Earlier versions are in git history. The 2026-09-24 version planned the
first write on 1.0.0, and its 2026-09-28/29/30 notes were the upgrade steps for 1.0.x.
This version supersedes both. The file answers one question: **which remaining work needs
a person, and which does not?** What changed in 1.1.0 is in
[releases/1.1.0.md](releases/1.1.0.md).

## The state, in one paragraph

home2 runs 1.0.0. That install wrote one production tape, L6-0001, holding the only tape
copy of `keepsake` (sealed 2026-10-01). It also exposed the defects that 1.0.1–1.0.6 and
1.1.0 fix. 1.1.0 is frozen on master `4342589`, untagged. It changes on-tape bytes and
adds migrations 027–035, so the CTO ruled that **production starts over instead of
upgrading** (item 36):
- a new catalog from `scripts/first-run.sh` step 1;
- the escrow identity kept (item 39), and every tenant key minted fresh, the operator's
  included (item 41);
- the 1.0.0 home and its staging deleted (item 38);
- L6-0001 erased and reused (item 37).

From the erase until two new copies seal and verify, `keepsake` has no copy on tape. Its
originals stay on disk, and the CTO accepted that window.

The whole chain is green, per #418's landing comment. It ran on land6 `01c4fb8`: fmt,
clippy (gnu and musl), every ungated test binary, check-docs, mhvtl_e2e 12/12,
mhvtl_motions, the mhvtl gate against an empty manifest, lifecycle `--all`, and the
first-run re-entry acceptance. `4342589` then added only the committed gen-1/gen-2
`catalog.db` fixtures, and the lib test binary was rerun green on it. What 1.1.0 has not
had is a real drive.

## Who does what

| | Step | Who |
|---|---|---|
| (a) | The 1.1.0 real-drive proof on home2, on the test cartridge only | an agent, **on your go-ahead** |
| (b) | Tag `v1.1.0` on the proven commit | an agent, once (a) is green |
| (c) | The fresh start on home2 | **you** (sudo, the escrow key, the cartridges) |
| (d) | `keepsake` on two cartridges the same day, then the Heir Kit | **you** |
| (e) | The backlog | agents, in the order below |

## (a) The 1.1.0 real-drive proof on home2

An agent does this, and only when all of these hold:
- the CTO has said go;
- no production tapectl process is running on home2 (`pgrep -a tapectl` prints nothing);
- the cartridge in the drive is the test cartridge **EW7VWMVKF6**, checked by its chip
  serial (`sg_read_attr`).

Nothing in (a) touches the production home `/srv/archive_meta/tapectl/.tapectl` or a
production cartridge.

1. **Build on vm-desk1, not on home2.** home2 is Debian 10, with glibc 2.28, which is too
   old for a binary built on vm-desk1. A static musl debug build runs there (`84b9c39`
   made the tape ioctls compile under musl):

   ```bash
   CARGO_TARGET_DIR=/scratch/tapectl-target-home2 flock /scratch/tapectl-build.lock \
       cargo build --target x86_64-unknown-linux-musl
   # → /scratch/tapectl-target-home2/x86_64-unknown-linux-musl/debug/tapectl
   ```

   Copy the binary to home2 (for example to `~/.cache/tapectl/tapectl-1.1.0-musl`). Check
   that `--version` names the frozen commit, and that home2's checkout of the repo is at the
   same commit: the scripts below come from it. home2 has no `/scratch`, so throwaway
   homes go under `~/.cache/tapectl` (the profile's `WORK_DIR`).
2. **The step-12 rehearsal with this binary:** `scripts/first-run.sh --profile
   contrib/hosts/home2.profile --home ~/.cache/tapectl/proof-home --from 12 --to 12
   --tapectl <musl binary>`. This runs the lifecycle suite's `first-year` scenario as
   you, with throwaway homes, erasing EW7VWMVKF6 after you type its serial.
   - **The restore proof.** The scenario's restore matrix includes RESTORE.sh restores
     read off the tape (`restore_sh_dd`, `restore_sh_primary`, `restore_sh_backup`): that
     is the RESTORE.sh proof.
   - **Why `--home`.** Without it, first-run's preamble reads the production
     `config.toml`. If that file still has `usable_capacity_factor`, the preamble offers
     to rename it to `fill_ceiling`. Answer **no** to any such offer during (a): the
     installed 1.0.0 does not know `fill_ceiling` and would refuse its own config.
3. **A catalog rebuild from the tape:** the lifecycle suite's `db-loss` scenario, run the
   way step 12 runs `first-year`. It covers `catalog rebuild` from tape, the DB-less
   raw-volume import and the pure heir path:
   `TAPECTL_BIN=<musl binary> TAPECTL_LIFECYCLE_OUT=~/.cache/tapectl/tapectl-lifecycle
   PATH=/usr/local/bin:$PATH:/usr/sbin:/sbin scripts/lifecycle-suite.sh --scenario db-loss
   --device /dev/tape/by-id/scsi-HUJ808A5L4-nst --erase short --single-cartridge
   --i-will-lose-the-cartridge EW7VWMVKF6`.
4. **The measurements, with a scratch `TAPECTL_HOME` (or `--home`)**, never the production
   catalog:
   - **#366 and #368:** the recipe in #366's last comment. #366 sets `hash_threads` to 1,
     2, 4 and 8 on `keepsake/20090101.sismarie-pictures`. #368 runs `--jobs` 1, 2 and 4
     on a mixed set of keepsake units. For each run, record the wall clock,
     `phase_timings`, iostat, the peak RSS and the sha256 columns. The source must be
     readable by the account the proof runs as. On home2, step 11's ACLs grant read to
     the `tapectl` user only.
   - **#326:** write throughput and full-readback throughput on EW7VWMVKF6, from
     `phase_timings`, the session log and st's counters. The issue's baseline is 56 MB/s
     write and about 16 MB/s confirm (2026-09-23). Its comments account for L6-0001's
     26 h on 1.0.0 with #389, #390, #399 and 1.0.3's double hash, all fixed since.
   - **A debug build is unoptimized**, so its hashing and encryption are far slower than
     a release build's. Compare runs with each other: for #366/#368, the shape across N
     is the result. Do not compare its MB/s with a release baseline. If a figure needs
     release speed, ask the CTO first: a release build is never made without the word.
5. **Record it** in `docs/runs/<date>-1.1.0-real-drive-proof.md`, and post the
   measurements on #326, #366 and #368.

Exit criterion: steps 2 and 3 GREEN on the frozen commit. If either is red, nothing is
tagged and the fresh start waits.

## (b) Tag v1.1.0

On the commit (a) proved, by the version rule of 2026-09-30:
`git tag -a v1.1.0 <sha> -m "tapectl 1.1.0"`, then `git push origin v1.1.0`. If (a) needed a
fix, the fix gets its own proof run before the tag. Installing v1.1.0 ends the on-tape
authorization of 2026-10-07 (item 40): from then on a failing `tests/on_tape_golden.rs`
is a CTO decision again.

## (c) The fresh start on home2

You do this on home2, inside tmux, in this order. Read it through once first.

1. **Stop the 1.0.0 timers** so the daily catalog backup does not fire at a deleted home:
   `scripts/install-systemd.sh --uninstall`. This removes the units and the `tapectl-op`
   wrapper, never the user, a home, the binary or the backups. Step 14 installs all three
   timers again, the drive poll among them.
2. **Check out the tag:** `git fetch --tags`, then `git checkout v1.1.0`.
3. **The escrow public key** (item 39). Copy the profile out of the checkout and fill in
   the copy: `cp contrib/hosts/home2.profile ~/home2-1.1.0.profile`, then set
   `ESCROW_PUBLIC_KEY=` in it to the `age1…` line on the Heir Kit's cover sheet. The public
   key is not secret. Why a copy: tapectl's build identity is `git describe --tags --dirty`
   (`build.rs`), so an edit to the tracked `contrib/hosts/home2.profile` would make step 3's
   binary call itself `v1.1.0-dirty` in every journal row it writes.
   - Before step 4 deletes the old home, compare the key with the file the old home kept:
     `sudo cat /srv/archive_meta/tapectl/.tapectl/keys/mikmorg-escrow.age.pub`. The two
     must be identical.
   - Step 7 then adopts the key (`init --escrow-public-key`) instead of minting one, so the
     handwritten secret in the sealed kit stays valid.
   - No command can replace an escrow identity later (ADR-0005). Without the key in the
     profile, step 7 asks for one, and an empty answer mints a new identity.
4. **Delete the 1.0.0 home and its staging** (item 38).
   - **What gets deleted.** The tapectl home is `/srv/archive_meta/tapectl/.tapectl`,
     inside the service user's home `/srv/archive_meta/tapectl` (`first-run.sh` computes
     `<service home>/.tapectl`). It holds the 1.0.0 catalog, keys, config, logs, stage
     reports and dar catalogues. Staging is `/srv/acache/tapectl-staging`.
   - **Nothing needs to be copied out.** The tenant keys are minted fresh (item 41). The
     escrow secret is on paper, and its public half is now in the profile copy. If that
     ever changes, and someone keeps using a 1.0.0 tenant key, the path is runbook F of
     [keys-and-recovery.md](keys-and-recovery.md#f-keeping-existing-keys-on-a-fresh-home),
     done before this step: it copies the secrets out and adopts them through
     `ADOPT_KEYS_FROM`.

   Then delete:

   ```bash
   pgrep -a -u tapectl                                     # nothing may be running
   sudo rm -rf /srv/archive_meta/tapectl/.tapectl
   sudo find /srv/acache/tapectl-staging -mindepth 1 -delete
   ```

   Keep the two directories themselves. `/srv/archive_meta/tapectl` is the service user's
   home, which `home2-prep.sh` moved there, and the staging directory is the profile's.
   This step is required: step 7 skips `init` whenever `<home>/tapectl.db` exists.

   Not ruled: the 1.0.0 catalog copies in `/srv/local_backup/tapectl` (they hold no keys).
   Ask the CTO whether to keep them.
5. **Prepare the host:** `contrib/hosts/home2-prep.sh --apply`. On home2 most of its parts
   report "already done". It does not delete the 1.0.0 home: its part 2 removes only the
   2026-09-13 attempt under `/var/lib/tapectl`. That is why step 4 is by hand.
6. **Run `scripts/first-run.sh --profile ~/home2-1.1.0.profile --to 12`**, from step 1.
   What differs from a first install:
   - **step 3** builds the release binary from the `v1.1.0` checkout and installs it to
     `/usr/local/bin`. `tapectl --version` should read `1.1.0 (v1.1.0, <date>)`;
   - **step 7** says it is adopting the escrow key from `ESCROW_PUBLIC_KEY`, and mints no
     escrow identity. It does mint a new operator key. With `OPS_GROUP=tapectl-ops` it
     creates the group, adds `tapectl` and you to it, and writes `[ops] group`;
   - **step 9** finds the old kit in `~/tapectl-heir-kit`. Answer **yes** to regenerate it:
     the old kit's `catalog.db.age` is the 1.0.0 catalog;
   - **step 11** mints new keys for `parents` and `family` and registers the `keepsake`
     collection;
   - **step 12** is required again. Its marker is per binary hash, and the release binary
     from step 3 is not the musl build that (a) proved. It erases EW7VWMVKF6 again.
7. **Unload the test cartridge, load L6-0001, and erase it in the drive** ("L6-0001 at
   its volume init", below, says why and how). Then run `scripts/first-run.sh --profile
   ~/home2-1.1.0.profile --from 13`. Step 13 writes L6-0001, verifies it in full, audits,
   moves it to `home2-rack` and regenerates the kit. Step 14 installs the timers and the
   wrapper.
8. **The ops group check (#393, still owed).** Log in again, because group membership
   takes effect at login. Then run `tapectl --home /srv/archive_meta/tapectl/.tapectl
   status` from your own account, without sudo. It must list the sessions. Record the
   result on #393.
   - **If it says "permission denied",** look first at the directories above the home: a
     member must be able to traverse each one. `/srv/archive_meta` is root 0750, and
     `home2-prep.sh` grants `x` on it by ACL to the `tapectl` user only.
   - **Which path to use.** Some docs spell this path as `/srv/archive_meta/tapectl`,
     which is the service user's home. Use the path step 7 prints.

### L6-0001 at its volume init

The new catalog knows neither the volume nor the cartridge. The cartridge still holds the
sealed 1.0.0 volume, and its File 0 says so. `volume init` reads File 0 and then probes for
a seal marker **at the position the tape's own File 0 gives, before it compares labels**
(`session::contact_report`). So reusing the label L6-0001 does not make the tape
"match":
- **If L6-0001 is loaded unerased,** init refuses: "the loaded cartridge already carries
  a SEALED volume … ADR-0003 … --force cannot override this". Step 13 explains this and
  stops. The refusal's remedy names `volume retire` and `cartridge mark-erased`. Neither
  applies here, because this catalog has no row for either (init refuses before it
  registers anything). Only the erase in the drive does. By then step 13 has already
  snapshotted and staged. Erase, reload, and re-run `--from 13`: the snapshots are
  unchanged and mint nothing, and the staged units are skipped.
- **So erase it in the drive first**, between `--to 12` and `--from 13` (step 7 of (c)),
  with L6-0001 loaded. Never degauss an LTO cartridge. There are two ways:
  - **The short way** (seconds): `sudo -u tapectl -H mt -f
    /dev/tape/by-id/scsi-HUJ808A5L4-nst rewind`, then the same with `weof 1`. File 0 is
    now an empty filemark. At init, tapectl refuses it as "File 0 is EMPTY" and does not
    take it as blank (ADR-0003 fails closed). Step 13 explains this and asks you to type
    the label to OVERWRITE, then runs `volume init L6-0001 --force`.
  - **The long way** (`mt … erase`, hours on LTO-6): the drive reports end of data at
    file 0, the tape proves itself blank, and init proceeds with no refusal and no
    `--force`.
- **Either way, the consent is yours, typed:**
  - step 13's `WRITE volume L6-0001 … (the cartridge's current contents are
    overwritten)` asks you to type the label;
  - after the short erase, the OVERWRITE confirmation asks for it again.

  Init then reads the chip serial, finds no cartridge row, and **registers the cartridge
  itself**: `cartridge <serial> auto-registered from MAM`, with the serial as a
  placeholder barcode. Step 13 prints `cartridge relabel <serial> <barcode>` for the
  sticker. The write follows. Then comes `volume verify L6-0001 --full`, the first full
  readback (the write's own confirm is quick by default), then `audit`, the move to
  `home2-rack`, and the kit.
- The same cartridge's earlier history (the 1.0.0 catalog's binding, events and
  journals) went with the old home. The new catalog starts its record at this contact.

## (d) keepsake on two cartridges, then the Heir Kit

`keepsake` has no copy on tape until both new copies seal (item 37), so write copy 2 the
same day:
1. Leave staging alone: the write does not release it. Load a second fresh cartridge,
   which must be a different cartridge from L6-0001.
2. Run `tapectl-op volume init L6-0002 --device /dev/tape/by-id/scsi-HUJ808A5L4-nst`,
   then `tapectl-op volume write L6-0002 --device …`, then `tapectl-op volume verify
   L6-0002 --device … --full`.
3. Run `tapectl-op volume move L6-0002 --to offsite`. If step 14 has not installed the
   wrapper yet, use `sudo -u tapectl -H tapectl` in place of `tapectl-op`.
4. `tapectl-op audit` should now find `keepsake` at two copies in two places (the
   defaults are `min_copies = 2` and `min_locations = 2`). Then:
   - `tapectl-op staging clean`;
   - `sudo systemctl start tapectl-backup.service`;
   - refresh the Heir Kit with `scripts/first-run.sh --profile
     ~/home2-1.1.0.profile --from 9 --to 9` and answer yes to regenerate. Reprint
     `COVER.txt`, put it with the handwritten escrow secret from the existing sealed
     kit, and keep two copies in independent failure domains.

`audit` warns `escrow_kit_stale` after every write session: that is the cue for the next
refresh.

## (e) After: the backlog

None of these blocks the fresh start. Each has an issue:
- **#326, #366, #368:** close them from (a)'s measurements. Set the `[staging]
  hash_threads` and `jobs` defaults from the knees. Plan the write-throughput follow-up
  from #326's numbers.
- **#425:** `min_locations` is checked only at `mark-tape-only`. The fix is one
  predicate, used by `audit`, `mark-tape-only` and `mark-reclaimable`.
- **#420:** the short seal, once home2's writes show how much capacity they really use
  and the drive's behaviour past early warning is measured. It is a 1.x on-tape change:
  a CTO decision and a re-pin.
- **#418 R4:** the cross-volume per-tenant index (1.2.0 or later).
- **#388:** Logical Block Protection: research, then a prototype on EW7VWMVKF6 only.
- **#409:** stays "not until spanning".
- **#416:** the perf leg inside the gate, and `mhvtl_motions` in the gate's Rust leg.
  Both are `scripts/` changes, so they need the whole gate chain.
- **#364 epic:** #369 (tune the dar invocation) and #371 (chunk-parallel age, only if
  still the bottleneck).
- **#393:** closes on the check in step 8 of (c).
- **2026-10-07 item 6:** the read-error rate assumes the drive clears log page 0x03 at
  load. Check it on home2's first two full verifies (L6-0001 and L6-0002), and revisit
  the rate if the drive keeps the counters.

## Decisions already ratified: do not re-litigate

- **2026-09-14:** ADR-0012 (copies are identical content, cartridges are known by
  serial), the dated corrections in ADR-0010/0011, and the `CONTEXT.md` vocabulary.
- **2026-09-16 to 2026-09-30:** the dated amendments in ADR-0012 and ADR-0013, the
  production host (home2, 2026-09-28) and the version rule (2026-09-30).
- **2026-10-06:** confirm quick by default, the 1.1.0 RESTORE.sh and the restore
  rulings, plaintext-free staging, a single source read, per-file data once per unit,
  X25519-only, the fill ceiling and the short seal later, 1 GiB slices, no scrub, no
  parity, and forward compatibility.
- **2026-10-07:** items 1–35: the write family's exit table, `audit` 70, readback
  freshness by start, `--policy-aware` and copy distinctness, `config set/add/remove`,
  the drive poll, journals never pruned, and the dotfile not counting as content.
- **2026-10-12:** items 36–48: the fresh start, L6-0001 erased, the old home deleted,
  the escrow identity kept, new tenant keys, the adopt path, the tape-only multiplier on
  copy floors only (and as a consent gate), tenant envelope backups and `catalog.db`
  generation 3 in 1.1.0, and #416's counters test-only.
- The process rules: `.claude/skills/unattended-run/SKILL.md` and the autopilot policy
  block.

## Records

| What | Where |
|---|---|
| What 1.1.0 changes, for an operator | [releases/1.1.0.md](releases/1.1.0.md) |
| The rulings | ADR-0012's amendments of 2026-10-06, 2026-10-07 and 2026-10-12 |
| The real-drive rehearsal of 1.0.0, DR from real tape, the end-of-tape fill | `docs/runs/2026-09-23-real-drive-rehearsal.md` |
| MAM capacity units and the feed-rate effect | `docs/runs/2026-09-23-lto6-capacity-measurement.md` |
| Hardware facts from the first LTO-6 session | `docs/lto6-session-journal-2026-09-10.md` |
| The forensics record: every contact sweeps and journals verbatim | ADR-0013 and its amendments |
| The install procedure | `scripts/first-run.sh`; [install.md](install.md) |
| The research behind 1.1.0's staging and catalog changes | `docs/research/2026-10-06-plaintext-free-staging.md` |

## Hazards to keep in mind

- **Device numbers move across reboots.** The real drive is
  `/dev/tape/by-id/scsi-HUJ808A5L4-nst`, and the mhvtl drives are `scsi-XYZZY_A*`. Every
  harness and `first-run.sh` refuse to default to a device.
- **The drive belongs to home2.** Lending it to vm-desk1 means attaching
  `contrib/hosts/home2-lto6-hostdev.xml` there. The two hosts share no tape lock, so
  detach it again before any production contact.
- **The proof (a) runs on the production host.** Run it only while no production tapectl
  process runs, and keep it on EW7VWMVKF6. The tape lock `/tmp/tapectl-tape.lock` is per
  host, so the lifecycle suite and a production command exclude each other on home2: a
  second user is refused, not queued. The 1.0.0 audit and backup timers never open the
  drive. vm-desk1 shares home2's CPUs, so keep it quiet while home2's tape runs.
- **The 1.1.0 release still refuses a sealed tape whatever its label** (above): erase in
  the drive, never with a degausser.
- **MAM's "remaining capacity" is not host-writable space.** The drive stops host writes
  at its early-warning point with about 107 GB still "remaining". tapectl plans against
  the generation table times the fill ceiling (`fill_ceiling`, 97% by default), never
  against MAM remaining. A late end of tape is a clean abort to an unsealed tape (exit 5)
  until #420.
- **A bursty host feed costs tape** (1.48 native bytes per data byte measured); a steady
  one does not. Keep the host quiet (`tapectl host check`).
- **The write's confirm is quick by default.** Run `volume verify --full` on each new
  copy before `staging clean`.
- **The on-tape authorization ends with the v1.1.0 install.** After it, a failing
  `tests/on_tape_golden.rs` is a CTO decision, never an agent's re-pin.
