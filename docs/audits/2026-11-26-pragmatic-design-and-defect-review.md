# Pragmatic design and defect review — 2026-11-26

**Asked for:** "a deep analysis of this project with respect to pragmatic design, and review for
bugs, improvements as well", by the CTO on 2026-11-26. Master `fd583f03`: the 2.0.0 candidate
(Layout Version 3), with the whole chain green on its code (`07144179`) and the real-drive runs
done. The tag is not made.

**What this is.** The code was read twice in the two weeks before (nine area reads on
2026-11-20, an adversarial read on 2026-11-24), each area by area. This review cuts the other
way: eight lenses that each cross every area. It also asks the question the earlier reads did
not: whether each part of 2.0.0 earns its size. It fixes nothing in the code, moves no pin and
changes no ruling. Its decisions for the CTO are in §9.

**Method.** Read-only on `fd583f03`: no cargo, no tapectl, no tape, no production host. Eight
finders, one lens each, under the rules of §11, each told what the earlier reads had found:

| Lens | Asks |
|---|---|
| delta | the 4,800 lines landed on 2026-11-25, read by nobody but their author |
| atomicity | the catalog and staging after the process dies at the worst moment |
| last-copy | every operation that removes bytes or a claim, against one definition of a copy |
| damaged-input | every reader, when the bytes are not what the writer wrote |
| secrets | keys, plaintext and trust boundaries |
| design | an independent read: does each part earn its complexity |
| cost | time, machine and operator attention spent that need not be |
| proof | do the tests prove what they say |

The design lens was given measured facts and none of the coordinator's views, because the
coordinator built most of what is reviewed. A usage limit stopped three finders part-way; each
was resumed from its own transcript and finished.

The coordinator read the cited lines of the high and of every medium, and re-ran four probes:

- the NUL-byte probe of D-1, under gawk and mawk;
- three damaged-tape runs (DI-1, DI-3, DI-4): the tree's own RESTORE.sh (sha256 `ddce81da…`,
  checked) on copies of the recorded tape, with the heir harness's stand-in `mt` and `dd` and
  the real `age` 1.1.1 and GNU tar 1.35.

"CONFIRMED" below means a probe showed it. "TRACED" means read and not run. No finding was
confirmed by building tapectl. Lows rest on their finder's trace unless marked.

**A correction to the coordinator's own figure.** RESTORE.sh is 4,118 lines at `fd583f03`. The
figure 3,633 given to the CTO on 2026-11-26 was counted on a stale checkout; it is the length
of the recorded tape's older script.

## 1. The result

- **One high, in the heir's script:** RESTORE.sh stops in every mode when File 0 gives a read
  error, though nothing in File 0 is needed to restore (§2).
- **Thirteen mediums.** Four are in RESTORE.sh (§3). Seven are in tapectl (§4): three about
  crashes and concurrency, one about the Heir Kit's medium, three about damaged tapes. Two are
  about proof (§7): the one test of a member over 8 GiB ran in no chain, and no test runs the
  current script on an older tape.
- **No case was found** where a crash leaves a copy counted that is not one, where a slice
  goes to tape for fewer recipients than policy, where one tenant's names reach another's
  envelope, or where plaintext lands on the staging device. The write session, staging, the
  staging release and `cartridge erase` held up under every kill point tried.
- **The design is pragmatic at its centre and has no brake at its edges** (§6). The data path
  is about 2,800 lines and the tape does not need tapectl. Three things grew with no rule to
  stop them: the heir's script, the destructive commands in the CLI layer, and the decision
  record.
- **Every RESTORE.sh fix moves a pin**, so each is the CTO's decision (§9, D14). A tape's
  script can never be patched afterwards. The review recommends one more move of that pin
  before the tag, and a test and a kit change that make later defects repairable.

## 2. The high: the heir's script stops at an unreadable File 0

**DI-1 (high, CONFIRMED).** `src/volume/heir/RESTORE.sh:948-952`, `bootstrap`: when the read of
File 0 fails, the script ends with "cannot read File 0 … is a cartridge loaded and is … the
right drive?". Every one of the seven modes calls `bootstrap` first.

- **State.** A sealed tape whose first file gives a read error and whose other files are
  sound.
- **Outcome.** Exit 1 in `--info`, `--verify`, `--find-envelope`, `--list`, `--restore`,
  `--where` and `--find`. Nothing else on the tape is tried.
