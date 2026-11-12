# Design and defect review — 2026-11-12

**Asked for:** "Recommend improvements in design, and review for bugs, gaps", by the CTO on
2026-11-12, after the intent and proportion review of the same day
(`2026-11-12-intent-and-proportion-review.md`). Master `a200e444`; home2 on 1.9.2; `keepsake` on
L6-0001 and L6-0002.

**What this is.** The last adversarial read of the code ended at `v1.1.1`
(`2026-10-15-1.1-diff-review.md`). About 130,000 inserted lines of `src/` since then (1.1.2 to
1.9.2: the index, the restore repairs, `volume rewrite`, the short seal, spanning, per-location
targets, Logical Block Protection, the one stream into dar, the upgrade, the CLI review, the
friends' setup) passed their gates and chains and were never read that way. This is that read,
with a walk of the production record for gaps and a list of design improvements. It fixes
nothing, files nothing, moves no pin and changes no ruling. Its decisions continue the intent
review's list (§9, D8 to D13).

**Method.** Read-only: no cargo, no tape, no production host. Thirteen finders, one per lens,
under the 2026-10-15 review's rules (§11), each given raw scope and production facts and none of
the intent review's conclusions. Every finding carries an exposure tier:

- **A** — bytes already on the two production cartridges. These cannot be fixed without
  rewriting tape.
- **B** — code and scripts home2 runs today.
- **C** — built and not used in production.

The lenses: the heir script's main restore; its other modes; File 1, RECOVERY.md and the Heir
Kit, as a paper run of the manual steps; the on-tape content and its readers; the production
write path (these five on the session's model); the tape layer; coverage and `audit`; staging;
the host scripts; the recovery paths; the unused features; ADR-0012 items 76 to 234 against the
code; and a completeness pass over what those reported as not reached (these eight on sonnet).
A rate limit stopped the first five part-way; each was resumed from its own transcript and
finished. The three heir lenses ran pieces of the generated script and the manual steps on
synthetic input with real `dar` 2.7.13 and `age` 1.1.1 and with gawk 5.2.1, mawk 1.3.4 and
busybox awk 1.36.1, on files, never a device.

The coordinator read the cited lines of the high, the conditional high and every medium,
reproduced the high with dar alone (§2), and read a sample of the lows. Lows otherwise rest on
their finder's trace. "Read" below means read and not run; no finding here was confirmed by
building tapectl. The coordinator built most of what is reviewed, which is why the finders read
it and why the walk of the record (§7) was done from the run records and not from memory.

## 1. The result

52 numbered findings from the thirteen lenses, two of them reported by two lenses each: **1
high (H1), 1 high that today's tapes cannot reach (B1), 14 medium**, the rest low, and seven
concerns not traced to an outcome (§6).

- **The whole-unit restore of the production tapes held.** No path was found by which an heir
  restoring a unit, or all of them, gets wrong or partial data called whole. The manual steps
  of File 1 were carried to a restored file for the first time (below).
- **The high is on both tapes** (§2): the dar catalogue in every envelope carries slice numbers
  that mean nothing, so RESTORE.sh's one-file restore (`--path`) and its fill from the other
  copy (`--fill-from`) do not work on any tape written since 1.1.0, File 1 and RECOVERY.md say
  something false about it, and the fill can empty the damaged file it was asked to repair
  (shown with dar on the script's own command line, not by a run of RESTORE.sh end to end).
  The whole-unit restore and slice replacement do not use those numbers.
- **The write path, the copy count and staging held** (§4): nothing seals, counts, releases or
  moves without a passing confirm; no source file can be left out of an archive without a
  refusal; no ruling of items 76 to 234 was found built differently.
- **Eight mediums are in what home2 runs**, none on the normal course of a write: two in what
  `audit` cannot see (B3, G2), one in who hears of a failure (G3), two in `db import` (B4,
  B5), one in the next re-staged copy (B2), one in a resume after an early warning (B6), one
  in the escrow key's parsing (B7). Five more are in texts and modes on the tapes or in the
  kit (A1, A6, A10, A11, K1) and one in an unused command (C1).
- **The gaps are in evidence and feedback, not code** (§7): nothing has decrypted a byte of the
  production tapes, nothing scheduled notices a new folder, a failed timer tells nobody, and one
  drive wrote and read both copies.

**The manual steps, run for the first time.** The intent review found no run that carried File
1's "Manual Recovery Steps" to a restored file. The paper-run lens executed steps 2 to 8 on a
stand-in tape built from the gate's 1.9.2 tape files (25 files, 2 tenants, units of 1, 12 and 1
slices, each file zero-padded to 512 KiB, `dd if=FILE bs=512k` standing in for `mt` and `dd`),
with the operator key: a restored file with the source's sha256. With the tenant key, steps 5
and 6 ran and the slices decrypt. Also run and right as written: the front-index hash against
the seal, `file_count`, the "short way", the missing-slice dar claims, rung 3's zero-strip, and
RECOVERY.md's awk and `sha256sum -c` lines. Not run: the escrow secret (the gate holds only
its public half), `eod` and `bsfm` and any real positioning. D4's drill is still owed on a real
cartridge; what it will meet is now known (A10, A11).

## 2. The high: the catalogue on every tape carries the wrong slice numbers

**H1, high, tier A.** `stage create` archives with `dar -c - ... -@ <on-fly>` and then makes the
envelope's catalogue with `dar -C <cat> -A <on-fly> -znone` (`src/dar/create.rs:44-50, 551-575`,
called at `src/staging/mod.rs:1199`; #419, in every release since 1.1.0). Isolating a catalogue
records the slice layout of the archive it reads, and the archive read here is the on-the-fly
catalogue file, a few hundred bytes to a few megabytes. So every slice number `dar -l <cat>
-T slice` prints from a catalogue on tape is the unit's offset divided by the size of a small
file. The paper-run lens found it by doing what File 1 says.

Reproduced by the coordinator with dar 2.7.13 alone, on six files of about 8.4 MB:

```
dar -c - -N -R src -@ onfly -Q > stream.dar     # as stage create runs it
dar -N -C cat -A onfly -znone -Q                # the catalogue that goes on tape
dar -l cat -N -Q -T slice    ->  d/f5.bin 1-2111 ... top.bin 10551-11817   "slice range [1-11817]"
dar -c arch -N -R src -s 1M -Q ; dar -N -C ctl -A arch -Q                 # control: 9 real slices
dar -l ctl -N -Q -T slice    ->  d/f5.bin 1-2 ... top.bin 8-9              "slice range [1-9]"
```

On the gate's own 1.9.2 operator envelope the finder measured `1-29778` for the one large file
of a 12-slice unit. home2 runs dar 2.7.21; the finder compared libdar's isolation code in both
versions and found it the same, and did not run 2.7.21.

What reads those numbers, all of it on L6-0001 and L6-0002:

- **File 1 and RECOVERY.md say something false.** "It lists every file of the unit and which
  slice holds it" (`src/volume/layout.rs:899-900`); "`dar -l env/catalogs/NAME -T slice -N -Q`
  ... shows which slices hold each file, so you can see beforehand what a lost slice costs"
  (2097-2099, 2066-2068); RECOVERY.md's "`-T slice     # which slice holds each file`" (8738).
- **`RESTORE.sh --restore --path` reads the wrong slices** (`restore_paths`, generated script
  3872-3946): it takes dar's "slice range [lo-hi]" and reads the manifest's slices inside it
  and the last. With the range far past the unit's last slice, only the last is read. Run with
  dar on the control archive with only the last slice present: dar exits 0, prints "not restored
  (user choice)" and leaves a 0-byte file; the script then says dar "declined to overwrite"
  files that already existed, in an empty directory (emulated by the finder with the script's
  lines; not run through RESTORE.sh end to end).
- **The same line refuses a path whose slices are not one run, with a right catalogue too.** A
  file in a folder is listed with its folder's slice. When the two do not adjoin, dar prints a
  list, `[1,4221-6331]` (`[1,3-5]` from the control), and the `sed` that reads it (3883) accepts
  only digits and dashes, so the script dies "'UNIT' version N holds nothing at" that path. Run
  by the coordinator on dar's real output. A path whose slices adjoin its folder's passes the
  `sed` (`[1-2]` in the control), so this half hides until a unit has more than a few slices.
- **`RESTORE.sh --fill-from` selects the wrong slices and can empty the file it is repairing.**
  It lists the catalogue the same way (5555-5625), expands each file's range into slice
  numbers, keeps the manifest's slices among them and the last (5662), and runs `dar -x ... -w
  -g FILE` (5737); numbers past the unit's last slice select nothing, and nothing stops the run
  before dar. Run by the coordinator with dar on the control: a root-level file whose slice is
  absent goes from 880,000 bytes to 0, dar exits 0, and nothing is filled. A file in a folder
  whose own entry is unread too is left alone.
  The script's own damage note sends the heir to this mode: "Another copy of this same unit
  and version holds them. Load that tape and run ITS RESTORE.sh ... --fill-from" (4192-4195).
  The file stays on the damaged list, so the run still ends INCOMPLETE.
- **tapectl's own `restore file` plans from the same numbers** (tier B;
  `src/volume/restore.rs:1571-1596`, `dar::restore::entry_slices`). It reads the subset only
  when every listed number is a slice of the volume, and every slice otherwise. Read, not run:
  for most files the numbers are past the unit's last slice, so it reads the whole unit and
  #411's saving never happens; for a file early in a large unit the numbers can all exist and
  name the wrong slices. tapectl's fill from another copy was not traced.

**Why nothing caught it.** Every `--path` and `--fill-from` test, and
`restore_file_reads_only_the_slices_that_hold_it`, builds its catalogue with the test-only
`extract_catalog` (`src/dar/create.rs:515-520`), isolating from a dar-sliced archive, which has
the right layout. The tests that cut as tapectl cuts restore whole units. No gate leg runs
`--path` or `--fill-from`. The gate's 12-slice unit works by accident: one file's range covers
every slice and the other is in the last. The fixture was built by another path than
production's.

**It is a regression of 1.1.0, and no record knew of it.** #411 measured `-Tslicing` on an
isolated catalogue at 1.0.5, when dar sliced the archive itself and the catalogue was isolated
from those slices: true then. 1.1.0 moved staging to `dar -c -` with tapectl's own cut (#370)
and to the on-the-fly catalogue (#419). The research note behind it checked that catalogue
against the cut slices with `dar -t ... -A`
(`docs/research/2026-10-06-plaintext-free-staging.md` §3.4), which passes; it did not list it
with `-T slice`. `docs/design/volume-format-v2.md` (line 1436) carries the same claim as File
1. No issue, ruling or run record mentions the numbers (searched: #411, #419, the note,
ADR-0012, the release notes, the 2026-10-15 reviews).

**What still works on these tapes:** a whole unit, or all of them, by RESTORE.sh, by File 1's
steps and by tapectl; `--list`; the per-file sha256 check (it reads `paths.tsv`); a bad slice
replaced from the other cartridge (`--swap-tapes`, `--slices-from`: by sha256, not by these
numbers). **What does not:** one file or folder by `--path`, and the fill from the other copy.
The way for an heir who wants one file, or whose restore ended with damaged files, is the whole
unit from the other tape.

The tapes cannot change without a rewrite. The Heir Kit's sheet and the troubleshooting guide
can say all of this today without moving a byte on tape, which is item 178's precedent. D8.

## 3. Tier A: the rest of what is on L6-0001 and L6-0002

Beside H1. For the whole-unit restore, no defect was found that gives an heir wrong or partial
data and calls it whole. The main restore was read end to end and its pieces run on synthetic
input under gawk 5.2.1, mawk 1.3.4 and busybox awk 1.36.1: the slice selection and space
arithmetic on a 27-unit, 1,253-slice manifest are identical in all three, byte counts print
with `%.0f`, and the per-file check passes names with spaces, tabs, newlines, backslashes,
leading dashes and non-UTF-8 bytes and catches both positive controls. The per-file sha256
check stands behind dar's exit status on every path read.

What follows is on the tapes for good. Line numbers are the generated script's
(`/scratch/tapectl-gate/run-20261111-150951/heir/RESTORE.sh`, the script these tapes carry but
for the label; the generator is `src/volume/layout.rs` and `src/volume/restore_script.rs`).

| id | Sev | Finding | Checked |
|---|---|---|---|
| A1 | medium | **A full or unwritable work directory while an envelope is unpacked is reported as a damaged envelope.** `try_envelope` stops for "no space left" only on the `dd` read (1588-1592). The `age -d \| tar xf -` step (1611-1628) calls every failure except "no identity matched" DAMAGED, tries the backup envelope, which fails the same way, and ends in `die_no_envelope`. The script's own warning says "If it fills, the run stops and says so" (1659-1663). tar's own words are printed under the warning, so a careful reader sees the cause; the verdict line is still false. The work directory is `${TMPDIR:-/tmp}`, a small tmpfs on many systems, and these tapes' tenant envelope holds 27 dar catalogues and the path index. | read by the coordinator; reproduced by the finder with the script's functions and a `tar` stub; no test (the one disk-full test stubs `dd`) |
| A2 | low | **The exit trap sends SIGKILL to every process id the run ever started.** `BG_PIDS` only grows (187, 2321, 2376) and `cleanup` kills each entry (188-197), about 1,280 of them reaped hours before the end of `--restore --all`. The per-file check forks once per restored file (4690), so where `pid_max` is 32768 the counter wraps during the run and a remembered id can belong to an unrelated process; as root, any process. Restored data is not touched. Latent where `pid_max` is 4194304 (systemd's setting). | `cleanup` read by the coordinator; the mechanism shown by the finder; a real wrap was not produced |
| A3 | low | **`--all` checks space once as if every unit streams.** On a host that must spool (dar before 2.7.9, or `stream_check` failing) each unit is checked again for twice its size (2559), so the 470.5 GiB unit can be refused part-way through the run, with a text that says the restore streams and "No slice has been read yet" (341-352). | traced by the finder; not run |
| A4 | low | **The index programs compare unit names as numbers.** `unesc(val(f, h, "unit")) != unit` compares two numeric-looking strings numerically, so units named `1.1` and `1.10` are one unit to the file check, `--where` and the range lookup (`restore_script.rs:503-505, 660, 944, 1007, 1116`). The production units are `keepsake/...`, so these tapes cannot reach it. | run by two finders independently: the file-check programs misfire in all three awks, `--where` in gawk and mawk |
| A5 | low | **`--find-envelope` ends with a command that refuses on these tapes.** "To restore, run: $0 --restore --key K --to /your/destination" (1810-1811); with 27 units that refuses with "multiple units found" (3636-3639) after a pass of the tape. The refusal names the fix. | traced by the finder |
| A6 | medium | **With `--swap-tapes`, a wrong cartridge ends the restore.** `swap_fetch` reads the offered cartridge's File 0 in the main shell (5270-5278, `other_tape_map \|\| return 1`), so a blank, unreadable, foreign or other-layout cartridge is a `die`: the run ends, the exit trap removes the scratch directory with every slice decrypted so far, and no summary or damaged-file list is written. `span_accept` puts the same read in a subshell and asks again (3324-3337, with a comment saying why). The prompt says "Type done ... to go on without them". This is the repair these two cartridges exist for: a bad slice on one, fetched from the other. Loading the right cartridge works; `--slices-from DIR` is not affected. | read by the coordinator; run by the finder on three bad cartridges with a fake `mt` and `dd`; no test |
| A7 | low | **`--fill-from` can end silently.** `sed ... \| head -n 20` under `pipefail` (5643) exits 141 when more than a pipe's worth of listed paths are not in the tape's catalogue; the `die` after it and the fill never run. | run by the finder (2,000 lines) |
| A8 | low | **A `df` that exits non-zero ends any mode silently.** `free_bytes` is a pipeline under `pipefail` (319-321); the "df failed; continuing" branches (375, 386) cannot be reached. | run by the finder |
| A9 | low, concern | **The file check can print "restored file(s) ... match" having checked nothing**, when its counts file is empty (4673-4701); each half was shown (busybox awk exits 0 on a failed write), not the whole. It needs a full work directory under busybox awk. The check also returns silently when the index lists a unit and no file row for it (4674-4678), which these tapes cannot produce. | not shown end to end |
| A10 | medium | **RECOVERY.md's per-unit commands, run as written for a second unit, restore nothing of it.** Every unit's block uses the same slice names (`restore.N.dar`), the same `mkdir /destination` and `-R /destination`, and never says to change directory or remove the last unit's slices (`src/volume/layout.rs:8751-8753, 8806-8813`); File 1 has both rules (2294-2296, 2370). After a 12-slice unit, a 1-slice unit's block leaves the earlier unit's higher slices in place, dar reads the earlier unit's catalogue from them, prints "0 inode(s) restored" and **exits 0**. RECOVERY.md calls itself sufficient, File 1 step 6 sends the reader to it, and `--find-envelope` prints it. | the text read by the coordinator; the three blocks of the gate's operator envelope executed by the finder |
| A11 | medium | **File 1 sends every key holder to "tapectl itself" for a damaged slice; with a tenant's key that route refuses.** "tapectl can ... reads around a damaged 64 KiB piece" (2104-2112); its commands take "your key file, the escrow key's included" (2148-2154). `catalog rebuild --key TENANT.key` is refused and points back to RESTORE.sh (`src/volume/rebuild.rs:1470-1497`), and no tapectl restore runs without the rebuilt catalog. The same paragraph says that with only the key file `age-keygen -y YOUR_KEY.age.key` prints ESCROW_PUBLIC, which for a tenant's or the operator's key registers the wrong identity. All 27 production units are `family`'s. | the text read by the coordinator; the refusal confirmed by the recovery lens |
| A12 | low | **Three texts that fail loudly.** Nothing the heir holds off the tape gives the 512 KiB read File 0 itself needs (the sheet names no block size; `dd` at its default answers "Cannot allocate memory", which only File 1 explains). RECOVERY.md names padded ciphertext by an age message (`unexpected data`, 8840) that age 1.1.1 does not print. "Exact size" is said of the manifest's `size_bytes`, which is the plaintext size (`src/volume/manifest.rs:95`; 2382, 2503-2504). | traced by the finder; the age message measured |

Two texts mislead beside these: rung 3's `zero_strip_instructions` prints a literal `"$DEVICE"`
for the heir to type (1139-1143), and "Use the RESTORE.sh stored on THIS tape" (866-869) is said
about the second cartridge on the swap path.

What the read settled in the tapes' favour: no short-seal or spanning branch is reached on a
full tape with no span; the two cartridges came from one staging, so each lists the other's
slice hashes, and a replacement slice is taken only by sha256 at a position the other tape's
seal covers; the only read of standard input is the swap prompt, and end of input is `quit`; a
300,000-row `paths.tsv` takes 6 to 24 seconds in the index programs.

**The plain route for all of these is the troubleshooting guide and the Heir Kit's sheet, not a
rewrite** (item 178's precedent): the work directory needs room and `TMPDIR` moves it; restore
as an ordinary user; load the right cartridge at a swap prompt; one directory per unit when
following RECOVERY.md; the escrow or operator key for tapectl's own restore. They are fixed in
the generator at the next re-pin, whenever that is (D8).

**In the Heir Kit, not on tape, so it can be corrected by a patch (K1, medium).** COVER.txt's
step 4 gives `tapectl restore raw-volume --device /dev/nst0 --to ./out` as the first command
and says "The first works with no database at all" (`src/crypto/escrow_kit.rs:1123-1128`). On
an heir's machine it is refused as not initialised (`src/main.rs:408-410`), and the refusal
names plain `tapectl init`, which mints a new escrow identity and shows a new secret. The sheet
has the right line (`init --operator NAME --escrow-public-key ...`) only under "IF RESTORE.sh
STOPS". Read by the coordinator.

## 4. Tier B: what home2 runs

No path was found that seals a tape or counts a write without a passing confirm in the same
transaction, initialises over a sealed volume (`--force` included), releases staging or moves a
cartridge before the write returns `Ok`, counts an unsealed, erased, retired or quarantined
volume as a copy, lets an old version's copies satisfy the current one, or resets the verify
clock on a failed read. The staging lens found no way for a source file to be missing from an
archive, or in it with other bytes, without a refusal. `adopt_lost` holds all four conditions.
ADR-0012 items 76 to 234: about 45 were traced to code and none was built differently from its
ruling.

| id | Sev | Finding | Checked |
|---|---|---|---|
| B1 | high, conditional | **`catalog rebuild` folds a different unit into a catalog unit of the same name and counts its tape as a copy.** `existing_unit` matches on uuid, then on name (`src/volume/rebuild.rs:2606-2619`; the comment chooses it, to avoid `UNIQUE(name)` aborting the rebuild). `ensure_snapshot` then finds the row by `(unit_id, version)` (2657-2670), a stage set and a `completed` write are inserted under it, and the two file lists are merged. Two units that carried one name become one unit whose v1 reads two copies; nothing in the report says a uuid differed. The same collision for a volume label is refused (`binding.rs:2158-2165`). **Not reachable on today's pair:** both cartridges were rewritten in handoff (i), so no tape carries the earlier generations' uuids. It is reachable after `unit rename` and a reuse of the old name, and wherever a catalog and a tape of different generations meet: handoff (i)'s fresh start deleted the dotfiles so that every unit was registered under a new identity with its old name (item 193). The old catalog and its backups are the three `*-old` directories still waiting for the operator's tidy; a current tape rebuilt into one of those would fold. | read by the coordinator; two finders reported it independently; no test |
| B2 | medium | **A tape written from a second staging of a version lists the unit dotfile with the first staging's sha256, so the heir's file check calls every such unit damaged.** `.tapectl-unit.toml` is exempt from the re-stage comparison (`src/staging/validate.rs:110-116`, item 24), its recorded hash is never refreshed (`src/db/files.rs:290-293`, `AND sha256 IS NULL`), and `index/paths.tsv` lists it like any file (`src/volume/index.rs:265-269, 297-300`). RESTORE.sh has no exemption, so after `unit tag`, `unit rename`, a `[policy]` edit or `location rename`, the next copy made from a re-stage ends `RESTORE INCOMPLETE`, exit 1, and sends the heir to another tape for a file that is not damaged. L6-0001 and L6-0002 came from one staging and are not affected. **It is what 1.9.2 writes the first time a replacement or third tape is staged after any dotfile edit**, and `docs/troubleshooting.md:3495` recommends one such edit ("the dotfile is not content, so the edit creates no new version"). | read by the coordinator; no test (the gate's one dotfile edit precedes staging) |
| B3 | medium | **A unit whose newest snapshot was never staged reads as covered everywhere.** The dirty scan compares the source with the newest snapshot of any status (`src/unit/content_match.rs:156-166`, `src/collection/fingerprint.rs:233`); copies are counted over `current` snapshots. After a `snapshot create` whose `stage create` failed or never ran, `audit` exits 0, `status` shows nothing, and `collection run` does not call the unit pending, while the newest content is on no tape. `report failed-stages` lists a failed stage; nothing else does. | read by the coordinator (the three legs); no test |
| B4 | medium | **`db import` cannot run when the live catalog will not open, and the recovery pages send the operator to it for a damaged catalog.** Every command opens (and migrates) the catalog before dispatch (`src/main.rs:588-609`); only `db fsck --repair` has another door. `docs/install.md` §8a and runbook D say "damaged or lost" and never say to move the file aside. A lost file works. | read by the coordinator; no test |
| B5 | medium | **`db import` replaces the live catalog with an unchecked file and keeps no copy of what it replaced** (`src/cli/operations.rs:4022-4024`): no integrity check, no schema version comparison. A backup from a newer binary leaves every later command failing, `db import` included (B4). | read by the coordinator |
| B6 | medium, rare | **After a recorded early warning, `volume resume` seals from the medium alone and re-sends nothing** (`src/volume/session.rs:2598-2604, 2704-2750`). Item 174's retry can demote a file at or before the warning to `pending` after the event; if the re-send then fails, the resume can seal right after a torn slice, and the sealed tape can never confirm. No copy is counted and staging is kept. It needs the early warning (never reached in production), a refusal surfacing at the seal and a failed re-send, and one step, the closing filemark after a refusal on the HP, is not verified. | the resume leg read by the coordinator; the rest traced by the finder; no test sets a warning with Logical Block Protection armed |
| B7 | medium | **An escrow public key is stored without being parsed.** `read_or_parse_public_key` accepts any string that starts `age1` (`src/crypto/keys.rs:66-73`), for `init --escrow-public-key` and `key import --escrow`. A mistyped key in the disaster-recovery recipe is accepted, ADR-0005 makes the row permanent, and the first `stage create` fails. home2's key is past this: its stages ran. | read by the coordinator |

Lower, each traced by its finder (the coordinator read B8, B9, B11 and B16):

- **B8.** `cartridge erase` on a tape that already reads blank or EMPTY returns "nothing to
  erase" before the catalog cross-check (`src/volume/erase.rs:781-795`), so a volume the catalog
  still counts on that cartridge stays a copy, with exit 0 and no warning.
- **B9.** `init` overwrites an existing `config.toml` with the defaults whenever `tapectl.db` is
  absent (`src/setup.rs:183-201`, `src/main.rs:746`).
- **B10.** `db fsck --repair` goes on when the integrity check failed, and records only counts
  of what it deleted (`src/cli/operations.rs:4122-4126, 4279-4350`).
- **B11.** `snapshot create --all-pending` skips a unit it could not scan with a warning, then
  prints "nothing pending: every active unit's source matches its newest snapshot"
  (`src/cli/snapshot.rs:371-398`).
- **B12.** `snapshot delete --force` checks for no live or resumable write session and unlinks
  the staged slices under it (`src/cli/operations.rs:3284-3511`); `stage clean --force` refuses
  the same case.
- **B13.** A stop part-way through `mark_writes(.., "aborted")` leaves a split session;
  `volume resume` then panics at an `.expect`, each time. `volume abort` then `volume write`
  recovers and nothing says so (`src/volume/session.rs:4671-4681`).
- **B14.** Rebuild takes a stage set's escrow receipt and slice size by unit name, so a tape
  holding one unit in two stage sets gives both the later set's receipt
  (`src/volume/rebuild.rs:1801-1815, 2857-2867`).
- **B15.** The exclude patterns the snapshot uses (the `glob` crate) and the ones dar is given
  (fnmatch) disagree on a few forms; the stage is refused, with the wrong cause named.
- **B16.** `docs/install.md:658` says `volume init` is not a tape command for `tapectl-op`;
  line 638 and `tapectl-op.in:52` list it (item 196).
- **B17.** The upgrade: a resumed `--upgrade` reuses a step-2 backup of any age and names it as
  the rollback (`scripts/first-run.sh:1157-1162`); `--from 4`, which the script itself
  suggests, skips the backup and the kept binary; a rollback leaves the new backup wrapper
  beside the old binary; step 11 prints "ran" for an exit 75; step 7 with `--home X` rewrites
  `/etc/tapectl/home`; the tape lock is the fixed name `/tmp/tapectl-tape.lock`.
- **B18.** Remedy strings that send the reader wrong: the hints after a stopped
  `volume write L --init --full-confirm --move-to X` name a bare `volume write L`
  (`src/cli/volume.rs:2305-2306`; a test pins it, item 218 asks for the one copy loop);
  `status` suggests a bare `stage create` for an unchanged under-copied unit, which refuses;
  the Tier-3 refusal names `snapshot mark-reclaimable` without the `--force` it needs;
  `mark-erased` is named without its barcode (`src/volume/write.rs:6190`) and without the
  `--force` or `--yes` it needs from most states (`src/volume/binding.rs:560-563`);
  `docs/troubleshooting.md:222` says no `--force` is needed after "nothing to erase", which is
  false for an EMPTY File 0 no erase produced; confirm's two guard errors are wrapped with "run
  `volume resume` to continue the full readback" (`src/volume/write.rs:4838-4839`), which after
  an abort first needs a full verify and after a retire refuses; and the next command pinned
  after an abort at the medium's end sends the operator to write the same full cartridge again
  (`src/volume/write.rs:25821-25827`; the second plan was not traced).

## 5. Tier C: built and not used

- **C1, medium.** `volume rewrite --resume` loops when the tape was erased and the catalog was
  not told. The erase's record is a separate step after the tape is erased
  (`src/volume/erase.rs:879, 943-962`); on resume the tape reads EMPTY, the erase returns before
  its cross-check (B8's early return), and the rewrite stops with "the cartridge is erased but
  the catalog still holds volume as retired" and tells the operator to `--resume` again
  (`src/volume/rewrite.rs:1912-1959, 1329`). The staged slices stay held, so nothing is lost;
  the only copy sits in staging while the operator is sent in a circle. `cartridge mark-erased`
  is the way out and is not named. Read by the coordinator.
- **C2, low.** `status` and `report fire-risk` judge active units only; a tape-only unit below
  its floor is absent from the home screen (`audit` flags it).
- **C3.** Three rulings the coordinator adjusted while building are recorded only in release
  notes and CLAUDE.md, with no ADR item: 1.7.0's fork X12, item 159 "as built", and items 149
  against 156. CLAUDE.md and a doc comment still call Q40's exit "pending the CTO" after item
  223 ruled it, and item 223 and the 1.9.1 notes quote a note ("unit is dirty: snapshot it")
  that the code words differently.
- **C4, low.** `release.yml` uploads with `--clobber`, so a tag pushed again replaces the
  release's binary and `SHA256SUMS`; the Heir Kit's sheet sends an heir to those sums when the
  kit holds no static binary.
- **Not read.** Spanning, per-location targets and auto-stage, compaction and
  `volume read-slices`, and most of `tapectl setup` (§10).

## 6. Concerns: suspected, not traced to an outcome

- **The short seal has never run with Logical Block Protection armed**, which is how every
  production write runs. The 1.6.0 and 1.7.0 acceptances predate arming, the 1.8.0 acceptance has
  no early-warning step, and the gate's short-seal legs run unarmed. No defect was traced in it
  beyond B6. `scripts/short-seal-acceptance.sh` on an armed drive settles it; it matters the day
  a tape fills.
- **`volume init`'s page clear.** After the immediate filemark of File 0 the device drops with
  no synchronous filemark before the page is cleared (`src/tape/ioctl.rs:310-322, 715`). File 0
  is rewritten by every `volume write`. A lone `volume init` then `volume identify` on the HP
  settles it.
- **File 0's `[media]` strings are written unescaped** (`src/volume/layout.rs:205-206`); a `"`
  in a hand-typed barcode on a cartridge with no chip serial would make File 0 invalid TOML for
  every Rust reader. Real chip strings are alphanumeric.
- **`clean_staging` selects before its transaction** (`src/staging/clean.rs:489-596`); a write
  starting in that window would have its set marked cleaned. Milliseconds, one operator.
- **`TMPDIR` on a filesystem without FIFOs** (exFAT on a borrowed disk): the script's own advice
  for a full work directory, and its `mkfifo` is unguarded (2308-2309).
- **Step 12's rehearsal** recognises a production cartridge by barcode and serial only; a
  catalog row with no chip serial would not be recognised. Both production rows have one.
- **After a hard-killed `volume rewrite` and a refused retry**, stage sets may be left `staged`
  with no slice files (`src/volume/rewrite.rs:1362-1386`, `1260`). How `StagedRun.prior` is
  captured for a compact directory that already exists settles it. Tier C.

## 7. Gaps: the four threats of intent 3, walked against the record

The intent statement names four threats: site loss, future-self failure, media and drive death,
online compromise. Each was walked from the production run records
(`docs/runs/2026-11-09-handoff-i-1.9.2-and-copies.md` and the three before it), the profile and
the units, asking what evidence exists for these two cartridges and these keys, not for the
code. Six gaps came out. None is a defect in what the code was ruled to do. G2 and G3 are
behaviour that is missing, and a lens reported each as a medium; the other four are evidence
that does not exist yet. G1's first step, G4 and G6 need no build.

- **G1. Nothing has ever decrypted a byte of the production tapes.** The write's full readback
  compares ciphertext sha256. `stage create` hashes the source beside dar's read and compares
  dar's catalogue; it does not decrypt what it encrypted (`src/staging/mod.rs`, no decrypt
  before the tests). Every real-drive restore on record (`docs/runs/2026-10-29-1.8.0-handoff-h.md`,
  step 12's rehearsals, 53/53 each) used the test cartridge and a throwaway home's keys. So the
  claim "L6-0001 opens with the keys in the envelopes" rests on the code being right, and on
  three things no record covers: that the handwritten escrow secret is the pair of the public
  key on the sheet (the ceremony's own check, `age-keygen -y`, `docs/keys-and-recovery.md` step
  3; no record says it was run after the paper was written), that the tenants' key files exist
  outside home2 (handoff (i) leaves "give `parents` and `family` their new key files" open), and
  that a 470 GiB unit restores (the largest unit ever restored on the real drive was a test
  unit). Until the tenants hold their files, the escrow paper is the only key outside home2.
  This is D4's drill; the paper check is two minutes and needs no tape.
- **G2. A new folder under `keepsake` is invisible until someone runs `collection sync`.**
  `audit` judges registered units only, and `status` maps registered pending units to their
  collection (`src/cli/status.rs:621-655`); neither walks a collection root. No timer runs
  `collection sync` or `collection status`. A new year's folder has no copy and the weekly audit
  exits 0. A changed file inside a registered unit is seen (`dirty`). The coverage lens
  reported the same independently.
- **G3. A failed audit, a failed backup and a raised TapeAlert reach nobody by default.** The
  three wrappers write to the journal and can ping one URL, `TAPECTL_HEALTHCHECK_URL`, set by
  hand in a systemd drop-in (`docs/install.md` 533-551). No unit has `OnFailure=`; neither
  `first-run.sh`, `install-systemd.sh` nor `home2.profile` sets or asks for the URL. Audit
  warnings (`escrow_kit_stale`, `dirty`, a verify falling due) are a success and are never
  pushed. Exit 75 (catalog busy) is quiet in all three. Whether home2 has a drop-in is not in
  the repo. For a system whose stated risk is the operator's own future inattention, the
  feedback loop is pull only. The scripts lens reported the same independently.
- **G4. Both copies were written by the one drive that has ever read them.** "Full readback
  passed" proves the HP drive reads what the HP drive wrote. No record shows any tapectl tape
  read in a second drive. LTO's servo tracks make interchange failures rare, but a drive that
  writes out of tolerance takes every tape it wrote with it, and both copies share that drive.
  This is the common-mode half of "drive death", and an heir's drive will be a different one.
- **G5. Both copies come from one staging.** Stage once, write N copies (handoff (i): "staging
  released: 27 stage sets" after copy 2). A defect in a staged slice would be on both tapes. The
  evidence against it is the source hash beside dar's read, #464's catalogue cross-check and
  the ciphertext readback; the missing piece is G1's decryption.
- **G6. Two places are not enforced for these units.** `[defaults] min_locations` binds
  tape-only units only (`policy::coverage::location_floor` returns 0 for an active unit; ruled,
  the 2026-09-29 rename kept the meaning). home2 sets no `copies_at` and no archive set, so if
  L6-0002 came home for its yearly verify and stayed, `audit` would stay clean with both copies
  on one shelf. The write warns once when a copy shares a place (item 26); nothing does after.
  `[defaults] copies_at = { "home2-rack" = 1, "offsite" = 1 }` is the built way to make `audit`
  hold it (1.7.1), and it is not set.

Two smaller ones, for the operator to settle on home2:

- **The default exclusions.** `init` writes `global_excludes = ["*.nfo", "Thumbs.db",
  ".DS_Store", "*.tmp"]` (`src/config.rs:988-993`), matched case-insensitively, and neither
  `first-run.sh` nor the profile changes it. No snapshot or stage output counts or names the
  files the patterns left out. If home2's config still has the defaults, any such
  file under `keepsake` is on neither tape and in no index. One `find` on home2 says how many.
- **The annual heir drill has no clock.** A tape's readback has one (`volume_verify_age`, 365
  days). The drill `docs/operator-guide.md` asks for each year is a paragraph; nothing records
  one or warns when it is overdue, and none is recorded for these cartridges.

## 8. Design improvements

Ten, in the order I would take them. Each names the intent it serves, whether it moves a byte on
tape, and what it waits on. None is a new capability for the bulk. Where one adds code, it is
within D2's exceptions (a production gap or heir-path work) or it waits for D1.

| # | Improvement | Serves | Moves on-tape bytes | Waits on |
|---|---|---|---|---|
| 1 | Prove the tapes, not the code: the heir drill as a full restore of one production tape with the escrow secret typed from the paper, compared against the index; then a read of one tape in a second drive | intents 3, 4 (G1, G4, G5) | no | D4 |
| 2 | Push, not pull: `first-run.sh` asks for a ping URL or a mail address and writes the drop-ins; `OnFailure=` on the three units; the weekly audit's warnings sent as a digest; `status` shows each timer's last run and result | intent 3 (G3) | no | nothing |
| 3 | `audit` walks collection roots: a folder that is not a unit is a warning (`collection_unsynced`) | intent 3 (G2) | no | nothing |
| 4 | Two places held by `audit` for the core: set `[defaults] copies_at` on home2, after the per-location code has had its read (D13; this review did not reach it) and a trial in a throwaway home (it also changes when staging is released, item 154) | intent 3 (G6) | no | D13's read; the CTO's word; no build |
| 5 | Exclusions are reported: `snapshot create` and `stage create` count and name what the patterns left out; `init` writes junk-only defaults (`Thumbs.db`, `.DS_Store`) | intent 2 (the core is irreplaceable) | no | nothing |
| 6 | A recorded production tape that every later build must read: a small tape image written by 1.9.2 (File 0 to the seal, with its RESTORE.sh) kept as a test fixture, restored and rebuilt from in CI | intent 5; makes D3 testable | no | nothing |
| 7 | RESTORE.sh's text out of Rust string literals into `.sh` and `.awk` files pulled in with `include_str!`, with `bash -n` and shellcheck on the generated script in CI (it lints clean today: three notes at warning level; CI runs neither) and the awk fragments run under mawk and busybox awk as well as gawk | intent 4; the maintainer | no (the golden pin proves the bytes did not move) | nothing |
| 8 | At the next format change only: slices as plain cuts of the one dar stream. Since 1.8.0 both restore paths strip the dar slice framing that staging adds (`dar::slice::Join`, `join_slice`) and rebuild the stream header from the reader's own dar (`stream_check`). Plain cuts make the join `cat`, remove the script's 250-line join section with its dar header parser, and remove its dependence on the heir's dar writing a 38-byte header like today's | intent 4 | yes | D1, D3, D5b |
| 9 | At the next format change only: the heir's script in two files. By section, about 3,400 of RESTORE.sh's 7,061 lines are the plain restore with `--info` and `--verify`; about 800 serve short and spanning tapes; about 2,850 are the index modes, the per-file check, `--list`, slice replacement, salvage and fill. A core script half the size, and a second file for repair | intent 4 | yes | D1, D3 |
| 10 | The record: a current-rules digest by topic in place of CLAUDE.md's 300-line chronology, ADR-0012 closed at item 234 with new rulings in topic ADRs, and one installer named as the future (`first-run.sh` is 2,862 lines of bash, `tapectl setup` 1,650 of Rust, for one job) | the maintainer; D7 | no | D7 |

Notes on the ones that need them:

- **1.** The drill D4 asks for should end in numbers: units restored, files checked against
  `paths.tsv`, the hours it took, the tool versions. Typing the secret from the paper is the
  point of it. The second-drive read needs a borrowed or bought LTO-6 or LTO-7 drive; an
  `--info` and a `--verify` of L6-0001 there is enough.
- **4.** This uses a built feature in place of building one. It needs a trial first because
  `copies_at` changes the copy floor (the larger of `min_copies` and the map's sum) and holds
  staging until each place has its copy.
- **6.** The golden pins hold the writer still. Nothing holds a later reader to the tapes
  already on the shelf except `requires`. The Heir Kit carries the tapectl that wrote them, which
  covers the heir; this covers the operator's next tapectl.
- **7.** `src/volume/layout.rs` is 8,866 lines before its tests, most of it shell inside raw
  strings, where no editor, linter or reviewer reads it as shell.
- **8.** This is a third answer to D5(b)'s "dar or tar": keep dar, drop the framing. Manual
  recovery stays tool-only (`cat` the decrypted slices into `dar -x -`). It is a format change
  with a `requires` entry, so it belongs to a bulk campaign or to nothing.
- **Code structure.** The largest files are mostly tests: `write.rs` is 9,900 lines of code and
  20,400 of tests, `session.rs` 4,950 and 9,400. I recommend no split for its own sake. The one
  structural change worth making is 7.

## 9. For the CTO

These continue the intent review's D1 to D7, none of which is ruled. Nothing here is filed; say
which to file and they become issues.

- **D8. The catalogue's slice numbers (H1).** Three parts.
  (a) *Now, no byte on tape:* the Heir Kit's sheet and the troubleshooting guide say that on
  tapes written by 1.1.0 to 1.9.2 `--path` and `--fill-from` are not to be used, that `-T slice`
  from the catalogue is not to be trusted, and that the way to one file or to a damaged file is
  the whole unit from the other tape. *Recommended: yes, first.*
  (b) *On master:* tapectl's `restore file` reads every slice until the numbers can be trusted;
  the test fixtures make their catalogue as staging does; the gate gets a `--path` and a
  `--fill-from` leg on a multi-slice unit. The generator's fix (a catalogue with the real
  layout, or readers that work from the manifest and read every slice) changes RESTORE.sh, so
  it waits for a re-pin.
  (c) *Whether L6-0001 and L6-0002 are written again for it.* *Recommended: no.* The whole-unit
  restore, the per-file check and slice replacement work; a rewrite is a sixth and seventh
  write of the same data and opens the one-copy window a third time. D3 stands, with (a) as
  the answer for these two tapes.
- **D9. No further copy is staged until B2 is fixed or ruled out for that copy.** A replacement
  tape or a third tape made after any `unit tag`, `unit rename`, `[policy]` edit or `location
  rename` tells its heir every such unit is damaged. A warehouse deposit of a sealed volume
  (D6) re-stages nothing and is not affected. *Recommended: yes; the fix is that `paths.tsv`
  states no hash for the unit dotfile, which is new on-tape text only for tapes not yet
  written.* Whether that needs a ruling as a format change is the CTO's.
- **D10. A patch for what home2 runs (1.9.3), under D2's exception for production defects.** No
  byte on tape, no spelling removed. In it: B1 (a name match under another uuid is refused or
  reported, never attached), B3 (a newer unstaged snapshot is pending), B4 and B5 (`db import`
  over a catalogue that will not open; a checked source and a kept copy), B7 (the escrow key
  parsed), B8 (the erase's catalog half before "nothing to erase"), K1 (the sheet's step 4),
  G2 (`audit` walks collection roots), G3 (the timers tell someone), the exclusions reported
  (§8, 5), and the lows of §4 as one batch. *Recommended: yes. G2, G3 and the exclusion report
  are small new behaviour; they close stated intent-3 gaps and add nothing for the bulk.*
- **D11. Checks on home2 this week, the operator's, no build.**
  1. `age-keygen -y` on the escrow secret typed from the paper; it must print the sheet's
     `age1...` (G1).
  2. The tenants' key files handed over (handoff (i)'s open item), so the paper is not the only
     key outside home2.
  3. `dar -l <home>/catalogs/<any> -N -Q -T slice | tail -3` on home2's dar 2.7.21, to see H1's
     numbers there.
  4. `find /srv/keepsake/original-data \( -iname '*.nfo' -o -iname '*.tmp' -o -iname Thumbs.db
     -o -name .DS_Store \)`, and `tapectl-op config get defaults.global_excludes`: what the
     tapes leave out.
  5. `systemctl cat tapectl-audit.service tapectl-backup.service tapectl-drive-poll.service`:
     is a ping URL set (G3).
  6. Until D8(a) lands, a line in each envelope: restore whole units; do not use `--path` or
     `--fill-from`.
- **D12. `[defaults] copies_at` on home2** (G6), so `audit` holds one copy at `home2-rack` and
  one at `offsite`. *Recommended: yes, but not yet: the per-location code is among what this
  review did not read (§10), so it waits for D13's read of it and then a trial in a throwaway
  home. Until then the two places rest on the operator and on `cartridge move`.*
- **D13. Before a feature not used today is first used in production** (`allow_spanning`,
  `--for-location`, `volume rewrite`, compaction, `tapectl setup`): its own adversarial read
  (§10 lists what this one did not reach), and for a tape that will fill,
  `scripts/short-seal-acceptance.sh` with Logical Block Protection armed. *Recommended: yes.*
  It puts that cost where D1 puts the need.

And two that sharpen D4 and D5:

- **D4, sharpened.** The drill is a whole-tape restore of L6-0001 or L6-0002 with the escrow
  secret typed from the paper, by RESTORE.sh, then one unit by File 1's steps, each unit in
  its own directory (A10), with the files checked against `paths.tsv` and the hours and tool
  versions recorded. Then `--info` and `--verify` of one tape in a second drive (G4).
- **D5(b), a third answer.** Keep dar and drop its slice framing (§8, 8).

## 10. Not reached

- **Tier C, almost all of it.** Spanning (`build_span_layout`, `CapacityBudget::place`,
  `session::span_split`, `span_proofs.rs`, `restore::span`, `rebuild::tape_split`,
  `promote_spanning_version`, RESTORE.sh's `span_*` and `restore_across_tapes`, read once and
  not run), per-location targets and auto-stage (`for_location.rs`, `NeedAt`, `Hold`),
  compaction and `volume read-slices`, `tapectl setup` after its first steps, the Heir Kit's
  `--static-binary`. D13.