- **Why it is wrong.**
  - Nothing File 0 states is needed. The script already defaults the front index to file 3,
    finds the seal from the end of data, and treats File 0's other fields as optional.
  - A File 0 that reads as garbage restores whole: the script warns and goes on "as a Layout 3
    tape". So the larger damage restores and the read error does not.
  - `docs/design/volume-format-v3.md` §1.2 says of a damaged File 0 that "the case an heir most
    needs RESTORE.sh for must never become a hard stop", and §5 that the seal's `requires` "is
    the fence when File 0 cannot be read".
  - tapectl's own contact treats an unreadable File 0 as an absence and goes on
    (`src/volume/binding.rs:1955-1963`). So the system counts such a tape as a copy while the
    heir's script cannot open it.
- **Why it is the likely damage.** A tape drive returns the right block or a read error; a
  block that reads back changed is rare. File 0 is at the beginning of the tape, which every
  load passes over.
- **Not shown.** The probe models the read error and assumes the drive can still space past
  File 0's filemark. The design states the same premise for tapectl's contact.
- **What softens it.** File 1's manual steps do not need File 0, and the Heir Kit carries
  tapectl, whose restore goes on.
- **No test.** No heir test fails file 0.
- **A fix would touch** `bootstrap` only: go on with empty File 0 facts and a warning, as for
  the garbage case. It moves RESTORE.sh's pin.

## 3. The heir's script: four mediums and seven lows

All are in `src/volume/heir/RESTORE.sh`. Every fix moves the pin.

### When the tape is damaged

- **DI-3 (medium, CONFIRMED). `--verify` passes with File 3 unreadable.** With File 3 giving a
  read error, or replaced by garbage, the run ends `VERIFY PASSED: 14 file(s) match the front
  index.`, exit 0. The files were matched against the seal's copy; the front index was never
  read. File 3 is the one file whose loss does not fail the check (`:1212-1228`,
  `:1257-1259`). When the block reads and is wrong, the seal's `front_index_sha256` is in hand
  to prove it and is not compared.
- **DI-4 (medium, CONFIRMED). One changed byte in File 3 costs more than losing File 3.** An
  unreadable File 3 restores whole, because the script falls back to the seal's copy. A File 3
  that reads with one byte changed does not fall back:
  - one hex digit in an envelope's hash line: the intact envelope is called DAMAGED and the
    tenant is refused, with the backup envelope never tried (`:1335-1338`, `:1312-1317`);
  - a slice's size line: the intact slice is left out, though the manifest inside the envelope
    is what the slice is checked against two lines later (`:2069-2075`);
  - a size with a leading zero: the run ends on bash's own words, "08327: value too great for
    base" (`:1414`). One tenant's restore is stopped by a line that describes another tenant's
    envelope.
- **DI-9 (low, a design point on §1.2 rule 1).** One changed byte in File 0's `magic` or
  `requires` line stops the script, and the remedy names the script itself. The seal's repeat
  of `requires` is not consulted.

DI-4 and DI-9 need a block that reads back changed, which a drive rarely gives. They matter
more for a volume kept as a raw dump or a warehouse deposit on other media.

### When the tape is crafted

`age` gives no sender authentication: anyone who knows a key's public half can make an
envelope that key opens, and the Heir Kit prints the escrow public key. ADR-0012 item 312
already names this case ("a crafted tenant envelope") and added a guard for it. Two holes
remain.

- **D-1 (medium, CONFIRMED). A NUL byte defeats the member-path guard.** `refused()`
  (`:2479-2488`) accepts a file-table path that holds a raw NUL. bash then drops the NUL, so
  `..<NUL>/victim` becomes `../victim`. In the probe, `scrub_inflight` (copied verbatim)
  removed a file outside the destination, under gawk and mawk. The guard is the one item 312
  added on 2026-11-24 to close #606 C-2. Damage cannot produce this row (age authenticates the
  envelope) and no file name holds a NUL.
- **S-1 (medium, the `mkdir` CONFIRMED, the rest TRACED). A unit's name is joined to `--to`
  unchecked.** With `--restore --all` on an envelope of more than one unit, `dest="$TO/$name"`
  takes the name straight from the manifest (`:3376`, `:3385`). A name `../x` restores that
  unit outside `--to`. tapectl's own restore validates the name again before joining it
  (`src/volume/restore.rs:2236-2244`); the script has no counterpart.
- **S-5 (low).** `--info`, which needs no key, prints the front index's `size_bytes` raw, so
  an escape sequence in a plaintext zone reaches the terminal.

### From the changes of 2026-11-25

Three of these come from the coordinator's own fix for the unit root's modification time
(`export TMPDIR="$WORK"`, and a reference file held open on fd 8).

- **D-3 (low, CONFIRMED with a stand-in error).** With `TMPDIR` in the work directory,
  `scrub_inflight`'s here-document (over 64 KiB at about 600 files) needs room in a directory
  that is full. The scrub is then skipped silently and tar's cut-short member stays in `--to`.
  A later run with `--overwrite` rewrites it, and the file check judges it.