- **ADR-0012.** About 45 of items 76 to 234 were traced to code; the rest were read as text.
- **Tier B, in part.** `volume/build.rs` and `layout_model.rs` (the capacity gate);
  `store.rs`'s `FileCursor` and `MemStore`; `session.rs`'s resend bookkeeping beyond B6;
  `rebuild.rs`'s Supplement loader and cartridge binding; `adopt_lost.rs`'s `staged_match`;
  `cli/restore.rs`, `restore/short_tape.rs` and the tails of `fill.rs` and `supply.rs`;
  `staging/release.rs` and `clean.rs` from line 700; the bodies of `cartridge retire`,
  `cartridge mark-erased` and `unit mark-tape-only`; `tape/contact.rs`'s bookkeeping, the
  health and wear modules; `first-run.sh` 700-834, 1935-2152, 2420-2530 and 2660-2775;
  `collection/plan.rs`, `batch.rs` and `hook.rs`; every test body.
- **Tools.** uutils coreutils, GNU cpio's `mt`, mawk 1.3.3 (Debian 10's, home2's userland),
  dar 2.6, 2.7.21 and 2.8, and a real drive were not available to any finder. The stream path
  (`stream_unit`, `join_slice`) was read, not run end to end; the gate runs it.
- **Not confirmed by a build.** Every Rust finding is a reading. B3, B6 and `restore file`'s
  wrong subset are the three a short test would settle first.