- **D-4 (low, CONFIRMED on GNU tee).** A write error on the plain cut that is not "disk full"
  loses its words: the slice is called bad by its length, and the heir is sent to another copy
  of a tape that was right (`:2104-2144`).
- **D-5 (low, CONFIRMED for awk).** `report_absent` passes the work directory through
  `awk -v` (`:2645`), which reads backslashes. A backslash in `--to` or `--work` makes gawk and
  BusyBox awk fail, and the run ends before its summary. The same class is at `:2999`.
- **D-6 (low, a concern).** The reference file is held open while its directory is removed
  (`:185-198`). On NFS that should leave the work directory in the restored tree. One restore
  onto an NFS mount settles it.
- **D-7 (low).** Without `/proc`, the root's modification time is no longer kept, and no NOTE
  says so.

### What was read and held

The read-ahead: a slice taken from it gets the same verdict as one read in the foreground; the
head's position is safe after a drop; no tape device is open at a prompt; `set -m` works with
no terminal; the space accounting holds. The Rust `check_member_path` and the awk `refused()`
agreed on 40,014 rows (through a model of the Rust), apart from D-1's NUL, which both accept.

## 4. tapectl: seven mediums

### Crashes and concurrency

- **A-1 (medium, TRACED). `volume rewrite` erases its source while the slices read from it
  are not synced.** `stream_verify_slice_to_staging` (`src/volume/write.rs:7851-7877`) creates,
  writes and drops each staged file with no `sync_all`. `verify_staged`
  (`src/volume/rewrite.rs:1290`) reads them back, which the page cache serves. The source is
  then retired and erased (`:2119`). A power loss in the kernel's writeback window after the
  erase can leave the only copy short or absent. `stage create` syncs each slice and its
  directory (#409, `src/staging/mod.rs:2406`); the recovery reads were not given the same.
  A fix is one sync.
- **A-2 with LC-1 (medium, TRACED). The retire family's last-copy floor is read before the
  prompt and never again.** `volume retire` locks only its own volume
  (`src/cli/operations.rs:479-488`), reads the impact (`:490`), asks the Tier 3 floor
  (`:521`), prompts, then commits an unguarded `UPDATE` (`:600-604`). `cartridge retire`,
  `cartridge mark-erased` and compaction's finish have the same shape. Two retirements of a
  unit's last two copies, run together, both pass. The floor's own comment calls it
  "structurally undefeatable". The more likely form: `volume rewrite A` plans its consent
  before an hours-long read (`src/volume/rewrite.rs:1493`), the operator retires copy B in
  another terminal, and the rewrite then erases A under a consent that named copies elsewhere.
- **A-3 (medium, the SQLite half CONFIRMED). `db import` replaces the catalog under a live
  session.** `db_import_stepping` (`src/cli/operations.rs:4349-4517`) probes no lock. A running
  `volume write` keeps committing to the unlinked old file, reports the volume sealed and
  confirmed, exits 0 and releases staging. The live catalog and the kept copy both lack the
  write. The tape is recoverable with `catalog rebuild --from-volume`, but nothing says it is
  needed.

### The Heir Kit's medium

- **S-2 (medium, TRACED). `key escrow-kit DIR` makes its plaintext catalog snapshot inside
  DIR.** `write_encrypted_catalog` (`src/crypto/escrow_kit.rs:520-524`) uses
  `tempfile_in(out_dir)`. The help's own example names a USB stick. Every tenant's unit names,
  paths, sizes and hashes are written there in the clear, then unlinked, not overwritten. A
  second signal or a crash leaves the file there, and nothing sweeps it. The home's work
  directory (`staging::home_work_dir`, mode 0700, swept) exists for this. The existing test
  lists the directory after a successful run, so it cannot see either case.

### Damaged tapes

- **DI-2 (medium, TRACED). A File 0 that reads but does not parse makes tapectl refuse every
  read of a counted volume.** `check_id_thunk_readable` (`src/volume/format.rs:825-833`)
  returns the parser's error when the layout is its own. That stops restore, `volume verify`,
  `volume read-slices`, `catalog rebuild --from-volume`, `restore raw-volume` and the adoption
  of a lost write. A File 0 that gives a read error is an absence and proceeds. RESTORE.sh
  restores the same tape and names the damage.
- **DI-5 (medium, TRACED). `volume verify` cannot judge a tape whose File 3 is unreadable or
  unparseable.** It reads and parses File 3 (`src/volume/write.rs:7442-7445`) before it makes
  the verification session. The command ends with a bare error: no session, no result, no
  quarantine. The volume stays `ok` and counted, and every later verify ends the same way.
  `volume-format-v3.md` §6 puts the hash comparison before the parse, and the chain walk has
  an arm for exactly this that verify never reaches.
- **DI-8 (medium, TRACED). `cartridge erase` calls a Layout 3 tape with a damaged File 0 "not
  written by tapectl".** `src/volume/erase.rs:527-533`. The consent word becomes the chip
  serial and the seal is never probed. #587 fixed this for other layouts only.

## 5. Lows, design points and concerns

### Lows in tapectl

| Id | What | Where |
|---|---|---|
| A-4 | A change and its audit row as two autocommit statements: `snapshot mark-reclaimable`, `tenant reassign`, cartridge relabel, operator serial, `cartridge add`, deposits, `volume import`. Siblings of #606 R-6. Audit trail only | `src/cli/operations.rs:3862`/`:3866` and six more |
| A-5 | `release_covered_slices` and `snapshot delete` choose outside their transaction. Sibling of #540; ends in a refusal with a working remedy | `src/staging/clean.rs:839-864`; `operations.rs:3585`/`:3655` |
| A-6 | A minted secret key is not synced before `key rotate` durably deactivates the old ones. After a power loss a tenant's slices are sealed to a key nobody holds; operator and escrow still open them | `src/crypto/keys.rs:45-58` |
| LC-2 | `report copies` leaves out every `missing` unit; `audit`, `report needs` and `report fire-risk` include them | `src/cli/report.rs:1292`, `:1361` |
| LC-3 | `tenant delete` refuses only for `active` units, so a tenant whose units are all tape-only can be deleted, and its keys leave `key list` | `src/db/queries.rs:717-719` |
| S-4 | Tenant `key import` accepts anything starting `age1`; later refusals name no key | `src/cli/key.rs:447` |
| S-6 | CI's dependency audit is `continue-on-error` "until #42 lands". #42 is closed and `quick-xml` is gone | `.github/workflows/ci.yml:155-158` |
| DI-6 | After a rebuild where the seal reads but does not bind File 3, the advice "`volume write L` starts over" is refused | `src/cli/catalog.rs:1509-1542` |
| DI-7 | The seal's position comes from File 0's `seal_marker` alone; a lowered digit makes rebuild record a sealed tape unsealed | `src/volume/rebuild.rs:2249-2261` |
| DI-10 | `restore raw-volume` leaves a cut-short dump under its final name on a read error | `src/volume/raw.rs:303-326` |
| DI-11 | `as` casts turn `size_bytes = -1` into `u64::MAX`; a sum then panics in debug and wraps in release | `src/volume/format.rs:280` |
| DI-12 | The LBP page parser keeps a page whose length says under 3 bytes, then indexes byte 6 | `src/tape/lbp.rs:207-246` |
| DI-13 | `catalog.db` is extracted to scratch with no flush, so a full disk truncates it silently | `src/volume/envelope.rs:333-335` |

### Design points on rulings

Each is ruled behaviour. Each is the CTO's to keep or amend.

- **DP-1, on item 310 (found by two lenses).** `unit mark-tape-only`'s Tier 3 counts a copy of
  a `reclaimable` or `purged` version (`src/cli/operations.rs:3009-3022`, `current_only:
  false`). Every other floor treats those versions as given up. A unit whose only tape copy is
  of a released version passes Tier 3; a later `volume rewrite` of that tape drops the copy
  without naming the unit. The coordinator wrote item 310's wording ("no version of the unit
  has a copy on a tape that counts") and did not weigh the version's status.
- **DP-2, on items 51 and 216.** `cartridge erase` of a retired volume shows nothing about
  what holds its versions now. If the other copy was quarantined since the retirement, the
  last readable tape is erased on a typed label. No command returns a `volume retire`d volume
  to service.
- **DP-3, on item 216.** A sealed tape the catalog does not know is called "another catalog's
  volume" and offered for erase. After `db import` of an older backup, that is this catalog's
  own newest tapes. Neither text names `catalog rebuild --from-volume`.
- **S-3, on item 188.** `tapectl setup --yes` mints the permanent escrow identity by default
  and prints the secret to stderr. `first-run.sh --auto` refuses the same act without
  `--mint-escrow`.
- **On item 289** (the design lens). Filling the queue to the early warning makes a unit
  across two cartridges the normal end of every full tape. The heir then meets the
  cartridge prompt on every full tape. A planner rule (stop at the last unit that fits whole,
  when the next unit would fit a cartridge by itself) would change no byte of the format. The
  lens did not re-read the measurements behind the ruling.

### Concerns, not traced to an outcome

- **A-C1.** `volume init` commits the catalog, displacement included, before it writes File 0
  (`src/volume/write.rs:724`, `:795`).
- **C-1.** `stage clean --force` and `snapshot delete --force` can remove staged slices that
  are the only copy anywhere (a `missing` unit staged and not yet written), with no fact shown.
- `catalog rebuild` inserts the tenant and unit names a tape states without validating them
  (`src/volume/rebuild.rs:2714-2724`, `:2805`).

## 6. Pragmatic design

The design lens's verdict, which the coordinator accepts: **the design is pragmatic at its
centre and has no brake at its edges.** No argument here rests on how much data the present
collection holds or on what tools the production host has (ADR-0012 items 281 and 273).

### The size, measured

| What | Lines |
|---|---|
| Production Rust under `src/` | about 150,000 |
| In-module tests | about 167,000 |
| `tests/` | 63,000 |
| Shell and Python harness and installers | 25,000 |
| RESTORE.sh | 4,118 (7,061 at 1.9.2, before tar replaced dar) |
| File 1 as rendered | 1,858 |
| ADR-0012 | 4,085, in 312 items |

The surface: 149 man pages, about 115 leaf commands, 92 config fields, 42 tables.

### Right as it is

- **The tape does not need tapectl.** A tape, a key and File 1 are the archive. Everything
  below about size is a cost of changing the tool, never a threat to what it has written.
- **Layout 3's data zone.** A tar stream cut where a member starts, each slice compressed
  alone and then encrypted. The writer's side is about 2,800 lines. One slice is an archive
  any tar reads. Replacing the format before production was the right call.
- **A script and a manual, both on the tape, tested as a pair.** bash, awk, tar and dd are the
  most conservative interface there is.
- **Each check before the next step, and no zero-fill.** A restore that is not proven whole
  says so.
- **Two readers of one format**, written apart: each is the other's check on what the format
  means. (DI-1 and DI-2 are the two readers disagreeing, which is how they were found.)
- **The typestate write session and the Store trait:** small interfaces over the dangerous
  parts.
- **Slices identical on every copy; the catalog as claims the tape can correct.**
- **Byte pins only the owner may move, and a recorded tape never re-recorded.**
- **No daemon; advisory audit; consent tiers that block only where a last copy is at stake.**

### What grew with no rule to stop it

1. **The heir's script.** 1,496 lines on 2026-11-19 (`7fa6c754`), 4,118 on 2026-11-25. Every
   step was ruled, and every fix added lines. It is the one part of the system that can never
   be patched, and it changed most in the last week. Three reads in seven days each found
   defects its tests and the gate had passed: one high and four mediums (2026-11-20), two lows
   and a concern (2026-11-24), and now §2 and §3. D-1 is in the lines added to close the read
   before it.
2. **The destructive commands in the CLI layer.** `src/cli/operations.rs` holds 5,144
   production lines and 98 SQL sites: `volume retire`, `cartridge retire`, `mark-erased`,
   `mark-tape-only`, `snapshot delete`, `db import`. Each reads its facts, prompts, then
   writes, with its own SQL. A-2, A-3, A-4, DP-1 and last week's fixes for #606 R-1 and C-1
   are all in that file. The catalog has no seam: its interface is the schema, and 85 files
   are its callers.
3. **The decision record.** One ADR holds 312 items that amend each other. `src/` cites "item
   N" 1,875 times. For the format, ADR-0014 gathers the items into 117 lines and works. For
   everything else, a reader reads ADR-0012 in order.

Also measured:

- `src/volume/write.rs` holds seven commands in 10,296 production lines, with functions over
  400 lines. `session.rs`'s header says write.rs "shrinks to CLI orchestration"; it did not.
- 44 tests assert on production source text, because the callers they pin need a drive. They
  mark a missing seam in front of the drive-observation calls. They also pin a file's text, so
  the file cannot be split without moving them.
- Drive and cartridge observation is about 8,700 of `src/tape/`'s 11,542 production lines,
  against 1,407 for the tape I/O itself.

### The extras, judged

The design lens judged each feature beyond the core by what it prevents and what it costs to
keep. Its discriminator: an extra the heir's script must carry is paid for on every tape for
ever; one only tapectl carries can change in a patch release.

| Verdict | Features |
|---|---|
| Keep | a unit continuing on the next cartridge and the short seal; compression (no fourth algorithm); warehouse deposits; collections; archive sets; Logical Block Protection; the drill record; the service account; `first-run.sh`; the feed ratio |
| Freeze (no growth without a named decision it would change) | `copies_at` and auto-staging; `volume rewrite`; compaction; drive and cartridge observation; the fifteen reports; `tapectl setup` |
| Remove or rule | the `research` feature: its own header says the shaping "is NOT ruled", and it costs a third clippy target on every chain run |

`volume rewrite` is the one command that erases a last copy by design, and it has drawn a
medium finding in each of the last four reads (#579, #580, #590, A-1).

### The method

Agents write nearly all the code, one person rules on design, a large harness proves it, and
the rate is high (434 commits from the go to the candidate).

- **What it does well.** Proof is never skipped for effort: 180 tests of a shell script, a
  test that runs a manual's commands as printed, a recorded tape. The record is complete.
  Independent reads are cheap, so they are done.
- **Where it is weak.** The harness tests what its author imagined, and the author is the same
  kind of mind each time. Nothing resists growth: an agent does not feel a 4,118-line script
  as pain, and the harness makes each addition safe to land. A list answered with "ratify all"
  makes the design the recommender's, with a veto; the veto was used well three times in one
  week, which shows what is lost when it is not. And the definition of done moves: 62 commits
  followed the commit the chain record called final.
- **Its failure mode is surplus, not negligence.**

## 7. The proof system

The proof lens sampled the safety suites for tests that cannot fail. In the write session,
erase, rewrite, release and coverage suites it found none, and the gate's handling of skips,
exit codes and by-name judgement held on every leg it checked. What it did find:

- **P-1 (medium, TRACED; the tar fact CONFIRMED). The only test of a member over 8 GiB ran in
  no chain.** `a_member_over_8_gib_is_read_by_its_size_record` (`src/tar/stream.rs:2357`) is
  the plan's proof that the stream reader takes such a member's size from the pax `size`
  record. GNU tar writes the header's own size field as zero for it. The test is an ignored
  lib test; the documented perf command builds another test binary, and neither the gate nor
  the chain names it. Swapping the precedence at `:765` would pass every other test.
  P1_RUN_RESULT
- **P-2 (low). The tests named for tenant isolation do not test tapectl's choice of
  recipients.** In `cargo test`, nothing puts one tenant's key against another tenant's
  production-staged slice; sealing slices to every tenant would pass CI. The gate's
  `crosskey_rejected` and the lifecycle suite's isolation step do catch it.
- **P-3 (low, CONFIRMED).** 27 of the 52 tests in `tests/integration.rs` call no tapectl
  function but `db::open`. About 15 are named for behaviour they never invoke.
- **P-4 (low).** #540's regression test (`src/staging/clean.rs:1741`) passes when its window
  hook never runs.
- **P-5 (low).** Three source-text tests give false comfort: one scans 354 of 477 lines of a
  file, because it splits at an inline `#[cfg(test)]`; one checks the first of two call sites;
  one accepts its needle anywhere in a 414-line body.
- **P-7 (low).** Of the 31 ignored tests, 21 run in the gate by name. Seven perf tests and
  P-1's ran in no recorded chain. None is ignored for failing.
- **P-8 (low).** Three lifecycle refusal checks judge by exit code and words, and never read
  the state or the tape afterwards.

**DS-1 (medium, a concern; the design lens).** File 1 says four times that one copy of
RESTORE.sh serves every tape. `tests/recorded_tape.rs:602-649` runs only the recorded tape's
own script (`1aa02316…`); no test runs the tree's script (`ddce81da…`) on that tape. This
review's damaged-tape probes did so by hand, and the undamaged runs restored every file. R1
(§8) makes that a test.

### What each level catches that the others cannot

| Level | Catches alone |
|---|---|
| In-module and integration tests (4,902, about 9 minutes) | logic, at the speed of a commit |
| Golden byte pins | any change to a generated byte |
| The recorded tape | a later build that cannot read a real 2.0.0 tape |
| Budget tests | a path that became slower while still correct |
| `file1_as_written` | a command in File 1 that does not run as printed |
| CI's Ubuntu 26.04 image and the container as uid 1000 | a substrate change under the heir's script |
| The mhvtl gate (84 checks) | the real `age`, `tar`, `mt` and `dd` on a tape device; interrupts |
| The lifecycle suite (487 checks) | command order across time |
| Real-drive runs | what mhvtl does not model: the early warning, the real rates |
| An independent read | what the author did not think to test |

No level can be dropped: each has caught something the others missed.

### What is still unproven

1. One release's RESTORE.sh on another release's Layout 3 tape, by test (DS-1).
2. File 1's manual steps with the real `mt`, `dd` and `age` on a tape device. They run today
   only against stand-ins.
3. A read error in the middle of a file. Only fakes produce one; the gate's corruption tests
   change bytes, which a drive rarely does (§2). The heir harness's stand-in `dd` fails before
   its first byte, where a drive fails after some.
4. Power loss, as against a kill. No test can tell a synced write from an unsynced one, which
   is how A-1 and A-6 stood.
5. Two tapectl processes that each pass their own check (A-2, A-3). The lock tests run one
   command against a held lock.
6. A full disk during tapectl's own restore, and during a catalog write.
7. `volume rewrite` and a long erase on the real drive, for 2.0.0.
8. A restore by someone who is not the author: the drill (#548).

## 8. Improvements

Ranked within each group. "Pin" marks a change to bytes on tape, which is the CTO's.

### To the design (the design lens's R1 to R10)

| # | Change | Cost it removes | Pin? |
|---|---|---|---|
| R1 | Hold by test that the tree's RESTORE.sh reads the recorded tape, whose own script is already an older one; put the current RESTORE.sh in the Heir Kit beside the binary | A defect in a tape's script is permanent today. File 1 says four times that one copy of the script serves every tape, and no test holds that | No (amends ADR-0009's list of the kit's contents) |
| R2 | A written ceiling and an admission rule for File 2: no new mode; a new capability goes into File 1's text, the envelope or the kit's tapectl. At the next on-tape authorization, weigh removing the read-ahead (223 lines of job control) and the Layout 2 option tombstones | A script that grew 2.75 times in six days | The rule is a document; the removal is a pin |
| R3 | Move the retire family, `mark-tape-only`, `snapshot delete` and `db import` into one library module with one shape: read the facts, decide the tier, apply in one transaction whose `UPDATE` restates the facts it relied on | The class behind A-2, A-3, A-4 and DP-1 | No |
| R4 | Gathering records on ADR-0014's pattern for the subjects the code cites most: the retire family and its floors; copies and coverage; cartridges and contacts; the kit and escrow; the host and releases | Reading 4,085 lines in order to learn one behaviour | No |
| R5 | Freeze the candidate before the chain runs; one run record for the commit that is tagged | A tag whose chain record names another commit | No |
| R6 | One source-scan test that fails on a `volumes.status` predicate outside `policy/coverage.rs` (eight sites today) | A rule in CLAUDE.md that nothing holds | No |
| R7 | One gate leg that runs File 1's manual steps with the real `mt`, `dd` and `age` | The by-hand fallback has only run against stand-ins | No |
| R8 | Make the span and short-seal acceptance scripts gate legs | Two harnesses (2,096 lines) for what item 288 made the ordinary end of a tape | No |
| R9 | Group `tapectl --help` so the routine's dozen commands come first; let `volume write` end by naming what is owed (`db backup`, the kit) | An operator choosing among 115 commands | No |
| R10 | Remove the `research` feature or rule on it | A build target on every chain run | No |

### To cost (the cost lens)

| # | Change | Evidence | Pin? |
|---|---|---|---|
| C-4 | Split `file_check`'s one `sha256sum -c` into contiguous pieces, one per core | 3,935 s of a 10,829 s whole-unit restore is this one process. A probe on GNU tools gave identical failure lists and 2.9 times the speed at four pieces. Not run under uutils | **Pin** |
| C-1 | `[profile.dev.package."*"] opt-level = 3` | Debug builds run age at 3 to 4 MiB/s against 55 in release; `tests/restore_memory.rs` takes 193 s for 5 tests. Not measured | No |
| C-3 | In `stage create`, move the member sha256 off the thread that also runs age | They run in series on one thread (`src/tar/stream.rs:985-1006`, `src/staging/files.rs:338`); 55.4 MiB/s on 1.75 cores. This is what bounds #601 | No |
| C-2 | Hash the restored tree on several threads in tapectl's restore | One thread today (`src/volume/restore/reader.rs:812-893`) | No |
| C-5 | A one-slice read-ahead in tapectl's restore | Slice N+1 is read only after slice N is hashed and fed | No |
| C-7 | Build the gate's test binaries in its first lock hold | A gate of 866 s took 1,483 s and 3,083 s under contention | No |
| C-6 | Say in the docs that `status`, `audit` and `report dirty` walk every active unit's tree, and that a `sha256`-mode unit is re-hashed each time | Read in code | No |
| C-8, C-9 | Budget gaps: no pass budget for restore, verify, rebuild or rewrite; `catalog_budget` covers 8 commands; `cost_budget` lacks restore through the supply, identify, erase and init | Read in the suites | No |

### Smaller, from the other lenses

- A way back from `volume retire` (DP-2).
- The confirm's first `INSERT` is the one statement in it not under the busy retry
  (`src/volume/session.rs:3796`).
- A write killed between its confirm and its move or release leaves nothing that says the
  follow-up is owed.
- The release workflow names its actions by moving tag in a job with write access, and its
  `SHA256SUMS` comes from the same step.

## 9. For the CTO

These continue the earlier reviews' numbering. Nothing here is built or filed as a ruling.

- **D14. One more move of RESTORE.sh's pin before the tag, for §2 and §3.** *Recommended:
  yes.* The reasons: DI-1 is the likely form of damage and the format document forbids the
  hard stop; D-1 and S-1 are holes in a guard item 312 added on purpose; D-3, D-6 and D-7 are
  regressions from the coordinator's fix of 2026-11-25; and after the tag every cartridge
  carries the script for good. The cost: one branch of fixes with a failing test first for
  each, the u26 container run, the gate's heir legs, a PR's CI, the whole chain again on the
  new commit, and a record for it. No real-drive run is needed for these (none changes a tape
  command), though the script that ran on the real drive would then be one version back.
  Under D14 the coordinator would also take C-4 (the split final check) in the same move,
  if the CTO wants it: it is the largest remaining cost of an heir's restore.
- **D15. The tapectl mediums before the tag, or in 2.0.1?** A-1, A-2, A-3, S-2, DI-2, DI-5 and
  DI-8 change no byte on tape. *Recommended: A-1, A-3 and S-2 before the tag* (each is a few
  lines: a sync, a lock probe, a directory), *the rest in 2.0.1 with R3*, since A-2's right
  fix is R3's shape and DI-2, DI-5 and DI-8 want one decision about a damaged File 0 and
  File 3 across all of tapectl's readers. If D14 is yes, the chain runs again anyway.
- **D16. Item 310's count (DP-1).** Should `mark-tape-only`'s Tier 3 leave out `reclaimable`
  and `purged` versions, as every other floor does? *Recommended: yes.*
- **D17. The erase of a retired volume (DP-2) and of an unknown sealed tape (DP-3).** Should
  the erase show which versions on the tape have no copy now, should a `volume retire` have a
  way back, and should "the catalog does not hold this volume" name `catalog rebuild
  --from-volume`? *Recommended: yes to the facts and the wording; the way back is a small
  command and can wait for 2.0.1.*
- **D18. R1: the current RESTORE.sh in the Heir Kit, and the cross-script test.** *Recommended:
  yes, the test now (no pin) and the kit change in 2.0.1.* It is the one change that makes
  every later defect in a tape's script repairable.
- **D19. R2: a ceiling and an admission rule on File 2.** *Recommended: yes to the rule, after
  the tag.* The read-ahead's removal is a separate question for the next on-tape
  authorization; the coordinator built it and does not rank it.
- **D20. A feature freeze after the tag, with R3 and R4 as the first work.** The 2026-11-12
  intent review asked for a freeze (its D2) and was not answered. *Recommended: yes.* The
  method's failure mode is surplus.
- **D21. Item 289** (fill to the warning). Is a unit across two cartridges at the end of every
  full tape the wanted default, given what it asks of the heir? *The CTO's call; the review
  only names the cost.*
- **D22. `tapectl setup --yes` and the escrow secret (S-3).** Should it refuse without an
  explicit flag, as `first-run.sh --auto` does? *Recommended: yes.*

## 10. Not reached

- **delta:** most of the new tests in `tests/heir_restore.rs` and the gate script; RESTORE.sh
  under uutils or an older bash.
- **atomicity:** `adopt_lost.rs`, `collection/batch.rs`, `tape/poll.rs`, `for_location.rs`,
  `store.rs` internals, `db fsck --repair`, lock-file reclaim, and 18 change-then-event sites
  the finder listed and did not open.
- **last-copy:** the installer scripts and `init` over a home; rebuild's internals; the
  rewrite's resume.
- **damaged-input:** `tar/stream.rs`; the supply, fill and chunk readers beyond one function;
  RESTORE.sh's table and index parsers; most of the tape layer.
- **secrets:** RESTORE.sh `:2260-2360`, `:2810-3345` and `:3560-4118` line by line; the Rust
  restores' scratch files; the plaintext generators in `layout.rs`; the root-run host scripts
  beyond grep.
- **design:** most extras were judged from module headers, size and reach, not a full read.
- **cost:** `session.rs`'s execute, `rebuild.rs`, `verify_contacted` itself, compaction, the
  lifecycle suite's breakdown. C-1's gain is not measured.
- Nothing was confirmed by building tapectl, and nothing was run on a tape device.

## 11. Finder rules (as given)

The 2026-11-12 rules (`2026-11-12-design-and-defect-review.md` §11), with these changes.
Every finder was told what the two reads of 2.0.0 and the open issues already hold, and to
report a new instance of an old class by naming the finding it resembles. A finding is TRACED
(state, command, wrong outcome, `file:line` for each step, every caller read), CONFIRMED (a
probe run on synthetic input under the finder's own directory) or a CONCERN with the one thing
that would settle it. Never argue from the present collection's size or the production host's
tools (items 281 and 273). An improvement needs a measured or cited cost. Each report has
findings, improvements, "considered and dropped", "read" and "not reached". The finders'
reports, whole, are comments on the review's tracking issue.