- **Outside the repo.** home2's drop-ins and config, where the envelopes and the offsite tape
  physically are, and whether the ceremony's key check was run: the operator knows (D11).

## 11. Finder rules (as given)

The 2026-10-15 rules, with a tier added: read-only (no cargo, repo scripts, tapectl, `/dev`,
network, `gh`, `ssh` or sub-agents); grep then read ranges; the design over the code, and a
behaviour an ADR item rules is not a defect; check `docs/` and `git log` before reporting;
default to "not a defect", each finding traced (state, command, wrong outcome, with
`file:line`) or marked CONCERN with what would settle it; no features and no rewrites. High =
a sealed tape overwritten without consent, a copy counted that is not one, a destructive step
below its ADR-0008 tier, an heir silently given partial or wrong data, plaintext on tape or
staging, a bricked or silently shrunk catalog, a restore that cannot complete from a tape the
system calls good; medium = an operator or heir told something false at a decision point, a
safety path with no test that can fail, a wrong refusal, lost forensic evidence. Every report
has findings, the remedy strings checked against the named command's own refusal logic,
"considered and dropped", "read" and "not reached". The heir lenses and the on-tape lens could
run single functions and awk fragments on synthetic input; the scripts lens could run `bash -n`
and `shellcheck`. The finders were given the production facts raw (host, catalog, tapes, what
has never run) and none of the intent review's verdicts.
