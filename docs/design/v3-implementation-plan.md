# tapectl 2.0.0, Layout Version 3: the staged implementation plan

This is the build plan for tapectl 2.0.0, written for implementing agents that were **not**
part of the design sessions (ADR-0012 item 279: "write up all plans in detail for worker LLM
to implement in stages"). The design is ruled in ADR-0012 items 235 to 291. A worker's job is
execution. It is never redesign.

**Status, 2026-11-17: no go has been given. Nothing in this plan is built.** Items 282 to
289 (slices that end at a member's start, each compressed alone; every unit continuing at
the early warning) were ruled that day and are carried below, and the choices D1 to D22
were ratified as they read here (item 290). The plan is a
document. A stage starts when its "Starts when" line is true, and every stage needs the
CTO's go first. One task does not: T0.3, the made tree, which item 291 said to run "now".

The stages are the phases of the epic, [#554](https://github.com/mikmorg/tapectl/issues/554),
with the same numbers:

| Stage | What | Lands on | Owner | Worker-ready as written? |
|---|---|---|---|---|
| 0 | Measure before any code | no code; a dated note | worker, operator | yes (T0.1, T0.2, T0.3, T0.5, T0.6, T0.7); T0.4 needs the operator |
| 1 | The normative set: the Layout 3 format document and its companions | `master` (documents) | **coordinator** | no: it is written by the coordinator, and §S1 lists what it must decide |
| 2 | Fifteen fixes that depend on nothing else | `master` | workers, three at a time | yes, on the go |
| 3 | Compatibility out | `layout-3` | workers | yes, on the go and after Stage 2 (D9, how a 2.x catalog is known, is ratified) |
| 4 | The data path on the tar payload | `layout-3` | workers, by task order | no: every task reads Stage 1's documents |
| 5 | Every carried feature, proven on the new payload | `layout-3` | workers | no: after Stage 4 |
| 6 | Reads and proof | `master` | coordinator, fresh readers, operator | no |
| 7 | The acceptance run | the production host | operator, CTO | no |

A task is named `T<stage>.<n>`. Each task states its **owner**, what it **depends** on, the
**issues and items** it carries out, what to **read**, its **fence** (the files it may change),
its **steps**, its **traps**, how to **verify** it, and when it is **done**.

---

## 1. Read these first

Every worker, before its task:

1. `CLAUDE.md` (the repo's rules; "Build Commands" and "Testing").
2. This file's sections 1 to 6, then the task's own entry. Do not read the other tasks.
3. The texts the task's **Read** line names, and only those sections.
4. The issue the task names, whole. **Every file and line an issue cites is a lead.** Confirm
   the defect in the code before writing anything. If it is not there, stop and report
   `DEFECT NOT PRESENT` with what the code does instead.

**Which design texts are normative depends on the stage.** This matters because the two sets
contradict each other on purpose.

| Stage | The code it changes writes | Normative for the task |
|---|---|---|
| 2 | Layout 2 | `docs/design/volume-format-v2.md`, `docs/design/layout-session.md`, `docs/design-errata.md`, ADR-0001 to ADR-0012 |
| 3 | Layout 2, minus compatibility | the same, with ADR-0012 item 258 |
| 4, 5 | Layout 3 | `docs/design/volume-format-v3.md` and the session document as Stage 1 leaves them, then ADR-0012 items 239 to 291. `volume-format-v2.md` is history there: never implement against it |

**Authority when texts disagree:** the format document for the stage, then the session
document, then ADR-0012's items (a later item amends an earlier one), then this plan. If two
normative texts conflict, or a step is ambiguous in a way that changes bytes on tape, the
database schema or a command's spelling: **stop and report the question.** Do not pick the
reasonable one. The coordinator takes it to the CTO.

## 2. Who does what

- **Worker.** Works in the one worktree it is given, on the one task it is given, inside the
  task's fence. Commits to its branch. Never pushes, merges, opens a pull request, closes an
  issue, touches a tape device, or changes a golden pin.
- **Coordinator.** Creates each worktree from the right base and checks the base. Writes each
  worker's prompt from `.claude/commands/worktree-agent.md` and this plan. Reviews the diff
  against the task's "Done when". Integrates, runs the full suite and the tape chain, pushes,
  watches CI, and closes issues. Writes Stage 1. Owns every golden pin, every file under
  `docs/adr/` and `docs/design/`, `CONTEXT.md`, `CLAUDE.md` and the gate's manifest.
- **Operator.** The CTO's hands on the production host: anything that changes that host, the
  real drive, the escrow paper, the cartridges.
- **CTO.** The go for each stage, every question a task stops on, and each go-ahead named in
  Stages 6 and 7.

## 3. Branches and how the suite stays green

- **Stage 2 lands on `master`**, wave by wave. Each wave is integrated, the full suite and
  the lints pass, it is pushed, and master's CI is green before the next wave starts.
- **Stages 3 to 5 land on the integration branch `layout-3`**, cut from `master` by the
  coordinator when Stage 2 is complete. The coordinator opens a draft pull request from
  `layout-3` to `master` so that CI runs on every push of the branch. `master` stays a
  coherent Layout 2 tree until Layout 3 is whole. `layout-3` is merged when Stage 5's last
  task is done and the pending list below is empty.
- **A worker's branch** is `l3-<task>` (for example `l3-t2-rebuild`), in the worktree
  `.claude/worktrees/l3-<task>`, made by the coordinator from the stage's branch. Never use
  the Agent tool's `isolation: "worktree"` (it forks from `origin/master`).
- **`cargo test --no-fail-fast` is green at every commit of `master` and of `layout-3`.** Two
  things make that possible while a payload is replaced:
  1. **Pending tests.** Between T4.5 (staging switches to tar) and the tasks that port the
     readers, a test that fails *only because a later task's path is not ported yet* is
     marked `#[ignore = "L3-pending T4.<n>: <the path it waits for>"]` by the task that broke
     it. Nothing else may be ignored this way: not a test of the task's own work, not a test
     that shows a defect. `grep -rn 'L3-pending' src tests` is the pending list. Each later
     task's "Done when" names the tag it must empty. The coordinator reads the list at every
     integration, and the merge to `master` needs it empty.
  2. **dar's tests leave with dar's code.** A test of the dar wrapper, of dar's slice framing
     or of the one stream into dar is deleted in the commit that deletes the code it tests
     (T4.5, T4.8 and T4.15 between them), never ignored.
- **The tape chain** (the mhvtl gate, `first-run.sh`'s rehearsal, the lifecycle suite, the
  acceptance scripts) is run by the coordinator only. It is green on `master` after Stage 2,
  is **not expected green on `layout-3` between T4.5 and T4.16**, and is green again before
  the merge.

## 4. Rules for every task

These repeat `.claude/commands/worktree-agent.md`, which the coordinator pastes into each
prompt. They are here so that a worker reading only this file is still held to them.

- Begin every shell command with `cd <your worktree> && `. Read and edit files only there.
- **Debug and check builds only. Never `--release`.** Every cargo invocation sets
  `CARGO_TARGET_DIR=/scratch/tapectl-target-<branch>` and is wrapped in
  `flock /scratch/tapectl-build.lock`. Run cargo in the foreground and wait for it; never put
  it in the background and never poll for it.
- Before every commit: `cargo fmt`, `cargo clippy --all-targets` (zero warnings),
  `cargo test --no-fail-fast` (cargo stops at the first failing test binary without the
  flag). After a clap change: `cargo run --example gen_man`, `cargo run --example gen_cli_md`
  and `python3 scripts/check-docs.py`, and commit what they regenerate.
- All logic lives in the library crate. `src/main.rs` stays a thin wrapper.
- **No new dependency** without the coordinator's word.
- **Never** touch `/dev/nst*` or `/dev/sg*`, never set `TAPECTL_MHVTL=1`, never run tapectl
  against `~/.tapectl` or `/etc/tapectl` (a temp home only), never ssh to the production host.
- **Never** change `tests/on_tape_golden.rs`. If a pin fails, stop and report which pin and
  which change moved it. The coordinator moves or removes a pin, each in a commit of its own.
- **Never** edit `docs/adr/`, `docs/design/`, `CONTEXT.md`, `CLAUDE.md`, `docs/audits/`,
  `docs/runs/`, `docs/research/` or the gate's `EXPECTED_FAIL` manifest, unless the task's
  fence names the file.
- **Never** `git stash`, `git commit --amend`, `git push`, `gh pr create` or `gh issue close`.
  No closing keyword in a commit message (`Fixes #N`, `Closes #N`, `Resolves #N`): write
  `(#N)`.
- Never fix a red test by weakening what it asserts. If a test contradicts the normative
  text, the text wins: say so in the commit message. If it contradicts your code, your code
  is wrong.
- Touch only what the fence names. No refactor on the way past, no module moved, no rename
  that the task does not ask for.
- A commit message is an imperative summary and a body that says why, naming the ADR item or
  the format document's section that asks for the change.
- **Heavy work asks first.** Before a full test run or a container, run
  `~/git/broker/status -q`. Do not start on a hold, on `/scratch` RED or EMERGENCY, or at a
  load of 25 or more. Stop heavy work when `/scratch` has under 30 GB free. Every
  `docker run` takes `--log-driver none` and carries no data on its standard output.

**Report at the end of a task**, in this order: what was changed (files); the commits; the
exact commands run and their last lines; every test added, by name; anything the task asked
for that was not done, and why; anything found outside the fence. A report that says "all
tests pass" without the command's output is not accepted.

## 5. What must stay true (a breach stops the line)

The first three are Layout 2's and are carried unchanged. The rest are Layout 3's.

1. **The seal marker is written only inside the session, after the last content file.** An
   interrupted or resumable tape has none.
2. **`Layout::validate` checks the staged slices before the tape moves**; execute re-hashes
   as it writes; confirm re-hashes from tape. The last two are never optional.
3. **No plaintext file on tape carries a tenant or unit name, a file name, a plaintext hash,
   a key fingerprint, or a user or group id or name.** The leak-scan's needles are never
   weakened; the id map adds needles.
4. **No reader zero-fills a lost slice, and none passes `--ignore-zeros`** (item 264). A
   slice holds whole members (item 283, rule 13), so a reader leaves a bad slice out and
   gives tar the good ones, joined in order; a long member with a bad piece in its run is
   left out, or removed again if tar had begun it. tapectl's own reader also feeds members by byte range inside a slice, to check
   each one as it is fed and to read around a bad chunk (item 101).
5. **No user or group name is in the stream, and no file's ACL is lost from it** (items 270,
   273, 274). tapectl's own check of the stream as it passes is the proof. The tar version is
   only the early refusal.
6. **A restore is judged by the per-file sha256 check, never by tar's exit status or a
   decompressor's.** busybox tar exits 0 with files missing, and BusyBox `unxz` exits 0
   after writing wrong bytes (both measured).
7. **A staging is refused when tar exits 1 or 2, when the stream's members are not the
   snapshot's list in order with those sizes, or when a file changed under tar** (tapectl's
   own before-and-after comparison decides, whatever tar says).
8. **A test's fixture goes through the real staging path.** A fixture made another way hid
   the slice-number defect (#518) for a month. A helper that builds a staged unit by hand is
   not written.
9. **No compatibility is added back, and no feature is cut.** "Simplify" (item 242) removes
   compatibility with 1.x. It removes no feature (items 246, 248). A worker that finds a
   feature hard to carry stops and reports.
10. **The standard is not lowered to an old tool on the host that writes** (item 273). The
    heir's readers stay wide: the heir's script and File 1 are held to old and small tools.
11. **Compression never reaches past its own slice, and it comes after every check** (items
    280, 284). Each slice is compressed alone, after the cut and before it is encrypted, so
    a lost slice costs only the files in it. Every check of the stream (members, hashes,
    names, ACLs) is made before compression, and a member's offsets in the file table are
    of the tar stream.
12. **Nothing is argued from what the present collection holds** (item 281). A task that
    finds a feature costly does not answer "the data does not need it". It stops and
    reports.
13. **A slice begins at a member's start, or it continues exactly one member that is larger
    than `slice_size`** (item 283). No slice begins anywhere else, and the slices joined in
    order are the stream, byte for byte.

## 6. Commands

```bash
B=<your branch>
T="CARGO_TARGET_DIR=/scratch/tapectl-target-$B flock /scratch/tapectl-build.lock"
env $T cargo check --all-targets          # while iterating
env $T cargo test --no-fail-fast          # the whole ungated suite
env $T cargo test --lib volume::          # one module
env $T cargo test --test integration      # one integration file
env $T cargo clippy --all-targets         # zero warnings
cargo fmt --check
env $T cargo run --example gen_man        # after a clap change
env $T cargo run --example gen_cli_md     # after a clap change
python3 scripts/check-docs.py             # after a docs or clap change
grep -rn 'L3-pending' src tests           # the pending list (Stages 4 and 5)
```

The budget suites (`cargo test --lib cost_budget`, `--test catalog_budget`,
`--test pass_budget`) pin what a path costs. A budget that fails is a cost change to explain
in the report. It is never a number to refresh without the coordinator's word.

---

## S0. Stage 0: measure before any code

Item 253 was ratified with measurements owed, "and a failure reopens it". Three are done
(GNU tar 1.30 and 1.35 as writers, bsdtar as a reader, one 96 GiB member through 96 pieces;
`docs/research/2026-11-16-layout-3-before-the-build.md`). The rest are owed. Each is a gate, and
the table says what it holds back.

| Task | Gates | Owner |
|---|---|---|
| T0.1 The readers of an ACL stored once | Stage 1 (which record the format keeps) | worker |
| T0.2 GNU tar built on Debian 10 | T4.14's host step, and Stage 7 | worker |
| T0.3 Real scale: a made tree of two million files | Stage 4 (item 253's own condition, as item 291 has it) | worker |
| T0.4 The escrow paper | Stage 7 (item 244) | operator |
| T0.5 The decompressors on a slice compressed alone | Stage 1 (the format's section on compression), T4.5 | worker |
| T0.6 The compression libraries in the static build | item 286 (tapectl itself compresses); T4.5 | worker |
| T0.7 The readers on a slice that ends at a member's start | Stage 1 (item 283 was ratified with this owed); T4.10 | worker |

A Stage 0 task changes nothing under `src/`, `tests/` or `scripts/`. Its product is a dated
note under `docs/research/`, which the coordinator lands, and a comment on the issue.

### T0.1 The readers of an ACL stored once

**Owner:** worker. **Depends:** nothing. **Carries out:** #563's second criterion; item 273's
rule ("an ACL is stored once, in the record a reader's ordinary flags restore").
**Read:** #563, whole; the research note §5 "xattrs and ACLs".

**Goal:** decide by measurement which of the two ACL records Layout 3 keeps. `tar --xattrs
--acls` stores an ACL twice: as text (`SCHILY.acl.access`, `SCHILY.acl.default`) and as the
raw extended attribute (`SCHILY.xattr.system.posix_acl_access`, `..._default`). Measured so
far, with GNU tar 1.35 as the reader only: the text record alone restores with the ordinary
`--acls`; the raw record alone restores only with
`--xattrs --xattrs-include='system.posix_acl_*'`.

**Steps:**
1. Work in a new directory under `/scratch`. Make a source tree: two small files; on one,
   `setfacl -m u:1:r,g:2:r`; a directory with a default entry (`setfacl -d -m u:1:rx`); one
   file with `setfattr -n user.note -v x`.
2. With this host's GNU tar (1.35), always with `--format=pax --numeric-owner`, write three
   archives:
   - `text.tar`: `--xattrs --acls --xattrs-exclude='system.posix_acl_*'`
   - `raw.tar`: `--xattrs` (no `--acls`)
   - `both.tar`: `--xattrs --acls` (the control)
   Read each archive's pax records with Python's `tarfile` (`pax_headers`) and record which
   keys each member carries. `text.tar` must carry the `SCHILY.acl.*` keys, by number, and no
   `SCHILY.xattr.system.posix_acl_*` key; it must still carry `SCHILY.xattr.user.note`. If
   it does not, stop: the writer's flags are the finding.
3. Read each archive with each reader, as root and as an ordinary user, into an empty
   directory, with the reader's ordinary flags:
   - GNU tar 1.30 (`debian:10-slim`), 1.34 (`debian:12-slim`), 1.35 (this host):
     `-x --xattrs --acls`
   - bsdtar (the version Phase 0 used, 3.6.2, or what `debian:12-slim` installs as
     `libarchive-tools`; record it): `-x --xattrs --acls -p`
   - busybox tar (`busybox` image): `-x`
4. After each read record: the exit status, every warning, `getfacl` of each file and of the
   directory, and `getfattr -d` of the file with `user.note`.
5. Positive control, required: `both.tar` read by GNU tar 1.35 as root restores the access
   entries, the default entry and `user.note`. If the control fails, the method is wrong and
   nothing else in the table counts.

**Traps:**
- A container hides names: an ACL shows a number where the container's passwd lacks the name.
  Ids 1 and 2 are `daemon` and `bin` in Debian's and Ubuntu's passwd, which is why they are
  used. Check the pax records, not `getfacl`'s spelling, for what the archive holds.
- Debian 10's packages are on `archive.debian.org`. Never install a package on the host.
- Tell the broker before a pull (`BROKER NOTICE:`), remove each image when done and say so.
- An ordinary user cannot restore `trusted.*` or `security.*`; they are not part of this.

**Decision rule, fixed before the measurement:** Layout 3 keeps the **text** record if GNU
tar 1.30 and bsdtar both restore the access and default entries from `text.tar` with the
flags above. If either does not, report the table and stop: the choice goes to the CTO.

**Done when:** the table (reader × archive × root or user → access, default, `user.note`,
exit, warnings) is in a note, with the method written so it can be repeated; #563 has the
result as a comment; the writer's exact flags for the kept record are stated.

### T0.2 GNU tar built from the upstream release on Debian 10

**Owner:** worker. **Depends:** nothing. **Carries out:** item 275's owed measurement; #563.
**Read:** #563; `contrib/hosts/home2-prep.sh` step 7 (how dar 2.7.21 was built there).

**Goal:** show that GNU tar 1.35 or later builds on Debian 10 with ACL support and writes ACL
entries by number, and write down the exact recipe the host step (T4.14) will use.

**Steps:**
1. Look at `https://ftp.gnu.org/gnu/tar/` for the newest release. Use the newest release
   that is at least 1.35; record its version and the sha256 of its `.tar.xz` (1.35's is
   `4d62ff37342ec7aed748535323930c7cf94acf71c3591882b26a7ea50f3edc16`). Verify the signature
   if the keyring is at hand; record whether you did.
2. In a `debian:10-slim` container (bind mount under `/scratch`, `--log-driver none`):
   install `build-essential`, `libacl1-dev`, `libattr1-dev`, `xz-utils` from
   `archive.debian.org`. Record every package and version.
3. `./configure --prefix=/usr/local` with ACL and xattr support on (`./configure --help`
   names the switches; record the line used), `make`, `make install`. Building as root
   needs `FORCE_UNSAFE_CONFIGURE=1`.
4. Prove it: `/usr/local/bin/tar --version`; `ldd /usr/local/bin/tar` names `libacl`; the
   system's `/bin/tar --version` still says 1.30. Then the write test: a file with
   `setfacl -m u:1:r,g:2:r`, archived with `--format=pax --numeric-owner --xattrs --acls`,
   has `user:1:r--` and `group:2:r--` in its `SCHILY.acl.access` record and no name.
5. The negative control, required: the same archive written by the container's `/bin/tar`
   (1.30) holds `user:daemon:r--`. If it holds a number, the test is not looking at what it
   claims to.
6. Run the new tar's own test suite if time allows (`make check`); record the result or that
   it was not run.

**Decision rule, fixed before the measurement:** item 275's way stands if steps 4 and 5
both hold: the tar built there writes the ACL's entries by number, and the system's tar
writes a name. If a tar of 1.35 or later cannot be built on Debian 10 with ACL support,
report what failed and stop: how the production host gets a tar that meets the floor
goes back to the CTO.

**Done when:** a note holds the recipe (packages, configure line, version, sha256), the proof
and the control, with the rule's verdict stated; #563 has a comment; #554's Phase 0 box is
the coordinator's to tick.

### T0.3 Real scale: a made tree of two million files

**Owner:** worker. **Depends:** nothing. **It does not wait for the go**: item 291
ratified a recommendation to run it "now", and it builds nothing. **Carries out:** item
253's third measurement, as item 291 ruled it; #554 Phase 0.

**What item 291 changed.** This task had two legs. The leg over the production host's
largest real unit is no longer run before the code: its proof is taken from the real-drive
acceptance (T6.6, step 1), where a unit of that size is written, read back and restored.
What is left here is the leg item 281 asked for, which needs no production data and no
production host.

**Goal:** a unit of millions of members through create, cut, encrypt, decrypt, join, read,
compare. Done so far: one 96 GiB member of zeros through 96 pieces, and 50,100 small
files.

**Steps:**
1. Under `/scratch`, make a tree of at least two million small files (a few hundred bytes
   to a few kilobytes each) in folders of mixed depth, one in a hundred with a name over
   200 bytes, one in a thousand with a named ACL entry, ten files larger than
   `slice_size`, and one hard link pair in each top folder. Record how it was made.
2. `tar` with the writer's command line (D2), cut at member starts (D3; `aligned.py` on
   #567 has the rule) with `slice_size` 1 GiB, `age -e` each slice, `age -d` each, join,
   `tar -x --to-command=<sha256 of the member>`, and compare every member's hash with a
   second, direct `sha256sum` walk of the tree. Nothing is kept but the two lists.
3. Beside it, the reference stream reader of the research note §6 makes the file table's
   text (D4). Record its size, the time to make it, and the time to find one path's row
   and slice in it with `awk` and with `grep`.
4. Read a sample of a thousand members from their slice alone, as T0.7's (a) does, and
   compare.
5. Record the peak memory of every process, the slice count against the count a plain cut
   would give, and the wall time of each stage.

**Decision rule, fixed before the measurement:** item 253 is reopened only by a content
mismatch, a member the stream reader places wrongly, or a tool that fails or runs out of
memory at this scale. Sizes and times are recorded, not judged, with two exceptions that
go to the CTO as a report: the table's text is over 1 GiB, or finding one path's row
takes over a minute.

**Traps:**
- A few gigabytes under `/scratch`, and hours of disk work: tell the broker before it
  starts (`BROKER NOTICE:`), keep 30G free, and remove the tree when done.
- No tapectl is built or run. The pipeline is tar, a script, `age` and tar.

**Done when:** a dated note under `docs/research/` holds the method and every figure, with
the decision rule's verdict stated; #554 has a comment.

### T0.4 The escrow paper

**Owner:** operator. `age-keygen -y` on the escrow paper's secret must print the escrow
public key the fresh start will adopt (items 39, 194, 244). If it does not, the fresh start
mints a new escrow identity. Gates Stage 7 only.

### T0.5 The decompressors on a slice compressed alone

**Owner:** worker. **Depends:** nothing. **Carries out:** item 280; #565's third criterion.
It measures **all seven** algorithms dar offered (gzip, bzip2, lzo, xz, lzma, zstd, lz4), so
that item 285 (gzip, xz and zstd are carried) is confirmed or reopened on real data and not
on the first check's random words. **Read:** #565, whole, and its first comment (the method and
the script of the first check).

**Goal:** what was measured on 2026-11-16 used this host's tools only. Before the format
document states compression, measure the readers an heir may have.

**Steps:**
1. Make the stream as #565's script does, with the writer's flags, and cut it at member
   starts (item 283; `aligned.py` on #567 has the rule), on a tree
   that also holds real data of several kinds (text, source code, a database dump, images
   that do not shrink). Record where the data came from; use nothing private.
2. For each of the seven, with its own tool, at the tool's default level and at its
   highest, compress each slice alone (item 284) and encrypt it with the real `age`.
3. Read back with each reader: the newest tools on this host; Debian 10's (`debian:10-slim`:
   zstd 1.3.8, xz 5.2.4, gzip 1.9); BusyBox (`busybox` image) where it has the applet. For
   each: every slice decoded alone gives the cut; one file is restored from only its
   slice; a file larger than a slice is restored from its run.
4. Damage, with real `age` in the pipeline: flip one byte of a slice's ciphertext in its
   first, middle and last 64 KiB chunk. Record what `age -d` writes and how it exits, what
   the decompressor after it writes and how it exits, and which members come out whole.
5. The growth bound: for each algorithm, the largest growth over slices of bytes that do
   not shrink, at 4 MiB, 1 GiB and the largest `slice_size` in use.
6. Size and speed on the real data, for each kind of data apart: the packed size, and the
   megabytes a second to compress and to decompress, at the default level and at the
   highest. Staging time is the operator's cost.
7. For each algorithm, which readers exist: its own tool in Debian 10, in this host's
   release, and in BusyBox; whether a stock install of each has it without a package
   being added.
8. The positive control, required: a slice stored with no compression goes through the
   same harness and every row passes. If it does not, the harness is wrong.

**Traps:**
- Tell the broker before a pull; `--log-driver none`; nothing on a container's standard
  output; remove each image when done.
- A decompressor's exit status is recorded, never trusted. Compare bytes.
- tapectl compresses in-process (item 286), and a slice packed by the library is not yet
  at hand here. Say so in the note: T4.5's tests and T4.16's gate legs prove library-made
  slices against the real tools.

**Decision rule, fixed before the measurement:** item 285 (gzip, xz and zstd) is
**confirmed** if, for each of the three, a slice packed by this host's tool at its default
and at its highest level is decoded to the cut by this host's tool and by Debian 10's
stock tool, and its worst growth over bytes that do not shrink is under 1% of the slice.
It is **reopened**, and goes to the CTO with the table, if one of the three fails either
test. BusyBox is recorded and decides nothing: an heir without the algorithm's own tool
is told which tool to get. What the other four algorithms measure is recorded for the
CTO and reopens nothing by itself.

**Done when:** a note holds the tables (reader × algorithm), the damage rows, the growth
bound, and size and speed by kind of data, with the method so it can be repeated and the
decision rule's verdict stated; #565 has a comment.

### T0.6 The compression libraries in the static build

**Owner:** worker. **Depends:** nothing. **Carries out:** #565; item 286 rests on it. **Read:** `.github/workflows/release.yml` (how the static musl
binary is built); `Cargo.toml`.

**Goal:** item 286 has tapectl compress in-process. That adds a library for each algorithm
carried to a binary that is built static for musl, and none has been tried there. Try it
outside tapectl's tree. **If it fails, the fallback is one process of the host's tool for
each slice**, which is a second writer's tool under items 273 and 274 and goes to the CTO
before anything is built on it: say so plainly in the note.

**Steps:**
1. In a new, empty crate under `/scratch` (never in the repo), depend on one maintained
   library for each of gzip, xz and zstd (item 285; if T0.5 reopens that item, the others
   are tried then). Try first: `flate2` with its pure-Rust backend for gzip; `liblzma`
   (the maintained successor of `xz2`) with its bundled C source for xz; `zstd` with its
   bundled C source. If one does not build, try one other maintained crate for that
   algorithm before reporting. Record each crate, its version, its licence,
   whether it builds C source, and its last release date.
2. Build it in **debug** for `x86_64-unknown-linux-musl`, the way `release.yml` sets the
   target up. Record what had to be installed and every failure.
3. Its one program compresses a file as one frame for each algorithm and decompresses it
   again, at the level D15 proposes as the algorithm's default and at one other (item
   287). For each: the frame is read by the algorithm's own tool on this host, and a frame
   made by the tool is read by the program.
4. `ldd` on the binary says it is static. Record its size with and without each library.
5. Run `cargo audit` on the crate's lock file and record the result.

**Traps:**
- Debug only. No release build. `CARGO_TARGET_DIR` under `/scratch`, under the build lock.
- An algorithm with no library that writes the frame its own tool reads is a finding, not
  something to work around: say which.

**Decision rule, fixed before the measurement:** a library is **accepted** if it builds
into the static musl debug binary with nothing installed on the host beyond what
`release.yml` installs, `ldd` says the binary is static, the algorithm's tool reads the
library's frame and the library reads the tool's, at both levels, and `cargo audit` names
no advisory for it. If no library is accepted for an algorithm, item 286's fallback goes
to the CTO with the note, and nothing is built on it before the answer.

**Done when:** a note holds the table (algorithm, crate, builds static or not, tool reads
library frame, library reads tool frame, size added) and the rule's verdict for each
algorithm; #565 has a comment.

### T0.7 The readers on a slice that ends at a member's start

**Owner:** worker. **Depends:** nothing. **Carries out:** item 283's owed measurement; #567.
**Read:** #567, whole, and its first comment (the script `aligned.py` and its output).

**Goal:** item 283 was measured with GNU tar 1.35 and BusyBox tar 1.36.1 only. Before the
format document states the cut, measure the other readers an heir may have, on a slice that
carries no end-of-archive blocks.

**Steps:**
1. Make the unit and the cut as #567's script does, with the writer's command line (D2)
   and `slice_size` 1 MiB. Add to its tree: a name over 255 bytes, a name with a newline in
   it, a FIFO, and a folder with a default ACL entry.
2. The readers: GNU tar 1.30 (`debian:10-slim`), 1.34 (`debian:12-slim`) and 1.35 (this
   host); bsdtar (what `debian:12-slim` installs as `libarchive-tools`; record its
   version); BusyBox tar (the `busybox` image).
3. For each reader, with no file table, told only which slices are one long member's run:
   - (a) each slice (a run joined) in its own run of the reader, in order, into one
     directory;
   - (b) the good slices joined in order in **one** run, with the fullest slice left out;
   - (c) the last slice alone (it carries tar's end-of-archive blocks) and the one before
     it alone (it carries none);
   - (d) one long member's run, joined, alone;
   - (e) one member named to the reader out of a slice of many, by its name as the file
     table spells it: an ordinary name, the name with a newline, and a folder's own member
     without what is under it.
4. Record for each: the exit status, every warning, the files whole by sha256, and where
   the reader restores them the hard link, the xattrs, the ACLs and the folders' mtimes.
5. The controls, both required. Positive: the whole stream read by GNU tar 1.35 restores
   every file. Negative: a plain cut at 1 MiB, read as in (a) by GNU tar 1.35, fails every
   run, as #567 measured. If either control does not hold, the harness is wrong and
   nothing else in the table counts.

**Decision rule, fixed before the measurement:** item 283 stands if GNU tar 1.30 and bsdtar
each exit 0 in (a) to (d) and restore every file outside a left-out slice. BusyBox tar is
held to content only (rule 10). If GNU tar 1.30 or bsdtar fails one of these, report the
table and stop: item 283 goes back to the CTO. Step (e) decides only how the heir's script
restores one path (D10). **By name**, if every reader in step 2, BusyBox included,
restores exactly the named member and nothing else for all three names, at exit 0.
**By its offsets** otherwise: the script then cuts the member out of its slice with
`tail -c +N | head -c M`, each cut its own pipeline, as #556 measured.

**Traps:**
- Tell the broker before a pull; `--log-driver none`; nothing on a container's standard
  output; remove each image when done and say so. Never install a package on the host.
- A reader's warning about a missing end of archive is recorded, never hidden.
- Run the readers as an ordinary user and as root: xattrs and ACLs differ.

**Done when:** the table (reader x case) is in a note under `docs/research/`, with the
method so it can be repeated and the decision rule's verdict stated; #567 has a comment.
---

## S1. Stage 1: the normative set (no code)

**Owner: the coordinator. Never a worker.** A worker was not in the design sessions, and
this stage decides bytes on tape. **Starts when:** the CTO gives the go, T0.1 has picked
the ACL record, and T0.7 has held item 283's cut to the readers still owed. Its section on
compression is written when T0.5 and T0.6 are in; the rest does not wait for them. **Lands on:** `master`, as documents. Until `layout-3` is merged, `CLAUDE.md`
keeps saying that the code writes Layout 2.

### What Stage 1 writes

| Task | Product |
|---|---|
| T1.1 | `docs/design/volume-format-v3.md`: the byte format, whole and standing alone. Zone order; File 0; File 1; the front index; the envelopes and what each carries; the data zone (the stream, the cut, the slice); the file table's and the id map's grammar; the seal, full and short; the writer at the early warning (W1 to W9, carried word for word from `volume-format-v2.md` §4.3); what follows the warning (§4.2 rewritten by item 288: no unit is `cut`); a range and every unit continuing (§4.4 rewritten: the writer's half by items 288 and 289, the reader's by items 255, 260, 264, 266, 267 and 283); a tape of another layout; the heir path end to end, with the script's modes; "self-describing: what that promises". It carries every rule of #562. |
| T1.2 | `docs/design/layout-session.md`, amended for Layout 3: the staging's products (the file table is one, and a write refuses a unit without it), contact with a tape of another layout, "where a restore starts". |
| T1.3 | `CONTEXT.md`: the terms *stream*, *member*, *file table*, *id map*, *the writer's floor*; *slice* redefined (whole members, item 283) and *run* added (a long member's slices); `cut` and the opt-in to spanning struck (items 288, 289); dar's terms struck. The id map is never called "ownership", which means the tenant that owns a unit. |
| T1.4 | ADR-0014 (0013 was taken by the forensics record), which gathers items 239 to 289 into one decision record for Layout 3, so that a reader does not need ADR-0012's amendments in order. ADR-0012 is not edited. |
| T1.5 | The baseline schema as a reviewed listing in the format document's appendix: every table and column of the 1.9.2 schema that is kept, the ones that go, and the new ones (D9). The SQL itself is T3.1's. |
| T1.6 | `.claude/commands/worktree-agent.md`: guardrail 3 names the normative set by stage (this plan's section 1) in place of the Layout 2 set. `docs/design-errata.md`: `volume-format-v2.md` is superseded for Layout 3. |
| T1.7 | This plan: every "Stage 1 decides" marker in Stages 4 and 5 replaced by the section of the format document that decides it. |

### The format document's outline (determined)

Determined by the coordinator on 2026-11-17, so that T1.1 is transcription and checking.
Each section of `docs/design/volume-format-v3.md` is carried from a section of
`volume-format-v2.md`, rewritten from it, or new, and the last column says what decides
it. "Carried" means the text is copied and only its version words change. Where a carried
section names dar, a dar catalogue, `catalogs/`, a slice's header or trailer, `short-seal`
or `spanning` as a `requires` entry, a `cut` unit or `may_span`, that sentence is
rewritten by the item named and nothing else in the section moves.

| Section of the Layout 3 document | From | Decided by |
|---|---|---|
| 1. Zone order, fixed 512 KiB blocks | v2 §1, carried; File 2 is the heir's script as one file | D10; item 239 |
| 1.1 File 0 and its `[media]` table | v2 §1.1, carried | D1; #547 (strings escaped as TOML) |
| 1.2 `requires`, and keys a reader does not know | v2 §1.2, rewritten: no entry is defined | D1; item 258(a) as amended by 288 |
| 1.3 Envelope backup copies | v2 §1.3, carried | item 44 |
| 1.4 The operator envelope's `catalog.db` | v2 §1.4, rewritten: one generation | D7, D9; item 258(d) |
| 1.5 The cross-volume index | v2 §1.5, carried | D8; #528 |
| 1.6 The file table and the id map | new | D4, D5; items 254, 265, 272, 276 to 278 |
| 1.7 The damaged-file list | v2 §1.6, carried | item 255 |
| 2. Plaintext and encrypted; the isolation invariant | v2 §2, carried, with the id map's needles and the two disclosures | invariant 3; items 283 (a slice's size follows its files) and 284 (and how well it packed) |
| 3. The data zone: the stream, its checks, the cut, a run, a slice on tape, compression | new | D2, D3, D12, D15, D22; items 253, 270, 274, 282 to 287 |
| 4. The front index | v2 §3, carried, the magic moved | D1 |
| 5. The seal marker and the chain | v2 §4 and §4.1, carried: the short seal is part of the layout | item 258(a) |
| 5.1 What a short tape promises | v2 §4.2, rewritten: no unit is `cut`; a slice past `ended_after` is withdrawn, and a staging with slices before it is a range | item 288 |
| 5.2 The writer at the early warning | v2 §4.3, W1 to W9 **word for word** | items 110 to 114, 240 |
| 5.3 A range, and every unit continuing | v2 §4.4, rewritten. The writer: no opt-in, one class, one bound. The reader: R1 to R9 are kept where they speak of `ended_after`, of accepting a cartridge by hash (R5, R8, R9) and of what a tape alone gives; what they say of `may_span` or of a spannable set goes | items 288, 289; D19, D20; items 255, 260, 264, 266, 267, 283 |
| 5.4 A range in `volume rewrite` and in compaction | new | D21 |
| 6. What confirm verifies | v2 §5, carried | invariant 2 |
| 7. Readers: tapectl's own; the heir's path end to end; which tars restore what; a tape of another layout | v2 §6, rewritten | D10, D16, D17; items 101, 131, 264, 266, 267, 269, 271, 278; T0.1 and T0.7's results |
| 8. Block mode | v2 §7, carried | |
| 9. What Layout 3 removes | new: dar and its catalogue, a slice's framing, `catalogs/`, the three generations of `catalog.db`, the `cut` unit, the opt-in to spanning, the `requires` entries, the heir's `--slices-from`, `--swap-tapes`, `--stream-tapes`, `--fill-from` and salvage mode | items 241, 242, 258, 288; D10 |
| Appendix A. The baseline schema | T1.5's listing | D9 |
| Appendix B. #562's rules, each with the section that carries it | this plan's Appendix A, third table | #562 |

T1.1 is done when every row above has its section, every rule of #562 is in Appendix B
with a section number, and a search of the document for `dar`, `cut unit`, `may_span`,
`spannable` and `plain cut` finds them only outside the running text: in section 9 and in
Appendix A, which name what goes.

### What Stage 1 writes down: D1 to D22, ratified

No ADR item fixed these at the level of bytes, schema or command line, so each was put to
the CTO as a proposal. **All twenty-two were ratified as they read here on 2026-11-17
(ADR-0012 item 290)**, and the amendment of that date holds each one's text. Stage 1
writes them into the format document; it does not reopen them. Where one of them is
ambiguous in a way that changes bytes on tape, the schema or a command's spelling, the
coordinator does not pick: the question goes to the CTO. Where a measurement decides a
point (T0.1 the ACL record, T0.7 how one path is taken from a slice), the rule is fixed in
that task and its result is the answer.

**D1. The layout's identity.** `layout_version = 3`. The magic becomes `tapectl-volume-v3`
in File 0, and the front index's and the seal marker's own magics move with it (#562: a
Layout 3 File 0 carries nothing a Layout 2 script could follow). `requires` is empty on
every Layout 3 tape: the short seal and a unit that continues on another cartridge are
both part of Layout 3 (items 258(a), 288, which superseded the `spanning` entry). The key
stays, so that a later feature can fence an older reader.

**D2. The stream.** One pax stream for each staging. The writer's command line is fixed in
the format document, flag by flag, from the commands #555 and the research note measured:

- `--create --file=- --format=pax`
- `--numeric-owner` (item 270), `--xattrs`, `--acls`, and the flags T0.1 shows store an ACL
  once in the kept record
- `--no-recursion --null --verbatim-files-from --files-from=<list>`: the list is tapectl's,
  NUL-separated, every name prefixed `./`, sorted bytewise, folders listed as their own
  entries, the unit's dotfile included
- `--pax-option=exthdr.name=%d/PaxHeaders/%f`, with access and change times not stored
- no `--sparse` (a sparse file is stored as its bytes), no compression
- run in the unit's root, with `LC_ALL=C`, and with `TAR_OPTIONS` and `POSIXLY_CORRECT`
  removed from its environment (GNU tar reads options from `TAR_OPTIONS`). If
  `--directory` is used in place of the working directory it comes **before**
  `--files-from`: tar takes it in order, and after the list it reads the names against the
  wrong directory and exits 2 (measured)

**D3. The cut, and what each size means** (items 283, 284). The cutter works at member
starts. The stream reader gives it each member's start and whole length (its extension
headers, its header and its padded data) before any of the member's bytes:

- a member that fits in what is left of the slice goes into it;
- a member that does not fit, and is no larger than `slice_size`, ends the slice before it
  and begins the next;
- a member larger than `slice_size` ends the slice before it and is cut at every
  `slice_size` bytes, its last piece ending where the member ends. Those slices are its
  *run*, and the member after it begins a new slice;
- the stream's tail (tar's end-of-archive blocks and its padding) is placed as a member
  is.

`slice_size` is the most a slice's cut holds, and is a multiple of 512. `size_bytes` and
`sha256_plain` are of the cut, before compression. `encrypted_bytes` and `sha256_encrypted`
are of the tape file. A compressed slice can be larger on tape than its cut: the format
states the worst growth for each algorithm, and the early warning's rules and
`post_warning_bytes` are held to `slice_size` plus that growth.

**D4. The file table's text** (items 254, 264; #562). One file for each staging the tape
holds, in that tenant's envelope, at `tables/<staging>.tsv`, named as `catalogs/` names a
staging's dar catalogue today. Tab-separated, one row for each member, in stream order,
after a header:

```
#tapectl-file-table 1
#stream_bytes <length of the stream>
<kind> <slice> <start> <end> <size> <sha256> <path> [<link target>]
```

- `kind`: `f` file, `d` folder, `l` symbolic link, `h` hard link, `p` FIFO, `c` and `b`
  device nodes.
- `slice`: the number of the slice in which the member begins (item 283). A member larger
  than `slice_size` also holds the slices after it, as many as its length needs (D3). A
  slice's own offset in the stream is the `start` of the first member that begins in it.
- `start`: the offset in the stream of the member's first extension header. `end`: the
  offset just past the member's last padded block. A row's `start` is the row before's
  `end`; the first is 0.
- `size`: the file's bytes; 0 for a kind with no data.
- `sha256`: of the file's bytes as they went to tape, for `f`; the target's, for `h`; `-`
  for the others.
- `path` and `link target` are escaped by the one rule `index/paths.tsv` already uses
  (`volume::index::escape`). The target is stated for `h` only; a symbolic link's target is
  in the stream.

**D5. The id map's text** (items 272, 276, 277). One file for each staging, beside its
table, at `tables/<staging>.ids.tsv`: rows `u <id> <name>` and `g <id> <name>`, sorted by
kind and then by number. The name is empty when the host knew none. An id the stream does
not carry has no row.

**D6. The MANIFEST's block for a staging.** dar's fields go (the catalogue's name, dar's
version, anything about dar's slice framing). New: `payload = "tar"`, `stream_bytes`, the
table's and the id map's names in the envelope, the writer's `tar --version` line (for a
reader's diagnosis only), and `compression`, which names the algorithm and is stated only
when the staging is compressed (D15). Kept: the unit, version and tenant facts, the slice
list with each slice's size and plaintext hash (of the cut, D3), its size on tape, its
ciphertext sha256 and its tape position. `may_span` goes (item 288): a tape that holds only
some of a staging's slices is known by the positions the block gives and by the seal. How
a long member's run is marked is D22.

**D7. The on-tape `catalog.db`.** One generation. It carries, for each staging on the tape,
the file table and the id map as the catalog database holds them, so that a rebuild restores
both and a copy written after a rebuild carries both (#557, #564).

**D8. The index.** `index/units.tsv` and `index/paths.tsv` stay the listing by version across
tapes. `index/paths.tsv` states no sha256 for the unit dotfile (#562, #528).

**D9. The baseline schema** (item 258(b)). One file, `src/db/migrations/001_baseline.sql`.
A 2.x catalog is known by SQLite's `application_id`, set by the baseline. A database that
has tables and lacks it is a 1.x catalog and is refused by name. Two new tables:
`stage_files` (the file table: staging, sequence, kind, slice, path, size, sha256, start,
end, link target) and `stage_ids` (the id map: staging, kind, id, name or NULL). Both outlive the
release of the staging's slices (item 265). **Until `v2.0.0` is tagged the baseline is
edited in place; no migration 002 is written.** After the tag the baseline is frozen and
changes are numbered migrations again. Items 284 to 289 add `stage_sets.compression_level`
beside `compression`, and take away `allow_spanning` (an archive set's and a unit's frozen
copy), `writes.may_span` and the write status `cut`. Those three are in the baseline T3.1
makes, because the code still reads them then, and T5.1 removes them with that code.

*Determined, for T1.5's listing* (names follow the 1.9.2 schema's own habits, and T1.5
checks each against it before it is written down):

- `stage_files`: `stage_set_id` (the staging, deleted with it), `seq` (the member's place
  in the stream, from 0), `kind`, `slice`, `path_id` (into `paths`; no path text is stored
  twice), `size`, `sha256` (32 raw bytes, NULL where the table's text has `-`), `start`,
  `end`, `link_path_id` (NULL but for kind `h`). Primary key `(stage_set_id, seq)`; an
  index on `(stage_set_id, path_id)`.
- `stage_ids`: `stage_set_id`, `kind` (`u` or `g`), `id`, `name` (NULL where the host
  knew none). Primary key `(stage_set_id, kind, id)`.
- `stage_sets` gains `tar_version`, `tar_command`, `stream_bytes` and
  `compression_level`, and loses dar's columns (D11).
- `stage_slices` gains `continues` (D22), 0 or 1.
- `snapshot_left_out` (D14, #542): `snapshot_id`, `seq`, `reason` (`pattern` or
  `socket`), `pattern` (NULL for a socket), `path` (text, escaped as the index is),
  `kind` (`file` or `folder`), `files` and `bytes` (what was under a folder; 1 and the
  file's size for a file). Primary key `(snapshot_id, seq)`.
- A `drills` table for D18: `id`, `recorded_at`, `volume_id`, `script_sha256`,
  `tar_version`, `age_version`, `note`.

**D10. The heir's script.** One file on tape, `RESTORE.sh`, and one file in the tree,
`src/volume/heir/RESTORE.sh`, the same bytes on every tape: what differs between tapes is
read from File 0 and the MANIFEST when it runs. Its awk programs are in the same file. It
keeps today's spellings where the mode stays:

| Mode | What it does |
|---|---|
| `--info` | what the tape is, with no key |
| `--verify` | reads and hashes every file of the tape against the front index, with no key |
| `--find-envelope --key K` | which envelope the key opens, and its units |
| `--list --key K [--unit U] [--version N]` | a unit's files, from the file table |
| `--restore --key K --to DIR` with `--all`, or `--unit U` (repeatable) and `--version N` | whole units |
| `--restore ... --unit U --path P` (repeatable), or `--paths-from FILE` | the named paths of one unit, by the file table. With the damaged-file list as FILE this is the one repair (item 255) |
| `--where UNIT`, `--where --all`, `--find TEXT` | the index modes, as today |
| `--work DIR`, `--no-space-check`, `--overwrite`, `--no-file-check` | as today; the work directory is made inside `--to` by default (item 271) and `--work` replaces `--scratch` |

**The reader's primitive** (item 283). Each slice is checked against the front index,
decrypted into the work directory and, where its staging is compressed, decompressed and
checked against its `size_bytes`, all before tar sees it. The good slices go, in order, to
one tar run. A bad slice is left out and its members are listed from the file table. A
long member's run (D22) is fed piece by piece, each piece checked first, so that no more
than one slice is ever held: if a piece fails, that tar run is ended, the member's partial
file is removed and the member is listed, and the next run starts at the slice after the
run. This needs room for one slice in the work directory, and the space check counts it. One path is restored by reading the
slice it is in and naming the member to tar, or by cutting the member out by its offsets:
T0.7's step (e) decides which.

One prompt, as items 266 and 267 have it: the next cartridge of a unit that continues
there (any unit may, item 288), where "I do not have it" is an answer. The restore goes on
across the swap as one tar run. With that answer, or on a tape read alone, every whole file
the tape holds is restored (item 283) and the script says which cartridges hold the rest;
only a file larger than a slice that lies across the two cartridges needs both. A unit
read in more than one run can lose folder times, and a hard-linked name whose first name
is on a tape not yet read is not restored: the script lists those names for the repair.
Gone: `--slices-from`, `--swap-tapes`, `--stream-tapes`, `--fill-from`, the salvage mode. After a restore it prints the damaged-file list, the id map
of each unit restored, and which tar it used and what that tar does not restore.

**D11. The command line and the config.**
- `[dar]` becomes `[tar]` with one key, `binary`. With item 258(d) an old key is an unknown
  key, refused as any unknown key is.
- `preserve_fsa` goes (item 253). `preserve_xattrs` and `preserve_acls` each come to mean
  what they say: the first gives tar `--xattrs`, the second `--acls`, both on by default.
  Today the first drops ACLs too and the second does nothing.
- `[staging] hash_threads` goes: the hash is taken from the stream, by one reader.
- `compression` stays, in `[defaults]`, an archive set and a unit's dotfile, with the values
  `none`, `gzip`, `xz` and `zstd` (item 285); any other value is refused as an invalid
  value is today. The check that asks the local dar which algorithms it was built with
  goes. `compression_level` joins it in the same three layers (item 287; D15 has the
  levels).
- `allow_spanning` goes from `[defaults]`, from an archive set (its `--allow-spanning`
  flag too) and from a unit's dotfile (items 288, 289). It is then an unknown key, refused
  as any unknown key is. T5.1 removes it.
- `config check`, `init`, `setup` and `stage create` probe tar and name the floor.
- `restore unit|file|volume` lose `--spool` and the scratch directory (tar is fed from
  memory, slice by slice, and a one-path restore reads only the slices it needs). They print the id map of what they
  restored, and `--json` carries it.
- The names that say dar change: the JSON keys `dar_version`, `total_dar_size` and
  `dar_bytes` of `stage info` and `stage create` become `tar_version` and `stream_bytes`;
  in the schema `stage_sets.dar_version`, `dar_command` and `total_dar_size` become
  `tar_version`, `tar_command` and `stream_bytes`, `catalog_path` goes,
  `dar_create_reports` becomes `tar_create_reports`, and the `dar_*` columns of `restores`
  become `tar_*`.

**D12. The stream's checks, exactly** (items 253, 270, 274; invariants 5 and 7). For each
member, as it passes: its path, kind and size are the next entry of the list; its header's
user and group name fields are empty and it has no `uname` or `gname` pax record; every
qualifier in an ACL record is a number; a listed file that had an ACL on disk before tar
ran (the `system.posix_acl_access` or `system.posix_acl_default` attribute is present)
carries the kept ACL record. After tar exits: status 0; every listed entry was seen; each
file's device, inode, size, mtime and ctime are what they were before. Each failure is a
refusal that names the path and the rule.

**D13. How a name is found for the id map.** The C library's lookup by id. The released
binary is static, so what it can resolve (the local files; a directory service) is measured
with that binary and written into File 1 and the guide (#564's fifth criterion).

**D14. Exclusions** (item 252, #542). tapectl's matcher is the only one: tar is given the
exact list, so the two matchers that disagreed become one. A snapshot records what each
pattern left out, and reports it. `init` writes an empty list.

**D15. Compression** (items 280, 284 to 287; #565 holds the measurements and the history
of the two shapes that were weighed). **Ruled:** it is carried, off by default, chosen by
`[defaults]`, an archive set or a unit's dotfile as today; each slice is compressed alone,
after the cut and before it is encrypted; the algorithms are gzip, xz and zstd; tapectl
itself compresses, with a library for each; an archive may set `compression_level`. T0.5
confirms the three algorithms on real data or reopens item 285. T0.6 tries the libraries
in the static build; if one fails, the fallback (the host's tool for each slice) goes to
the CTO before anything is built on it.

The detail, ratified with the rest of this list (item 290):

- **The frame.** One complete stream of the algorithm for each slice, as the algorithm's
  own tool writes and reads it, so that `age -d`, then the tool, gives the cut. tapectl
  adds nothing around the frame. **Determined** (the frame's fields): gzip is one
  member, with no file name and a zero time in its header; xz is one `.xz` stream with
  the CRC64 check, as the `xz` tool writes by default; zstd is one frame with its content
  checksum on and the content size not stated, written as the cut comes (corrected
  2026-11-18 at T4.5's integration: a stated size needs the whole cut first, which held up
  to `slice_size` in memory per slice; the size is not a ruled byte, item 290 names only
  the frame). A slice is compressed on one thread, so that
  memory is bounded by the level.
- **The levels.** `compression_level` is a whole number in the algorithm's own range
  (**determined:** gzip 1 to 9, xz 0 to 9, zstd 1 to 19). When it is not set the
  algorithm's own default applies (gzip 6, xz 6, zstd 3). A level
  outside the range is refused, naming the key. The level is recorded with the staging
  and no reader needs it.
- **The worst growth** of each algorithm, stated in the format document from T0.5's bound.
  D3 holds the early warning's rules to `slice_size` plus it.
- **The check at staging.** Item 253 takes each file's sha256 "from the bytes that go to
  tape", and with compression the bytes are packed after the hash. So staging decodes
  every slice it has just packed and compares it with the cut before the bytes go on, and
  a difference refuses the staging. That is a check made in every staging, not a test.
  Without it, item 253's sentence would have to be reworded for a compressed unit.

**D16. tapectl's own check after a restore.** Today tapectl's restore hashes restored files
only after damage; the heir's script hashes every file every time. For Layout 3: tapectl
checks every member's hash against the file table as it feeds tar (always), and after tar
ends hashes every restored file on disk, as the script does, unless `--no-file-check`.
Invariant 6 needs the second half; it costs one more read of what was restored.

**D17. A range and `restore volume`.** Today `restore volume` refuses a version this tape
holds only a range of. Item 266 lets a restore start on any tape of a span and take the
files that tape holds whole. Proposed: `restore volume` and `restore unit` do that when the
other cartridges are not offered, and list the rest; with the other cartridges at hand they
go on across them as today.

**D18. The drill's record** (item 247, #548). A command `drill record LABEL` writes one
event: when, which volume, which script and tar. `audit` gains a check that warns when no
drill is recorded or the newest is older than `[defaults] drill_interval_days`, 365 when
absent.

**D19. The queue, and what a batch is** (item 289). `collection run` takes units in the
collection's order and skips none. Units are added while the plan's end is at or below
the cartridge's nominal capacity. The unit that carries the plan past it is the batch's
last, and it is planned whole. A batch is still bounded by the staging area (item 85): if
the staging area ends the batch first, the tape ends by its plan, with room left. The
next-fit skip over a unit that does not fit, the two classes and `after_the_tail` go.

**D20. The one plan bound** (item 289). On a drive that declares `post_warning_bytes` a
plan lists slices in order to the end of the unit that passes the cartridge's nominal
capacity, and the early warning ends the tape. On a drive without the key the plan stops
at the last slice that fits under the fill ceiling, the tape is sealed in full there, and
the slices after it continue on the next cartridge. An early warning before the plan's
end seals short in both cases. A slice the tape did not take has no written position: it
is never a copy and is never counted.

**D21. A range in `volume rewrite` and in compaction** (item 133 is superseded by item
288, and its design was owed). A rewrite or a compaction carries a volume's live slices as
they are: the same slices of the same staging, by ciphertext, in slice order. The new
volume's block for that staging lists every slice of it and gives positions to the ones
this volume holds, exactly as a range written from staging does. Whether a slice is live
is its version's state, as for a whole unit. `volume rewrite`'s typed consent names every
version of which the volume is the only holder of some slice, and its hold stays until
the new volume reads back.

**D22. A long member's run on tape** (item 283). In the MANIFEST's slice list, a slice that
continues a member begun in the slice before it states `continues = true`. A slice without
it begins at a member's start. A reader joins a slice with the `continues` slices that
follow it before tar sees any of them, and when one of them is lost the member is lost and
listed. No plaintext file says which slices are a run.
---

## S2. Stage 2: fifteen fixes that depend on nothing else

**Starts when:** the CTO gives the go. **Lands on:** `master`. **Normative:** the Layout 2
set. No task here changes a byte a tape carries, the schema, or a file under `scripts/`.
(Earlier texts say "sixteen fixes". The count is fifteen: #527, #529, #530, #531, #533,
#534, #535, #536, #537, #538, #539, #540, #541, #544, #546.) Checked on 2026-11-17 against
items 288 and 289: none of the fifteen is a fix to code those items remove (the `cut`
unit, the opt-in, the plan's two classes), so the stage stands as written.

**The issue is the specification.** Each issue's body states the defect, the cited lines and
the acceptance criteria. This plan adds what the issue cannot know: who owns which file, the
order, what has been ruled since, and where an issue's own text is wrong. Where a ruling
below and an issue's body differ, the ruling wins.

**Rulings that settle what the issues left open:**

| Issue | Ruled | Item |
|---|---|---|
| #527 | `catalog rebuild` **refuses** a unit whose name matches a catalog unit under another uuid | 249 |
| #529 | a unit whose newest snapshot was never staged is **pending** | 247 |
| #530 | `db fsck --repair` **refuses** on a failed integrity check | 249 |
| #534 | `audit` **warns** of a folder under a collection root that is not a unit | 247 |
| #535 | no notification is built in; the command line is made sufficient for reporting tools | 250 |
| #539 | `snapshot create --all-pending` exits **non-zero** when it skipped a unit | 249 |
| #544 | the hint after a stopped write names the **whole copy command** (item 218) | 249 |

**Seven tasks, one worker each, in three waves.** The grouping in #554 (a to f) was drawn
before the code was read for it and is wrong in four places; this one was checked against
every cited file on `dc7e7605`. Inside a task the issues are worked in the order listed, one
commit or more for each issue, never one commit for two.

| Wave | Task | Issues | The files only this task may change in its wave |
|---|---|---|---|
| 1 | T2.1 | #531, #541 | `src/volume/session.rs`, `tests/resume_session.rs`, `tests/post_warning_room.rs` |
| 1 | T2.2 | #536, #537, #544 (parts 1, 3, 4) | `src/volume/erase.rs`, `src/volume/rewrite.rs`, `src/volume/write.rs`, `src/volume/binding.rs`, `src/error.rs`, `src/cli/volume.rs`, `src/cli/cartridge.rs`, `docs/troubleshooting.md`, `tests/integration.rs`, `tests/retire_family_locks.rs`, `tests/dry_run_contract.rs` |
| 1 | T2.3 | #529, #534, #539 | `src/collection/fingerprint.rs`, `src/collection/sync.rs`, `src/collection/status.rs`, `src/unit/content_match.rs`, `src/for_location.rs`, `src/cli/status.rs`, `src/cli/audit.rs`, `src/cli/report.rs`, `src/cli/snapshot.rs`, `src/cli/unit.rs`, `src/cli/operations.rs`, `src/cli/mod.rs`, `docs/operator-guide.md`, `tests/cli_smoke.rs`, `tests/collection.rs`, `tests/audit_exit_codes.rs` |
| 2 | T2.4 | #530, #538, #540, #544 (part 2) | `src/main.rs`, `src/db/mod.rs`, `src/cli/db.rs`, `src/cli/operations.rs`, `src/cli/snapshot.rs`, `src/cli/mod.rs`, `src/setup.rs`, `src/staging/clean.rs`, `docs/install.md`, `docs/keys-and-recovery.md`, `tests/cli_smoke.rs`, `tests/setup.rs`, `tests/dry_run_contract.rs`, `tests/dry_run_global.rs` |
| 2 | T2.5 | #527 | `src/volume/rebuild.rs`, `src/cli/catalog.rs`, `tests/catalog_rebuild.rs` |
| 2 | T2.6 | #533, #546 | `src/crypto/keys.rs`, `tests/adopt_key.rs`, `.github/workflows/release.yml`, `docs/development.md` |
| 3 | T2.7 | #535 | `src/cli/status.rs`, `src/cli/report.rs`, `src/cli/audit.rs`, `src/cli/mod.rs`, `docs/operator-guide.md`, `tests/scheduled_*.rs` |

**Generated pages.** `gen_man` and `gen_cli_md` rewrite every page. A worker commits only
the pages of the commands it changed and reports the rest as noise. The coordinator
regenerates all of them at integration.

**After each wave** the coordinator integrates, runs the full suite and the lints, pushes,
and waits for master's CI. **After wave 3** the coordinator runs the whole tape chain on
`master`: T2.1 and T2.2 change the write path, and only the tape proves that.

### T2.1 The write session: a resume after a recorded warning, and a split session (#531, #541)

**Read:** `docs/design/layout-session.md` "Resume" and "Spanning"; `volume-format-v2.md`
§4.2, §4.3 (W1 to W9); ADR-0012 items 113, 114, 174, 186.

1. **#541 first** (it is smaller and #531's tests lean on a consistent session). The three
   places that mark a session's writes `aborted` one row at a time run inside one
   transaction, as the fourth already does. `rehydrate` and `load` refuse a session whose
   writes are split, by name, where today an `.expect` panics.
2. **#531.** After a recorded early warning, `resume` must not seal from the medium alone
   when the session saw a Logical Block Protection refusal after the warning: the file at
   the drive's position is re-sent from its planned start (item 174's rule) before the seal,
   and a session that saw a refusal confirms in full (item 186).

**Traps:**
- Invariant 1: the seal marker is written only after the last content file. Every new path
  through `seal_after_recorded_warning` must hold it.
- #531's third criterion changes a text in `src/volume/write.rs` (`finish_confirm`). That
  file is T2.2's. State the exact new text in your report; T2.2 or the coordinator places
  it.
- #531's fourth criterion is a real-drive run. It is not yours: it is in T6.6.
- The test hooks `TAPECTL_TEST_EARLY_WARNING_AT` and `TAPECTL_TEST_LBP_CORRUPT_BLOCK` exist
  for exactly these cases. Use them through `MemStore` and the session's own tests
  (`mod lbp`, `mod short_ending`); never a real device.

**Done when:** each issue's criteria that are code are met, each with a test that fails on
the parent commit; `cargo test --lib volume::session` and the full suite pass.

### T2.2 Erase, rewrite and the remedy texts of the write path (#536, #537, #544)

**Read:** ADR-0008 (the consent tiers); ADR-0012 items 105 to 107 (`volume rewrite`), 218
and 249; `docs/design/layout-session.md` on the hold.

1. **#536.** `cartridge erase` on a tape that already reads blank or EMPTY runs the catalog
   cross-check **before** it returns "nothing to erase", so a volume the catalog still
   counts there is recorded erased (or refused, as the issue's criteria say).
2. **#537**, which is the same defect seen from `volume rewrite --resume`: the erase step
   calls `cartridge_erase`, gets "nothing to erase", records nothing, and loops. With #536
   fixed, prove the loop is gone with a test that fails on the parent, and fix what remains.
   Record the erase through the public `operations::cartridge_mark_erased` or by making
   `erase::record` reachable; do not edit `src/cli/operations.rs`.
3. **#544**, parts 1, 3 and 4: `left_initialized_line` and `after_stop_text` name the whole
   copy command (`volume write L --init --full-confirm --move-to X`, with what the stopped
   write was given); `finish_confirm`'s catch-all and the hint at the medium's end name a
   command that does what the text says. Part 2 (the Tier 3 refusal in
   `src/cli/operations.rs`) is T2.4's.

**Traps:**
- Every `tapectl ...` text in `src/` is checked by `tests/refusal_recipes.rs` and
  `tests/runtime_text.rs`: each must name a real command and real flags.
- A remedy must be tried, not read: for each changed text add a test that runs the command
  the text names in the state the text is printed in, and shows it does not refuse.
- `cartridge erase`'s help says "nothing to erase, and the command succeeds". If that stops
  being true in a case, the help changes with it, and the cartridge pages are regenerated.
- T2.1 may hand you one text for `finish_confirm`. Place it as given.

**Done when:** the three issues' code criteria are met with tests that fail on the parent;
the `--resume` loop has a test; the full suite passes.

### T2.3 What is pending and what is seen (#529, #534, #539)

**Read:** `CONTEXT.md` (*Pending*, *Dirty*, *Collection*, *Unit*); ADR-0012 items 213, 225,
247, 249; `src/cli/audit.rs`'s `CHECKS` table and its doc.

1. **#529.** A unit whose newest snapshot was never staged is pending, with a reason of its
   own beside `New` and `Dirty`, in `audit`, `status`, `report` and `collection run`'s
   selection. `latest_snapshot` has four callers; change what each needs and say in the
   report what each now sees.
2. **#539.** `snapshot create --all-pending` exits non-zero when it skipped a unit it could
   not scan, and does not say "nothing pending" after a skip. `status` shows tape-only units
   where the issue says it leaves them out, and suggests only a `stage create` that will
   run.
3. **#534.** `audit` gets a check, scoped to the archive, that warns of a folder under a
   collection root that is not a unit; `status` shows the count. It reuses
   `collection::sync`'s discovery (make `candidate_unit_dirs` reachable); it does not walk
   the tree a second way.

**Traps:**
- A new pending reason makes four `match` sites non-exhaustive (`collection/status.rs`,
  `collection/sync.rs`, `for_location.rs`, `cli/unit.rs`). They are in your fence for that.
- A new `audit` warning changes exit codes. `tests/audit_exit_codes.rs` is yours;
  `scripts/lifecycle-suite.sh` lists the checks it allows and is **not** yours: report the
  new check's name so the coordinator adds it.
- `tests/cli_smoke.rs` pins "nothing pending" as the first line of stdout, and
  `src/cli/status.rs` pins `tapectl stage create a b`. Those pins move with the ruling; say
  which and why in the commit.
- The new check costs a directory walk for each collection. Add its budget to
  `tests/catalog_budget.rs`'s `Audit` row only with the coordinator's word; report the
  figure.
- If the new state needs a word in `CONTEXT.md`, propose it in the report. Do not edit the
  file.
- `src/cli/operations.rs` is in your fence for `unit_mark_tape_only` only.

**Done when:** each issue's criteria are met with tests that fail on the parent; `audit`'s
exit codes are stated for the new check; the status and snapshot pages are regenerated.

### T2.4 The catalog's door, `init`'s overwrite, and a delete under a write (#530, #538, #540, #544 part 2)

**Read:** ADR-0005 (escrow), ADR-0008; ADR-0012 item 249; `docs/keys-and-recovery.md`
runbook D; `src/staging/release.rs`'s doc (the gate in front of `clean_staging`).

1. **#530.** `db import` can run when the catalog will not open; it checks what it imports
   (an integrity check, and that it is a tapectl catalog) and keeps the catalog it replaces
   as a dated file; `db fsck --repair` refuses after a failed integrity check.
2. **#538.** `init` and `setup` never overwrite an existing `config.toml` when `tapectl.db`
   is absent: they refuse and name the file, or keep it, as the issue's criteria say. The
   one change is in `setup::make_home`, which both use.
3. **#540.** `snapshot delete --force` refuses while a write session holds the staged
   slices, live or resumable. `clean_staging` re-checks inside its transaction.
4. **#544, part 2.** The Tier 3 refusal's remedy in `src/cli/operations.rs` names a command
   that is not itself refused.

**Where the issues' own texts are wrong (checked in the code):**
- #530 says the door works when the file is lost. It does not: `main.rs` stops at "not
  initialized" before any open when `tapectl.db` is absent, and the config is loaded first.
  The door must be made for both cases, a file that is missing and one that will not open.
- #540 says "the same refusal as `stage clean --force`". That refusal covers only a live
  session and a pending rewrite's hold. It does **not** refuse an interrupted or aborted
  write with no live process. #540 needs the wider one: a resumable write is refused too.

**Traps:**
- `db import` replaces the catalog. The copy of what it replaces is made and synced before
  the replacement, and the test kills the process between the two.
- `src/cli/snapshot.rs` has a dry-run arm that repeats `snapshot delete`'s refusals. Both
  must change together, and `tests/dry_run_contract.rs` and `tests/dry_run_global.rs` hold
  them to one verdict.
- T2.3 changed `src/cli/operations.rs`, `src/cli/snapshot.rs`, `src/cli/mod.rs` and
  `tests/cli_smoke.rs` in wave 1. Your worktree is cut after that wave lands; check the base.

**Done when:** each issue's criteria are met with tests that fail on the parent; the two
runbooks say what the commands now do; the db, init, setup and snapshot pages are
regenerated.

### T2.5 `catalog rebuild` and a unit of the same name (#527)

**Read:** `src/volume/rebuild.rs`'s module doc; ADR-0012 item 249 and the amendment of
2026-09-29 (later) ("`catalog rebuild` never changes a status"); `CONTEXT.md` (*Copy*,
*Unit*).

`catalog rebuild` refuses, by name, a tape's unit whose name matches a catalog unit under
another uuid, and counts no copy for it. The supplement maps are keyed by what identifies a
staging (the MANIFEST block's `stage_set_id`, with the unit's uuid), never by name alone.
The report and the JSON say which units were refused and why.

**Traps:**
- This is a high finding on a consent path: a tape wrongly counted as a copy lets a release
  of staging go ahead. The test must show the copy count, before and after.
- "Refuse" means that unit of that tape. The rest of the tape still rebuilds.
- `src/volume/manifest.rs` is read, never edited: its bytes are pinned.

**Done when:** #527's criteria are met; a test that fails on the parent shows the unit
refused and no copy counted; the full suite passes.

### T2.6 The escrow key is parsed, and a release is never replaced (#533, #546)

**Read:** ADR-0005; ADR-0012 item 268 ("never a re-pushed tag"); `.github/workflows/release.yml`.

1. **#533.** An escrow public key is parsed as an age recipient before it is stored
   (`read_or_parse_public_key`, `read_public_key`). The change is in `src/crypto/keys.rs`
   alone, which covers `init --escrow-public-key`, `key import --escrow` and `setup`.
2. **#546.** `release.yml` uploads without `--clobber` and fails when the release already
   has the asset. `docs/development.md` gets the rule: a tag is never pushed twice; a fix is
   a new release.

**Traps:**
- Test fixtures store fake recipients (`age1fake...`) straight into the database. They do
  not pass through the parser and need no change. If a test does pass a fake key through
  the parser, give it a real generated recipient.
- The tests that pass an escrow key through `init`, `setup` or `key import`
  (`tests/setup.rs`, `tests/cli_smoke.rs`, `tests/adopt_key.rs`, `tests/first_run_adopt.rs`)
  were checked on `dc7e7605`: each passes a generated recipient, so none of them breaks.
- Do not run the workflow. Do not create a tag. The change is read, not executed.

**Done when:** a string that starts `age1` and is not a recipient is refused at all three
entries, shown by tests; the workflow change is one reviewed hunk.

### T2.7 The command line gives reporting tools what they need (#535)

**Depends:** T2.3 and T2.4 landed. **Read:** ADR-0012 item 250 and the reading under it
("what sufficient means"); `docs/operator-guide.md` "Scheduling the advisory half".

A tool learns, from the command line alone, in JSON and by exit code: the audit's verdict;
the age of the newest catalog backup and whether it passed its check; the drive's last
health reading; what is pending. Find which of the four the command line already gives
(`audit --json`, `status --json`, `report`, `drive status --json`) and add only what is
missing. Document the four in the operator guide as the contract for such a tool, with the
exit codes.

**Traps:**
- **Nothing is built in**: no address, no mail, no `OnFailure=`, no new unit, no change to
  `contrib/systemd/` or `scripts/`. The optional ping the scheduled wrappers carry is left
  as it is. The issue's older text about a channel is overtaken by item 250.
- JSON shapes that scripts read are pinned by tests. Add keys; do not rename or remove one.

**Done when:** each of the four facts is shown obtainable by a test that runs the command
and reads the JSON and the exit code; the guide's section exists and passes `check-docs.py`.
---

## S3. Stage 3: compatibility out (item 258)

**Starts when:** the CTO's go is given and Stage 2 is complete on `master`; the coordinator
then cuts `layout-3`. **Lands on:** `layout-3`. **Normative:** the Layout 2 set, with ADR-0012
items 242 and 258.

**What this stage is.** The code still writes Layout 2 when Stage 3 ends. What goes is every
path that exists to serve an older tapectl: older catalogs, older tapes, older spellings,
older config keys. **Stage 3 changes no byte that a tape carries.** If a removal would change
generated on-tape text (File 0, File 1, RESTORE.sh, RECOVERY.md, MANIFEST.toml, the front
index, the seal, the on-tape `catalog.db`), leave it and report it: it belongs to Stage 4.
The golden pins must pass untouched at the end of every Stage 3 task.

**Waves.** 3A: T3.1 with T3.2. 3B: T3.3 with T3.4. Then the coordinator runs the whole tape
chain on `layout-3` (it is still a Layout 2 tree and must be green) before Stage 4 starts.

The inventory behind these fences was taken on `dc7e7605`. Line numbers are leads.

### T3.1 One baseline schema, and a 1.x catalog refused by name

**Owner:** worker. **Depends:** Stage 2 landed. **Carries out:** item 258(b), (c); D9
(ratified, item 290: how a 2.x catalog is known and how a 1.x one is refused).
**Read:** ADR-0012 item 258; this plan's D9; `src/db/mod.rs` from `fn migrations()` to
`schema_is_current`; `vendor/rusqlite_migration/VENDORED.md`.

**Fence:** `src/db/mod.rs`, `src/db/migrations/*`, `src/db/export.rs`, `src/db/files.rs`,
`src/error.rs` (the `Migration` and `DatabaseNeedsRepair` texts only), `src/cli/db.rs`; the
migration tests in `src/tape/drive_identity.rs`, `src/tape/contact.rs`,
`src/tape/mam_journal.rs`, `src/tape/log_pages.rs`, `src/tape/health.rs`,
`src/tape/read_errors.rs`, `src/volume/restore_record.rs`; the two controls that open an
older schema in `src/policy/coverage.rs` and `src/staging/mod.rs`; the two issue-#233 tests
in `tests/cli_smoke.rs` and `tests/dry_run_global.rs`; `src/cli/audit.rs` (the legacy NULL
`completed_at` branch only).

**Steps:**
1. Produce `src/db/migrations/001_baseline.sql`: the schema that migrations 001 to 041 (and
   any Stage 2 added) leave behind, written as plain `CREATE` statements. Make it from a
   database the current code has migrated (`sqlite3 <db> .schema`), then order and comment it
   by hand. Drop the relic row `meta.schema_version`. Set `PRAGMA application_id` to the
   constant D9 names.
2. **Prove the baseline is the old schema, before deleting anything.** Add a test, in this
   commit only, that opens one database through the 41 migrations and one through the
   baseline and compares `sqlite_master` (type, name, tbl_name, and the SQL with whitespace
   normalised) and every table's `PRAGMA table_info`, `foreign_key_list` and `index_list`.
   It must pass. Report its output. Then, in the next commit, delete the 41 files and that
   test together.
3. `fn migrations()` holds the one baseline. Remove `VACUUM_AFTER_VERSIONS`,
   `warn_030_nulled_mtimes`, `open_memory_at_version` and every `open_memory_at_NNN` and
   `seed_schema_NN` helper. Keep `migrate_to`'s error mapping, `open_for_repair` and
   `schema_is_current`: later 2.x migrations need them.
4. The refusal. Opening a database that has tables and does not carry the application id
   fails with a `TapectlError` whose text says: this catalog was made by tapectl 1.x; tapectl
   2.0.0 does not upgrade it (ADR-0012 item 258); start a new catalog, or use tapectl 1.9.2
   for this one. Exit code as other "cannot open the catalog" errors. `db import` of a 1.x
   backup is refused the same way.
5. Delete every test that migrates from an older level or asserts on a migration number
   (the inventory lists about seventy in `src/db/mod.rs` and nine elsewhere). Keep
   `every_migration_file_is_named_for_its_position`, which now sees one file.
6. The two issue-#233 tests build an unfinished database from migrations 001 and 002. Give
   them a test-only second migration appended to the baseline (a `#[doc(hidden)]` or
   `cfg(test)` seam that takes extra `M` values) so that "has not finished migrating" and
   the repair door are still proven.
7. The two query-plan controls that used `open_memory_at_version(26)` get their control
   another way (drop the index inside the test), so each still shows it can fail.
8. Remove catalog-side tolerance for rows only an older tapectl wrote: the "before migration
   030" mtime handling in `src/db/files.rs` that no baseline row can need, the removed
   `REASON_MAM_NOT_ATTEMPTED` string in `src/tape/contact.rs`, the legacy NULL
   `completed_at` in `src/cli/audit.rs`, the "recorded before the fix" marks in
   `src/tape/read_errors.rs`. For each, confirm first that no code path of the current
   release can still write such a row. If one can, leave it and say so.

**Traps:**
- `catalog rebuild` reads mtimes from a tape's text. Do not remove a conversion that the
  rebuild of a tape written by the *current* code still uses (`src/db/files.rs:161,180`).
- `src/db/ontape_catalog.rs` copies the catalog's `user_version` into the on-tape
  `catalog.db`. That file is T3.2's. Do not edit it; tell the coordinator the new value.
- `scripts/first-run.sh` and `scripts/upgrade-mhvtl.sh` compute `MIG_TOP` from the file
  names. They are T3.4's.
- No new table and no renamed column here. The dar-named columns are renamed in Stage 4,
  where the baseline is edited again (D9).

**Verify:** the full suite; `db fsck` on a fresh home; a hand-made 1.9.2 catalog (make one
with the binary built from `v1.9.2`'s schema files, or keep one from step 2) is refused with
the text of step 4.
**Done when:** one migration file exists; the equivalence test's passing output is in the
report; a 1.x catalog is refused by name; no test refers to a migration number above 1.

### T3.2 One on-tape `catalog.db`, and no reader's tolerance for an older tape

**Owner:** worker. **Depends:** Stage 2 landed. **Carries out:** item 258(d).
**Read:** `docs/design/volume-format-v2.md` §1.4; the module doc of
`src/db/ontape_catalog.rs`; `src/volume/rebuild.rs`'s module doc.

**Fence:** `src/db/ontape_catalog.rs`, `src/db/catalog_snapshot.rs`,
`src/volume/rebuild.rs`, `src/volume/manifest.rs`, `src/volume/envelope.rs`,
`src/volume/format.rs` (readers and tests only), `src/volume/binding.rs`,
`src/volume/index.rs`, `src/volume/layout_model.rs`, `src/store.rs` (tests only),
`tests/catalog_rebuild.rs`, `tests/fixtures/ontape_catalog/`, `tests/heir_restore_sh.rs`
(only the tests of the 1.5.0 and 1.6.0 script fixtures and the `Era` variants for older
tapes), `tests/fixtures/restore_sh_1_5_0/`, `tests/fixtures/restore_sh_1_6_0/`.

**Steps:**
1. `ontape_catalog`: one generation, the one the current writer makes. Remove `Generation`'s
   older variants, `detect_generation`'s older shapes, `LEGACY_FILES_OF_SNAPSHOT`,
   `LEGACY_SPANS`, the older row type, and `fixture::downgrade`. A `catalog.db` of any other
   shape is refused, naming what was found. Remove `catalog_snapshot.rs` if it is only the
   re-export it says it is, and point its two users at `ontape_catalog`.
2. `rebuild`: remove the branches for a tape with no `catalog.db`, with no tenant ownership,
   with no `[media]` table, with no index, and "written by an older tapectl". A tape that
   lacks what the current writer always writes is refused, naming what is missing.
3. Readers: `Manifest` reads on past a key it does not know (`volume-format-v3.md` §1.2
   rule 3, #134). *(Corrected 2026-11-17: this step first asked for
   `deny_unknown_fields`, against rule 3.)*
   File 0 without `[media]` or without `cartridge_identity_source`, `requires` absent, a
   `paths.tsv` with no sha256 column, a `layout.json` sidecar from before 1.7.0: each is
   refused, naming the field, where it was tolerated.
4. Delete the tests that read an older shape (the inventory lists them by name in
   `ontape_catalog.rs`, `tests/catalog_rebuild.rs`, `format.rs`, `store.rs`, `erase.rs`,
   `manifest.rs`, `envelope.rs`, `index.rs`), the two committed `catalog.db` fixtures, the
   two old-script fixtures and their four tests, and the `Era` rewrites for 1.5.0 and 1.6.0.
   **For every deleted test, check that the current shape has a test of the same behaviour.**
   Where it does not, convert the test instead of deleting it, and list the conversions.

**Traps:**
- `src/volume/layout.rs` and `src/volume/restore_script.rs` are **outside** this fence.
  RESTORE.sh's own handling of older tapes is generated on-tape text. It leaves with the
  script in T4.10.
- The seal rule "a seal with no `ended_after` is a full seal" (rule S3) is Layout 2's and is
  what the current writer makes for a full seal. It stays. Stage 1 restates it for Layout 3.
- `src/volume/erase.rs`'s `File0Report` reads the label of any tapectl tape. It stays: T4.7
  builds on it.
- `src/cli/catalog.rs` holds one note about a tape with no `catalog.db`. It is T3.3's file.
  Report the line; do not edit it.

**Verify:** the full suite; the golden pins untouched and passing.
**Done when:** `grep -rn 'Generation::\|LEGACY_\|downgrade' src/db tests` finds nothing;
the report lists every deleted test beside the test that still covers its behaviour.

### T3.3 The renamed spellings and the old config keys

**Owner:** worker. **Depends:** T3.2 landed (for `src/cli/catalog.rs`). **Carries out:**
item 258(d). **Read:** `src/cli/renamed.rs`'s module doc; `docs/configuration.md` "Removed
and renamed keys".

**Fence:** `src/cli/renamed.rs` (deleted), `src/main.rs`, `src/cli/mod.rs`,
`src/cli/unit.rs`, `src/cli/catalog.rs`, `src/cli/report.rs`, `src/cli/cartridge.rs`,
`src/cli/config.rs`, `src/config.rs`, `src/config_edit.rs`, `src/policy/unknown_keys.rs`,
`tests/renamed_spellings.rs` (deleted), `tests/frozen_spellings.rs`,
`tests/dry_run_contract.rs`, `tests/json_errors.rs`, `tests/refusal_recipes.rs`,
`tests/man_matches_help.rs`, `docs/configuration.md`, `docs/man/*`, `docs/cli/*`.

**Steps:**
1. Delete the `RENAMED` table, its two hooks in `src/main.rs`, and every hidden stub
   command and flag. A removed spelling is then an ordinary clap error. Remove
   `Verdict::Renamed` and its rows from `tests/dry_run_contract.rs`; every clap leaf still
   has exactly one verdict.
2. Delete `stale_lto_fields_message` and `RENAMED_DEFAULTS_KEYS` with their hint in
   `policy::unknown_keys`. An old key is an unknown key, and unknown keys are already
   errors everywhere (ADR-0012, 2026-09-14). Confirm that with one test per section that
   had a named refusal: the old key is still **refused**, now as unknown.
3. Delete the `receipts/` to `stage-reports/` move, the acceptance of the empty
   `collections = []` an older `init` wrote, and `cartridge list --status offsite`'s named
   refusal.
4. `docs/configuration.md`: remove the table of renamed keys. Regenerate `docs/man` and
   `docs/cli`. Run `scripts/check-docs.py`.

**Traps:**
- The three File 1 commands keep their spellings (item 221). `tests/frozen_spellings.rs`
  stays, without its `renamed::` calls.
- "Refused by name" becomes "refused as unknown". Never "accepted". A test that shows an
  old key loading without error is a defect in this task.
- Other docs name old spellings in upgrade notes (`docs/install.md`, `docs/releases/*`).
  The release notes are dated records and are not edited. `docs/install.md` is T3.4's.

**Verify:** the full suite; `check-docs.py`; `tapectl backend add` and
`tapectl db backup --to X` each exit 2 with clap's own message.
**Done when:** `grep -rn 'renamed::\|RENAMED' src tests` finds nothing; the old config keys
are refused as unknown, shown by tests.

### T3.4 The upgrade tool and the scripts' one-time moves

**Owner:** worker edits; **the coordinator proves** (a `scripts/` change is proven only by
the whole chain). **Depends:** T3.1. **Carries out:** item 258(c); #562 "The plan".
**Read:** `docs/install.md` §10; `scripts/first-run.sh` step 0 and the `--upgrade` block;
`scripts/upgrade-mhvtl.sh`'s header.

**Fence:** `scripts/first-run.sh`, `scripts/upgrade-mhvtl.sh`, `docs/install.md`,
`contrib/hosts/vm-desk1-mhvtl.profile`, `tests/first_run_*.rs` where they pin a removed
text.

**Steps:**
1. `first-run.sh` step 0: remove the offers to rename `min_*_for_tape_only` and
   `usable_capacity_factor` and the check for a leftover `~/.tapectl` beside the new home.
2. `--upgrade`: remove the switches for releases before 2.0.0 (the `db backup --to`
   spelling, the version-pair notes). It refuses, by name, to upgrade an installed tapectl
   below 2.0.0: there is no upgrade from 1.x (item 258(c)), and the way is a fresh start.
   `MIG_TOP` still comes from the migration file names.
3. `upgrade-mhvtl.sh`: it needs a 2.x tag to start from and there is none yet. It exits with
   a clear message when the newest tag before HEAD is below `v2.0.0`. Its first real run is
   the release after 2.0.0. Say so in its header and in `docs/install.md` §10.
4. `docs/install.md`: §10 says upgrades start from 2.0.0; the notes on 1.x version pairs,
   the renamed `[defaults]` keys, the `receipts/` rename and the older home go.

**Traps:**
- Do not run any of these scripts. They install into a home and drive a tape.
- `first-run.sh`'s texts are pinned by `tests/first_run_refusal_phrases.rs` and others.
  Change a pin only for a text this task removes, and list each.

**Done when:** `bash -n` passes on both scripts; the full suite passes; the report lists
what the coordinator must run (`first-run.sh`'s rehearsal on mhvtl) to prove it.
---

## S4. Stage 4: the data path on the tar payload

**Starts when:** Stage 1's documents are landed (D1 to D22 are ratified, item 290), Stage 3 is on
`layout-3`, and T0.3's leg of made files has passed (item 291 moved its other leg to T6.6). **Lands on:** `layout-3`.
**Normative:** `docs/design/volume-format-v3.md` and the session document as Stage 1 leaves
them. Each reference below to the format document names its section of
`docs/design/volume-format-v3.md` (T1.7). Where a section marks a point **OPEN (Stage 1)**,
stop: it is the CTO's.

**What the code does today, in one paragraph** (so that a worker knows what it is
replacing). `stage create` runs `dar -c -`, hashes every source file itself beside dar's
read, cuts dar's output into dar slices (a header, a payload, a trailer byte), encrypts each
with age in memory and writes `.dar.age` files. A dar catalogue is isolated beside it and
goes into the envelopes. Every restore decrypts slices and feeds dar: one stream into
`dar -x -`, or slices spooled to disk. The heir's script, about five thousand lines of shell
held in Rust strings in `src/volume/layout.rs`, does the same with a FIFO. All of that goes.

**Order and waves** (at most three workers at once; a later wave starts when the one before
is integrated):

| Wave | Tasks | Why this order |
|---|---|---|
| 4.0 | T4.0 (coordinator) | the branch is made ready |
| A | T4.1, T4.3 | new modules, no callers; both file-disjoint |
| B | T4.2, T4.4 | T4.2 needs T4.1's writer for its tests; T4.4 edits the baseline after T4.3 |
| C | T4.5 | the switch: from here some tests are pending |
| D | T4.6 | what a tape carries; one worker at a time on the generators |
| E | T4.8, T4.9, T4.10 | readers of what T4.6 defined; file-disjoint |
| F | T4.7, T4.11 | |
| G | T4.12 | |
| H | T4.13, T4.14, T4.17 | T4.17 edits the heir's script after T4.12 and is file-disjoint from T4.13 and T4.14 |
| I | T4.15 | dar leaves |
| J | T4.16 (coordinator) | the gate and the suites on Layout 3; the tape chain is green again |

### T4.0 The branch is made ready

**Owner:** coordinator.
1. Merge `master` (with Stage 1's documents) into `layout-3`.
2. Remove the Layout 2 pins from `tests/on_tape_golden.rs`, **each in a commit of its own**
   naming ADR-0012 item 243: the script's sha256, the MANIFEST, the range block, the short
   seal, and the two tests that parse the pinned short seal. The module's history stays,
   with one line that Layout 3's pins come in T6.3.
3. Add `acl` (for `setfacl` and `getfacl`) to what `.github/workflows/ci.yml` installs, and
   to `tests/test_dependencies.rs`'s named requirements, beside dar for now.
4. Confirm the runner's tar meets the floor: `tar --version` in a CI step, kept as a step.
5. Add the decompressor of each algorithm carried (item 285: gzip, xz, zstd) to what `ci.yml` installs and
   to `tests/test_dependencies.rs`'s named requirements: T4.5's and T4.8's tests read
   library-made frames back with the real tools.

### T4.1 The writer: GNU tar run with the format's exact command line, and the floor

**Owner:** worker. **Depends:** T4.0. **Carries out:** items 253, 270, 273, 274; #563's
early refusal; #562 "The writer"; D2. **Read:** `volume-format-v3.md` §3.1 and §3.2
(the literal command line is §3.1's: D2's, with the flag T0.1 picked for the ACL record);
`src/dar/mod.rs` `command` and
`src/dar/create.rs` `spawn_archive`, as the shape to follow, not to copy; #563.

**Fence:** new `src/tar/mod.rs`, `src/tar/writer.rs`, `src/tar/probe.rs`; `src/lib.rs` (one
`pub mod tar;` line); `src/error.rs` (new variants only).

**Build:**
1. `tar::command(binary)`: a `Command` with the parent-death signal as `dar::command` sets
   it, `LC_ALL=C`, and **`TAR_OPTIONS` and `POSIXLY_CORRECT` removed** from the child's
   environment.
2. `write_list(dir, entries) -> PathBuf`: the file list. NUL-separated, every name prefixed
   `./`, in the order given, mode 0600, in the directory the caller names (the caller gives
   the home's work directory: file names are plaintext and never touch the staging device).
3. `CreateParams { binary, root, list, xattrs, acls }` and `create_command(&params)`: the
   argument vector, exactly `volume-format-v3.md` §3.1's, in its order. `recorded_command` is the
   same line as text for the catalog.
4. `spawn_archive(&params) -> TarStream`: stdin null, stdout piped, stderr drained on a
   thread into a bounded buffer. `TarStream::finish() -> TarFinish`: exit 0 is `Clean`;
   exit 1 is `FilesChanged` (tar says a file changed as it was read); exit 2 or a signal is
   `Failed`, each with the captured stderr. **Exit 1 and exit 2 both refuse a staging**
   (invariant 7); this function reports, the caller refuses.
5. `probe(binary) -> TarCheck`: resolves the binary as `dar`'s check does (a bare name on
   `PATH`; never a hard-coded `/usr/bin/tar`), runs `--version`, and reports: the path; the
   first line; whether it is GNU tar (the line starts `tar (GNU tar) `); the version; whether
   it meets `FLOOR` (1, 35); and ACL support as `Yes`, `No` or `Unknown(reason)`.
   ACL support is found by doing it: in a temp directory the caller names, make a file, give
   it an ACL entry for a numeric id, archive it with the writer's flags, and read the
   member's ACL record. A numeric entry is `Yes`. No ACL record is `No`. A file system that
   refuses the ACL is `Unknown`. `TarCheck::refusal() -> Option<String>` gives the text for
   not GNU tar, below the floor, and `No`; it names the version found and the floor.

**Traps:**
- No test-only helper that makes a staged unit or slices from tar. Later tasks stage through
  the real path (invariant 8).
- No `--sparse`, no compression flag, no `--one-file-system`, no `--ignore-failed-read`.
  If `volume-format-v3.md` §3.1's command line and D2 differ, the document wins. Compression is
  never tar's: it is applied to each slice after the cut (rule 11).
- Run tar in the unit's root. If `--directory` is used it comes before `--files-from`:
  after the list, tar reads the names against the wrong directory and exits 2 (measured).
- A file named like an option must be archived as a member, never acted on: that is what
  `--null --verbatim-files-from` and the `./` prefix are for. Test it with files named
  `--checkpoint=1`, `-T`, `--files-from=x` and a name with a newline in it.
- Version strings to parse in tests: `tar (GNU tar) 1.30`, `1.34`, `1.35`, a distribution's
  suffix after the version, `bsdtar 3.6.2 - libarchive ...`, `tar (busybox) 1.36.1`.

**Verify:** unit tests for the argument vector (compared whole, as a list), the environment,
the list's bytes, the version table, and the exit mapping with the real tar on a tiny tree
(a listed name that does not exist gives `Failed`); the floor refusal with a stand-in script
that prints `tar (GNU tar) 1.34`; the probe against the host's real tar says `Yes`.
**Done when:** the module has no caller outside its tests, the suite is green, and the
report gives the probe's output on this host.

### T4.2 The stream reader: members, hashes, ids, and the stream's checks

**Owner:** worker. **Depends:** T4.1. **Carries out:** items 253, 254, 256, 270, 272, 274,
277, 283 (the cutter's input); #562 "The stream reader"; #563's stream check; D3, D12;
invariants 5, 7 and 13.
**Read:** `volume-format-v3.md` §3.2 (the stream's checks) and §1.6 (the file table); the research note
(`docs/research/2026-11-16-layout-3-before-the-build.md`) §5 and §6; #563's tables.

**Fence:** new `src/tar/stream.rs`, `src/tar/pax.rs`; `src/tar/mod.rs` (exports only).

**Build:** a reader that is fed the stream's bytes in order, changes none of them, and
yields one row for each member. It reads 512-byte blocks itself. **Do not use the `tar`
crate's iterator**: it gives a member's own header, and a member starts at its first
extension header.

1. Input: the snapshot's list, in order (path, kind, size, and whether the file had an ACL
   on disk before tar ran), and the rules (whether ACLs are kept).
2. For each member it yields: kind, `start`, `end`, size, sha256, path, and for a hard link
   its target. `start` is the offset of the member's first extension header; `end` is the
   offset just past its last padded block. The size comes from the pax `size` record when
   there is one. Directories, links and device nodes have no data.
3. It hashes each regular file's bytes with `pass_count::Sha256` (never `sha2` directly).
4. It collects every user id and group id it sees, from the header and from ACL entries.
5. It refuses, naming the path and the rule (D12): a member that is not the next entry of
   the list, by path, kind or size; a header whose user or group name field is not empty; a
   `uname` or `gname` pax record; an ACL entry whose qualifier is not a number; a listed
   file that had an ACL on disk whose member carries no ACL record; a global extended
   header; a GNU long-name member; a header whose checksum is wrong; a type it does not
   know; anything but zeros after the end-of-archive blocks; a stream that ends inside a
   member; a listed entry never seen.
6. **A hard link.** The list has two regular files; the stream has the first with its data
   and the second as a link member with none. That is accepted when the link's target is a
   path listed earlier, and the row is kind `h` with the target's sha256 and the target's
   path.
7. **For the cutter** (D3, rule 13). Before any byte of a member passes on, the reader
   gives the member's `start` and its whole length: its extension headers, its header and
   its padded data. It therefore holds a member's header blocks back until the member's
   own header is read, and nothing more. The stream's tail is announced the same way.

**Traps:**
- A member's own header alone loses a long name, a nanosecond time and a size over 8 GiB.
  The test for this restores a long-named member from `start..end` alone, with real tar.
- Two zero blocks end the archive. tar then pads to its record size with more zeros. Both
  are the tail, belong to no member, and are still part of the stream that is cut.
- The reader must hold memory for one block and one member's pax records, never a member's
  data. A 96 GiB member was measured.
- ACL text has the form `user::rw-,user:1:r--,group::r--,mask::r--,other::r--`. An empty
  qualifier is the owner's own entry and is fine. A non-empty one must be all digits.

**Verify:** tests that make real streams with T4.1's writer from real files: long names
(over 100 and over 255 bytes), a nanosecond time, a symbolic link, a hard link pair, a FIFO,
an empty folder, a file with an ACL (`setfacl`), a file with a `user.` attribute. For every
stream: each row's `start` is the row before's `end`; the rows' ranges are the stream less
its tail; each sha256 equals `sha256sum` of the file; each member's announced length is
its row's `end` less its `start`, and is given before its first byte. The refusals, each from a real tar run
with the wrong flags put in front of the reader: without `--numeric-owner` (names); without
`--acls` over a file that has an ACL (an ACL lost); a list with a name tar skips. A stream
cut short. A gated test (`TAPECTL_PERF_TESTS=1`) for a member over 8 GiB.
**Done when:** no caller outside tests; every refusal in step 5 has a test that fails
without it; the suite is green.

### T4.3 The file table and the id map: schema, text, lookup

**Owner:** worker. **Depends:** T4.0, T3.1. **Carries out:** items 254, 264, 265, 272, 276,
277, 283; #557; #564; #567; D4, D5, D9, D13. **Read:** `volume-format-v3.md` §1.6 (the file table's and the
id map's text) and Appendix A.3; ADR-0012 items 264 and 265; `src/volume/index.rs` `escape` and `unescape`;
`src/db/files.rs`.

**Fence:** `src/db/migrations/001_baseline.sql`; new `src/db/stage_files.rs`,
`src/db/stage_ids.rs`; `src/db/mod.rs` (module lines only); new `src/volume/filetable.rs`,
`src/idmap.rs`; `src/volume/mod.rs`, `src/lib.rs` (module lines only).

**Build:**
1. **The baseline is edited in place; no migration 002** (D9). Add `stage_files` and
   `stage_ids` as `volume-format-v3.md` Appendix A.3 lists them. Add, to existing tables, the
   new columns D11 names (`stage_sets.tar_version`, `tar_command`, `stream_bytes`;
   `tar_create_reports`; `restores.tar_*`). **Do not drop a dar-named column or table
   here**: their users are other tasks' files. T4.15 drops them.
2. `db::stage_files`: insert a staging's rows inside the caller's transaction (prepared
   once, not one statement text for each row); read them back in stream order as an
   iterator; `exists(stage_set_id)`. `db::stage_ids`: the same for the map. Neither is
   deleted when a staging's slices are released; both go only with the staging's own row.
3. `volume::filetable`: write a staging's table as the format's text, streaming, from the
   database rows or from T4.2's rows; parse it, streaming, refusing a bad header, a row
   whose `start` is not the row before's `end`, a `slice` lower than the row before's, an
   unknown kind, a bad hash, a bad escape.
   One escaping rule for the whole format: call `volume::index::escape`; do not write a
   second one.
4. `filetable::ranges(rows, wanted) -> Vec<Range>`: the byte ranges that hold the wanted
   paths' members, **joined at member boundaries** (two wanted members that touch become one
   range). A wanted hard link brings its target's member, and the target comes first. A
   wanted folder is its own member, which does not lie beside its contents. `whole(rows)`
   is one range. `slices_of(rows)` gives the slices the given members need: each one's
   `slice`, and for a member larger than `slice_size` the slices of its run (D3).
   `slice_start(rows, n)` gives a slice's offset in the stream, so that a range becomes an
   offset inside a slice. `held_whole(rows, slices_present)` gives the members whose every
   slice is present (items 260, 266), and `members_of(rows, n)` the members a lost slice
   costs.
5. `idmap`: look a user id and a group id up through the C library; write and parse the
   map's text. An id the host cannot name has an empty name. An id never seen has no row.

**Traps:**
- A million files is a million rows: one transaction, one prepared statement. The
  performance suite pins `stage create`'s statement count against the entry count; say what
  the table adds, and do not move the pin.
- `stage_files` refers to a path by `paths.id`; do not store path text twice.
- The lookup must never block a staging on a directory service that does not answer for
  long: look each distinct id up once.
- The id map's needles join the plaintext leak scan in T4.6, not here.

**Verify:** round-trip tests of both texts, with paths that hold a tab, a newline, a
backslash; `ranges` on the measured shape `d`, `d-x`, `d.txt`, `d/a` (a folder is two
ranges); a hard link asked alone brings its target; `held_whole` when one slice of a long
member's run is missing; `members_of` for a slice of many members and for one piece of a
run; the lookup of this process's own uid gives a name, and of an id far above any
real one gives none.
**Done when:** the suite is green; the report states the baseline's new tables and columns
as SQL.

### T4.4 The snapshot: what a pattern left out, an empty default, and no sockets

**Owner:** worker. **Depends:** T4.3 (the baseline). **Carries out:** items 252, 270;
#542; D14. **Read:** #542, whole; `src/staging/exclude.rs`; `walk_directory` in
`src/staging/mod.rs`; `CONTEXT.md` (*Snapshot*, *Exclusion*).

**Fence:** `src/staging/exclude.rs`; `src/staging/mod.rs` (`walk_directory`,
`snapshot_create_detailed_reading` and their tests only); `src/config.rs` (the default
exclusion list only); `src/cli/snapshot.rs` (output only); `src/db/migrations/001_baseline.sql`
and a new `src/db/snapshot_left_out.rs` (the table is determined under D9);
`src/main.rs` (`init`'s written config only).

**Build:**
1. `init` writes an empty exclusion list (item 252). A config that names patterns keeps
   them.
2. Every snapshot records and reports what each pattern left out: the pattern, the path, and
   whether it was a file or a folder with what was under it, as #542's criteria say.
3. Sockets are left out of every unit and reported the same way, with the reason "socket"
   (item 270): tar skips a socket at exit 0, so a listed socket would have no member.
   FIFOs and device nodes stay.
4. The walk keeps a pruned folder's own entry, empty, as today.

**Traps:**
- tapectl's matcher is now the only matcher. Do not build tar exclusion flags: tar is given
  the exact list. The functions that turn patterns into dar masks (`dar_masks`) are deleted
  in T4.15, not here: staging still calls them until T4.5.
- `src/staging/mod.rs` is T4.5's file in the next wave. Stay inside the two functions named.
- An existing test asserts the old defaults (`*.nfo`, `*.tmp`). It changes with the ruling:
  name it in the commit.

**Done when:** #542's criteria are met; a snapshot of a tree with a socket, an excluded
file and an excluded folder reports all three, shown by a test; the suite is green.

### T4.5 Staging on tar: the switch

**Owner:** worker (one, alone in its wave). **Depends:** T4.1 to T4.4, T0.5, T0.6, T0.7.
**Carries out:** items 241, 253, 254, 265, 270, 272, 274, 277, 280, 283 to 287;
#555; #562 "The writer"; #563; #564; #565; #567. **Read:** `volume-format-v3.md` §3 whole (the stream, the cut, a slice on tape, compression) and `layout-session.md` "Layout 3" (what a staging produces);
`src/staging/mod.rs` `stage_create_inner`, top to bottom; `src/staging/validate.rs` `plan`
and `recheck`; `src/staging/files.rs`.

**Fence:** `src/staging/*` (all of it); `src/cli/stage.rs`; `src/cli/config.rs`;
`src/db/migrations/001_baseline.sql` (`archive_sets.compression_level`,
`stage_sets.compression_level` and `stage_slices.continues` only: format Appendix A.2;
*added 2026-11-18*, T4.3's fence held only D11's columns);
`src/policy/depth_check.rs`; `src/config.rs` (`[tar]`, and the keys D11 removes or
redefines); `src/setup.rs` and `src/main.rs` (the tool checks only);
`tests/pass_budget.rs`, `tests/cli_smoke.rs`, `tests/test_dependencies.rs`, `tests/setup.rs`.

**Build, in `stage_create_inner`'s order:**
1. The early refusal: `tar::probe`, before anything is made.
2. The list is the snapshot's own entries in bytewise path order, never a new walk.
   `validate::plan` still compares the tree with the snapshot first, with today's rules for
   MISSING, DIRTY and NEW.
3. Before tar: for each listed entry record device, inode, size, mtime, ctime, and whether
   it has an ACL (the `system.posix_acl_access` or `system.posix_acl_default` attribute).
4. Spawn tar (T4.1). Pump its output through the stream reader (T4.2) into the cutter and
   on to `SliceWriter` as today (age in memory, both hashes, the file made durable). **The
   cutter follows D3**: it ends a slice at a member's start, gives a member larger than
   `slice_size` a run of slices to itself, and begins a slice nowhere else (rule 13). No
   header, no trailer. Each slice's row records whether it continues a member (D22).
   **Where the unit's resolved `compression` is not `none`**, each cut is compressed alone,
   by tapectl's library for that algorithm, as one frame, at the resolved
   `compression_level` or the algorithm's default (items 284 to 287; D15), between the
   cutter and encryption (rule 11). `size_bytes` and `sha256_plain` are taken from the cut
   **before** compression (D3). The staging's algorithm and level are recorded in
   `stage_sets`. **Every slice is decoded again and compared with its cut before it goes
   on** (D15: what was hashed is not what goes to tape); a difference refuses the staging.
5. After tar: `finish` must be `Clean`; the reader must have seen every entry; `recheck`
   compares each entry with step 3, and a file inside the racy window is hashed from disk
   and compared with the stream's hash, as today.
6. Each stream hash is compared with the version's recorded sha256 where there is one. A
   difference refuses the staging, for every path but the unit dotfile, as today
   (`validate.rs`, the check near line 1079). Hashes not yet recorded are filled in.
7. One transaction at the end, as today, now also holding the file table's rows, the id
   map's rows (each id looked up once), `tar_version`, `tar_command`, `stream_bytes`.
8. `config check`, `init` and `setup` name tar where they named dar, with the probe's
   refusal text. `[tar] binary` is read; `[dar]` is no longer read (T4.15 removes it).
9. `slice_size` must be a multiple of 512 wherever it is set (`[defaults]`, an archive set,
   a unit dotfile). Another value is refused, naming the key (#562).

**Removed here:** the slice template and `SliceFrame`'s use; the dar spawn and its report;
the seccomp filter's use (#441: tar makes no `fadvise` call); the source hasher beside the
archiver (`ConcurrentHash`, `ReadAhead`, `Lanes`, `DropBehindBoth`, `hash_threads`); the
catalogue's isolation and `CatalogueCheck` (#464's comparison is replaced by step 5's member
check); the exclusion masks handed to the archiver.

**Traps:**
- **No plaintext on the staging device** (#370). The file list holds file names: it is
  written in the home's work directory and removed. `staging/plaintext_audit.rs`'s test must
  still pass, unchanged.
- Slice files are named `<base>.<N>.tar.age`. `staging/clean.rs` finds leftovers by suffix:
  change both together.
- `check_staging_space` adds a figure for each entry's headers. **Determined:** 2,560
  bytes for each entry, plus twice the length of its path in bytes rounded up to 512, plus
  4,096 for an entry that has an ACL or an extended attribute, plus 1,024 for each slice
  the estimate comes to (measured: about 1,951 bytes of stream for a small file, more with
  a long name or an ACL). It is an estimate made before tar runs; the space accounting
  while the stage runs still catches a stream that outgrows it, as today. It never counts
  on compression to make room: bytes that do not shrink grow a little.
- Compression must not hold a slice's plaintext on disk, and must not hold more than one
  slice in memory for each unit being staged.
- **Determined:** the check that decodes each packed slice (D15) compares the sha256 of
  the decoded bytes with the cut's `sha256_plain`, both taken as the bytes pass. Neither
  the cut nor the decoded slice is held whole for it.
- The cutter decides at a member's start, from the length the reader gives it. It never
  looks ahead in the stream and never holds a member's data. A member of exactly
  `slice_size` fits in an empty slice; one block more is a run.
- A slice whose on-tape size would pass the stated bound (D3) refuses the staging by name.
  It is not stored uncompressed in place: a staging has one algorithm.
- `[staging] jobs` stages several units at once: each has its own tar, list and reader.
- A stop (Ctrl-C, a kill) must end tar and leave no slice file the catalog does not know,
  as the dar path does today (`after_failed_stage`, `cleanup_failed_stage_set`).
- `tests/pass_budget.rs` pins the bytes hashed by `stage create`. The layers are now the
  files' bytes (from the stream), the slices' plaintext and the ciphertext. Report the new
  figure. **Do not change the pin.** The coordinator moves it, saying why.
- **Pending tests.** From this commit every test that stages and then restores, or builds a
  volume from a staging, fails until T4.6 to T4.9. Mark each
  `#[ignore = "L3-pending T4.<n>: ..."]` with the task that will carry it, and list them in
  the report with their count for each tag. A test of staging itself is yours to make pass.

**Verify:** for the cut, through `stage create`: every slice that does not continue a
member is read alone by the real tar at exit 0, and gives exactly the files the table
says begin in it; a long member's run, joined, is read alone; the slices joined are the
stream byte for byte; a member of exactly `slice_size`, and one a block larger; the tail
after a run. For each algorithm carried: a unit of several slices that holds a file larger
than a slice, each slice read back with the host's real decompressor to the cut's
`sha256_plain`; a unit of bytes that do not shrink; a level set, and a level out of range
refused; and a unit with `compression` unset beside it. Then, tests through `stage create`
itself:
a unit with a hard link pair, a symbolic
link, a FIFO, an ACL naming an id the host knows, a `user.` attribute, a long name; the
file table's rows match `sha256sum` of every file and their ranges restore each file with
real tar; the id map holds the owners, the groups and the ACL's id; a file replaced while
tar reads it refuses the staging (use a test hook, not a race); a stand-in tar below the
floor, one that writes names, and one that writes no ACL record each refuse the staging by
name.
**Done when:** `stage create`, `quick-archive` and `collection run` stage on tar; nothing
under `src/staging/` names dar; the pending list is in the report; the suite is green.

### T4.5b A staging job's memory counts its compressor (#570)

Added 2026-11-18 at T4.5's integration. **Owner:** worker. **Depends on:** T4.5. **Runs
beside** T4.6 (disjoint files).

**Build.** `staging::jobs::effective_jobs` admits jobs at a flat `MEMORY_PER_JOB` (192 MiB),
which does not count the compressor's own working memory: xz at level 9 takes about 674 MiB
to pack and 65 MiB to decode again (the decode-and-compare check runs beside the packer).
Give each job its own figure, from its resolved compression and level: 192 MiB plus the
algorithm's pack and decode memory at that level, read from a table in `staging::pack`
beside the level ranges (gzip, xz and zstd, each level; the figures from each library's
own documentation, cited in the table's comment, never guessed). `effective_jobs` judges
the queue by the jobs' own figures, and its notice names the compression and level when
it lowers the count.

**Fence:** `src/staging/jobs.rs`, `src/staging/pack.rs` (the table only).

**Done when:** a test shows that the memory which holds three jobs at `none` holds fewer at
xz 9, and the notice names the level; each figure in the table cites its source; the suite
is green.

**For T4.6 to T4.10 (binding, found at T4.5's integration).** T4.5 needed only two
`L3-pending` marks, because most restore, rebuild, heir and write tests build their
archives by hand (`dar::create`, fixture stage sets), and the write path accepts a NULL
`catalog_path`. Those tests will not go red when their path is ported wrongly. Each of
T4.6 to T4.10 therefore drives its own tests from a **real staging** (`stage create` on a
made tree, as `src/staging/tar_stage_tests.rs` does) and ports or deletes every test of its
path that builds a dar archive by hand; the report lists each.

### T4.6 What a tape carries: Layout 3's identity, the MANIFEST, the envelopes, both ways

**Owner:** worker (one, alone in its wave). **Depends:** T4.5. **Carries out:** items 254,
258(a), 265, 276, 283 and 288 (what a tape states); #528, #547, #557, #564; #562 "The
layout" and "The table"; D1, D4 to D8, D22.
**Read:** `volume-format-v3.md`, whole (this task implements most of it: §1, §2, §4 and §5);
`src/volume/build.rs` `materialize_envelope_streaming`; `src/volume/manifest.rs`;
`src/db/ontape_catalog.rs`'s module doc.

**Fence:** `src/volume/format.rs`, `src/volume/layout.rs` (File 0, the front index, the seal
and the MANIFEST generators only; **not** File 1, RESTORE.sh or RECOVERY.md's prose),
`src/volume/manifest.rs`, `src/volume/build.rs`, `src/volume/envelope.rs`,
`src/volume/index.rs`, `src/volume/write.rs`, `src/db/ontape_catalog.rs`, `src/store.rs`
(the claims check), `tests/format_v2.rs` (renamed `tests/format_v3.rs`),
`tests/tenant_isolation.rs`.

**Build:**
1. `LAYOUT_VERSION = 3`, the new magic, in File 0, the front index and the seal marker
   (D1). `requires` is empty; `short-seal` and `spanning` are no longer known entries.
   **Determined** (#562's rule, made exact): File 0's and the seal's `requires` are the
   empty list. `may_end_early = true` stays in File 3's `[index]` and in every
   `[manifest]`, on every Layout 3 tape. Rule S2 becomes "every seal carries
   `ended_after` and keeps S1", and S3 (a seal with no `ended_after`) goes: such a seal
   is unparseable under S6. §1.2's rule 6 is carried as it reads.
2. **#547:** File 0's `[media]` strings are escaped as TOML strings.
3. The MANIFEST's block for a staging as D6 has it, with `compression` stated only for a
   compressed staging, no `may_span`, and `continues` on a slice that continues a member
   (D22). `manifest.rs` stays one type in both directions and reads on past a key it does
   not know (§1.2 rule 3, #134). *(Corrected 2026-11-17: this step first asked for
   `deny_unknown_fields`, against rule 3.)*
4. Each tenant envelope carries, for each staging the tape's plan holds a slice of, that
   staging's whole file table and id map as text (D4, D5), from the catalog database. The
   operator envelope's `catalog.db` carries both as rows (D7). `catalogs/` is gone.
5. `envelope.rs` reads them back: `open_envelope` gives a caller the table and the map of a
   staging, streaming. T4.8 and T4.9 use this and do not edit `envelope.rs`.
6. **A write refuses, by name, a unit whose file table the catalog cannot supply** (item
   265, #557): before the tape moves, at validation. The message names the unit and says to
   stage it again.
7. `index/paths.tsv` states no sha256 for the unit dotfile (D8, #528).
8. The plaintext leak scan (`tests/tenant_isolation.rs`, `tests/format_v3.rs`) gains
   needles: a user name, a group name, and an id that the test unit alone carries.

**Traps:**
- **Invariant 3.** The table and the map are inside envelopes. No plaintext file gains a
  field. The front index still states positions, types, sizes and ciphertext hashes only.
- The envelopes are written before the slices, so on a tape that ends short the table lists
  files of slices after `ended_after`. That is the plan, and the seal withdraws it. Do not
  try to write the table after the slices.
- The planner keeps its two classes, and the catalog its `may_span` and its `cut` rows,
  until T5.1. This task stops stating `may_span` and `spanning` on tape and changes no
  plan.
- Envelopes are built by streaming. A table of a million rows is about 121 MB of text:
  never hold it in memory.
- File 2 on tape is still the old script until T4.10, and RECOVERY.md still speaks of dar
  until T4.13. Leave both generators' prose alone; remove only what no longer compiles.
- `tests/heir_restore_sh.rs` reads tapes the old script understands. From this commit it
  cannot pass. Gate the **whole file** with one `#![cfg(any())]` line under a comment
  `L3-pending T4.10`; T4.10 deletes the file. No other whole file may be gated.
- The pins are gone from this branch (T4.0). Do not add one.

**Verify:** `tests/format_v3.rs`, the keyless acceptance suite, on real stagings: the chain
walk, the zone order, the leak scan with the new needles, a tenant's envelope holding none
of another tenant's tables or ids, the backups byte copies of the primaries, a short tape.
A write of a unit whose `stage_files` rows were deleted is refused by name.
**Done when:** the tags `L3-pending T4.6` are gone; `tapectl volume write` to a `MemStore`
and a read-back of every envelope round-trip the table and the map; the suite is green.

### T4.7 A tape of another layout at contact

**Owner:** worker. **Depends:** T4.6. **Carries out:** item 269; #560.
**Read:** #560; ADR-0003 (File 0 is the consent point); `src/volume/erase.rs` `File0Report`;
`src/volume/format.rs` `FormatClaims::refusal`.

**Fence:** `src/volume/format.rs` (the claims' refusal texts), `src/volume/loaded.rs`,
`src/volume/erase.rs`, `src/volume/binding.rs`, `src/cli/cartridge.rs`, `src/cli/volume.rs`
(texts only).

**Build:** tapectl 2.0.0 knows a tapectl volume of another layout when it meets one. It
reads the label from File 0 and nothing else. Every command that meets one says: this is a
tapectl volume of Layout N, label L; read it with the tapectl that wrote it or with the
RESTORE.sh in its own file 2. `volume init` over it is refused; `cartridge erase` with the
typed label is the way, and it works on such a tape. No envelope, index or seal of another
layout is parsed.

**Traps:**
- This is consent, not compatibility (item 242 stands). Do not add a reader.
- With no readable chip serial the existing carve-out still applies (ADR-0012: a blank tape
  plus a `--cartridge` bound to a live volume is refused).
- The test's Layout 2 File 0 is a literal text in the test. That is a sample of a foreign
  format, not a fixture of our own staging, so invariant 8 does not forbid it.

**Done when:** #560's criteria are met, each with a test on `MemStore`; the suite is green.

### T4.8 tapectl's restores, fed whole members by byte range

**Owner:** worker. **Depends:** T4.6. **Carries out:** items 255 (tapectl's half), 256,
260, 264, 266, 278, 283, 284; #556; D16, D17. **Read:** `volume-format-v3.md` §7.1 (tapectl's reader) and §5.3
(a range); ADR-0012 items 101 to 103 (what a damaged slice and a damaged file mean), 127, 264;
`src/volume/restore/supply.rs`'s module doc; `src/volume/restore/fill.rs`.

**Fence:** `src/db/migrations/001_baseline.sql` (`restores.tar_argv`, `tar_exit_code`,
`tar_stdout`, `tar_stderr`, `tar_version`, each after its `dar_*` twin, with the pin in
`restore_record.rs`'s `restores_has_its_columns_and_indexes`; *added 2026-11-18*: T4.3 could
not add them without that pin), `src/volume/restore.rs`, `src/volume/restore/*`, `src/volume/slice_holders.rs`,
`src/volume/restore_record.rs`, `src/cli/restore.rs`, `tests/restore_key.rs`,
`tests/integration.rs` (restore tests only).

**Build:**
1. **One reader.** A restore decrypts the slices it needs, in order, and writes to tar's
   standard input exactly the byte ranges `filetable::ranges` gives, then the end-of-archive
   blocks. `restore unit` asks for the whole table; `restore file` for its paths;
   `restore volume` for each unit in turn. There is no spool mode and no scratch copy of a
   slice. A range lies inside one slice, or is a long member's run (rule 13).
2. tar is run to extract with these flags (**determined**; T0.1's result may add the one
   flag its kept ACL record needs): `--extract --file=- --directory=<destination>
   --numeric-owner --preserve-permissions --xattrs --acls`, and `--keep-old-files`, or
   `--overwrite` when the restore was given `--overwrite`. Never `--ignore-zeros`, never
   `--absolute-names`. It runs with `LC_ALL=C` and without `TAR_OPTIONS` or
   `POSIXLY_CORRECT`, as the writer does. `--xattrs` and `--acls` are always given: what
   tar could not set it says, tapectl repeats it, and the per-file check is still the
   verdict (rule 6). The reader's tar is not held to the writer's floor.
3. **Every member is checked as it is fed**: its bytes are hashed against the table's
   sha256. A member that fails is not fed and is listed as damaged.
   **A compressed staging** (D15): each slice is decompressed after it is decrypted, and
   the ranges are cut from that. A slice that gives more or fewer bytes than its
   `size_bytes`, or whose bytes do not match `sha256_plain`, is a bad slice, whatever the
   decompressor said of itself.
4. **A bad slice.** `Supply` still replaces a slice from another copy by its sha256, asking
   for the cartridge, as today. When no copy has it: the members in it
   (`filetable::members_of`) are left out and listed, and the restore goes on (item 264). The read around a bad 64 KiB chunk
   (`crypto::chunks`) is kept and made finer by the table: only the members that touch a
   bad chunk are left out. **Nothing is zero-filled** (invariant 4): the tolerant mode's
   holes become ranges that are skipped. In a compressed slice nothing after the bad chunk
   can be decompressed, so the members with bytes from the bad chunk to the slice's end are
   left out; the bytes before it are still used.
5. **The repair.** The damaged paths are restored from another copy of the same version
   through that copy's table (item 255's repair; `fill`). A range is a source for the files
   whose slices it holds whole (item 260).
6. **After tar ends** every restored regular file is hashed on disk against the table,
   unless `--no-file-check` (D16). The verdict of the restore is that check and step 3,
   never tar's exit status alone (invariant 6); a non-zero exit is still reported.
7. The id map of each unit restored is printed, and is in `--json` (item 278). No owner is
   changed by name.
8. A restore that starts on a tape holding only a range takes the files it holds whole and
   lists the rest (item 266, D17), where the other cartridges are not offered. With them
   offered it goes on across the swap as one tar run, as today (item 131); any unit may now
   lie across cartridges (item 288).
9. `--key FILE` works as today on every path above.

**Removed here:** `stream_into_dar`, `spool_and_extract`, `--spool`, the choice between
them, every call into `dar::restore`, `dar::slice::Join`, and the `dar_*` fields written to
the `restores` row (the `tar_*` columns take their place).

**Traps:**
- A hard link is fed only with its target's member, target first. If the target's member is
  damaged, the link is listed as damaged too (item 256).
- A folder's own member is not beside its contents. Restoring one path of a folder feeds
  the folder's member only if it was asked for; tar makes missing parents itself.
- Members are fed in stream order whatever order the paths were asked in.
- `restore file` of a path whose slices are on this tape must read only those slices. The
  motion budget (`cargo test --lib cost_budget`) pins tape motions for each read path.
  Report the figures. Do not move a pin.
- The test fixtures in this area were built by running dar directly (`volume_with`,
  `real_fixture_keyed`, `written_tape`, `archive_to_files`, `extract_catalog`). They are
  deleted, and every test stages through `stage create` (invariant 8).

**Verify:** through real stagings on `MemStore`: a whole unit; one path; a volume of three
units; a slice replaced from a second copy; a slice with no copy (the members in it
listed, every other file whole); one piece of a long member's run with no copy (that
member listed, every other file whole); one bad chunk in a slice of many files (only the files
that touch it listed); the same in a compressed slice (the files from the chunk to the
slice's end listed, the files before it whole); a compressed unit whole and by path, for
each algorithm carried; the repair from a second copy; the repair from a range; a hard link
whose target is damaged; a unit with a file named like an option; a restore as an ordinary
user of files owned by another id (restored, the id map printed, the owner not changed).
Each restore is compared with the source by content and, as root in a test that can be,
by owner, mode, times, xattrs and ACL.
**Done when:** the tags `L3-pending T4.8` are gone; `grep -rn 'dar' src/volume/restore*
src/cli/restore.rs` finds nothing; the suite is green.

### T4.9 `catalog rebuild` and the adoption of a lost write

**Owner:** worker. **Depends:** T4.6. **Carries out:** items 144, 265, 272; #557, #564;
ADR-0012 amendment 2026-09-29 on #360. **Read:** `src/volume/rebuild.rs`'s module doc and
fact table; `src/volume/adopt_lost.rs`'s module doc; `volume-format-v3.md` §1.4 and
Appendix A.4 (the on-tape `catalog.db`).

**Fence:** `src/volume/rebuild.rs`, `src/volume/adopt_lost.rs`, `src/cli/catalog.rs`,
`tests/catalog_rebuild.rs`.

**Build:**
1. A rebuild restores, for each staging the tape holds, the file table and the id map from
   the operator envelope's `catalog.db`, so that a copy written after a rebuild carries
   both (#557, #564). The catalogue files are no longer restored: there are none.
2. **Determined:** a rebuild takes the file table and the id map from the operator
   envelope's `catalog.db` and from nowhere else. The tenant envelope's text is for the
   heir and for a restore given a tenant's key. Add no second source.
3. The adoption of a lost write reads File 0, the seal and the front index, and compares
   ciphertext hashes with the staged rows. It does not read the payload. Confirm that and
   keep it. A staging adopted this way must still have its table, or the adoption is
   refused by name.
4. Item 144's promotion (a version found `staged` with every slice on a sealed volume) is
   unchanged, and serves any unit (item 288).
5. A staging of which this tape holds only some slices is recorded as a range (item 130).
   It is known by the positions the MANIFEST's block gives and by the seal's `ended_after`:
   there is no `may_span` to read (D6). Until T5.1 removes the catalog's `writes.may_span`,
   a rebuild sets it for a range row and clears it for any other.

**Traps:**
- `catalog rebuild` never changes a status, with item 144's one exception. Nothing here
  adds another.
- T2.5's refusal of a unit of the same name under another uuid (#527) must survive this
  task. Its test must still pass, unchanged.
- A rebuilt staging's `origin` is `rebuilt`, and the escrow answer for it is `unknown`
  until attested, as today.
- Until T5.1 the writer still makes `cut` units, whose slices before `ended_after` are on
  the tape. A rebuild now records them as a range, which is the final behaviour (item
  288). A test that expected them to be passed over changes with it: name it in the
  report.

**Done when:** the tags `L3-pending T4.9` are gone; a test rebuilds from a tape, writes a
second copy from the rebuilt catalog, and that copy's envelopes carry the same table and
map; the suite is green.

### T4.10 The heir's script, part one: a file in the tree, the tape, whole units

**Owner:** worker (the script has one worker at a time, T4.10 to T4.13). **Depends:** T4.6.
**Carries out:** items 255, 264, 271, 283, 284; #553; #520, #525 (the script's low
findings), #561's work directory; #562 "The heir's script"; D10, D22. **Read:** `volume-format-v3.md` §7.2 to §7.5 (the heir
path), whole; File 1 as a reader would (the generated text, from a test); #520, #525;
`docs/audits/2026-11-12-design-and-defect-review.md` findings A1 to A12.

**Fence:** new `src/volume/heir/RESTORE.sh` and `src/volume/heir/mod.rs`;
`src/volume/layout.rs` (the RESTORE.sh generator and the tool table only);
`src/volume/restore_script.rs` (deleted when nothing uses it); `src/volume/build.rs` (the
one call that puts File 2 on tape); new `tests/heir_restore.rs` and `tests/common/heir/`;
`tests/heir_restore_sh.rs` (deleted); `.github/workflows/ci.yml` (the lint job); a test
helper program under `examples/`, where `short_seal_filler.rs` already is.

**Build:**
1. The script is **new**, written as a shell file in the tree, and put on tape byte for
   byte (`include_str!`). No Rust string assembles it. It is not a port of the old one:
   read the old one for what a mode must say and for the tape handling that was proven on
   the real drive (one rewind, forward spacing only, the block size, `mt` and `mt-st`
   told apart, a failed read forcing a rewind), and carry that.
2. This task builds: the tool check for each mode from one table; the tape handling;
   File 0's claims (Layout 3 only; another layout refused by name); the front index and
   the seal, full and short (R1 to R4: `ended_after` is learned first, and a slice past it
   is never read); `--info`; `--verify`; `--find-envelope`; the envelope opened into the
   **work directory inside `--to`** (item 271), with the space check before it;
   `--list` from the file table; `--restore` of whole units (`--all`, `--unit`); the file
   check after it.
3. **The reader's primitive** (items 264, 283; #556; D10). Each slice is checked against
   the front index, decrypted into the work directory and, where the MANIFEST's block
   states `compression`, decompressed by that algorithm's tool and checked against its
   `size_bytes`, all before tar sees any of it. The good slices go, in order, to **one**
   tar run. A bad slice is left out and its members are listed from the file table. A long
   member's run (D22) is fed piece by piece, each piece checked first; if a piece fails,
   that tar run is ended, the member's partial file is removed and the member is listed,
   and the next run starts at the slice after the run. The work directory holds one slice
   at a time. No zero-fill, no `--ignore-zeros`.
4. **CI lints the script** (#553): `bash -n`, `shellcheck`, and the script's awk programs
   run under `gawk`, `mawk` and busybox `awk`, in a job of `ci.yml`.
5. The tests' fixture is a real staging and a real volume build, laid out as the directory
   tape the old tests used, with `mt` and `dd` as stand-ins (carry `FAKE_MT` and `FAKE_DD`).
   **`age` in the tests is a small program built from the `age` crate** (an example, as
   `examples/short_seal_filler.rs` is) that takes the
   arguments the script gives the real `age`, so the tape holds what staging wrote. If that
   cannot be done without a new dependency, stop and report.

**Traps:**
- The heir's readers stay wide (invariant 10): POSIX `sh` features plus what the old script
  already relied on; GNU tar of any version, bsdtar, busybox tar (content only); GNU and
  uutils coreutils. **Ubuntu 26.04's uutils hangs on `tail -c 1 <fifo`** and its `cat` and
  `tee` enlarge pipes to 1 MiB: read no FIFO for a slice's end, and prove the script in the
  `tapectl-u26` container as uid 1000 (the coordinator runs that).
- A full or unwritable work directory is said to be that, never "a damaged envelope" (#520).
- `sha256sum` on some systems quotes names with a backslash; the file check must read both.
- `LC_ALL=C` is exported, and a `TAR_OPTIONS` in the heir's environment is cleared.
- The decompressor is in the tool table for a unit that states compression, and only for
  such a unit: an heir restoring an uncompressed unit is never asked for it. It is checked
  before the tape is read.
- A decompressor's exit status is given no weight (BusyBox `unxz` exits 0 after writing
  wrong bytes). The slice's decompressed length against `size_bytes`, and then the per-file
  check, decide.
- The verdict is the per-file check (invariant 6). busybox tar restores no xattr, no ACL
  and no sub-second time, silently: the script says which tar it found and what that tar
  does not restore.
- The old test file's 203 tests are the checklist of behaviours. For each group, the new
  tests either carry the behaviour or the report says why it no longer exists (dar's own
  cases: the catalogue, the stream join, dar's versions, the late-damage note).

**Done when:** the tag `L3-pending T4.10` is gone and `tests/heir_restore_sh.rs` is
deleted; `--info`, `--verify`, `--find-envelope`, `--list` and whole-unit `--restore` each
have tests on a full tape and on a short one, and whole-unit `--restore` on a compressed
unit for each algorithm carried, with GNU tar and with a stand-in that drops a
file at exit 0 (the check must catch it); the lint job passes; the suite is green.

### T4.11 The heir's script, part two: paths, the one repair, the index, the id map

**Owner:** worker. **Depends:** T4.10. **Carries out:** items 255, 256, 260, 276, 278;
#518's criteria (carried by #555), #521, #528; D10.

**Fence:** `src/volume/heir/RESTORE.sh`, `tests/heir_restore.rs`, `tests/common/heir/`.

**Build:** `--restore --unit U --path P` and `--paths-from FILE` (only the slices that hold
the wanted members are read; a hard link brings its target, target first; a member is
taken from its slice by name or by its offsets, as T0.7's step (e) decided, D10); the damaged-file list,
written in the index's escaping, and the repair that reads it back as `--paths-from` on
another copy of the same version (another version or another unit is refused; a range is a
source for the files it holds whole); `--where` and `--find` over the index, as today's
tests have them; the id map printed for each unit restored.

**Traps:**
- #518's defect was a path restore that read the wrong slices and exited 0. The test here
  restores one path from a unit of many slices and proves, from the stand-in `dd`'s log,
  **which** tape files were read, and that the file is right.
- #528: the check is against this tape's table, the unit dotfile included, on a tape
  written from a re-stage.
- The list's paths must round-trip a tab, a newline and a backslash.

**Done when:** each mode above has a test; the repair restores two copies each damaged in
different files to a whole unit in two runs; the suite is green.

### T4.12 The heir's script, part three: a unit across cartridges

**Owner:** worker. **Depends:** T4.11. **Carries out:** items 240 (R5, R8, R9 stand), 246,
266, 267, 283, 288; #558; #521.

**Fence:** `src/volume/heir/RESTORE.sh`, `tests/heir_restore.rs`, `tests/common/heir/`.

**Build:** the restore of a unit held across cartridges, which any unit may be (item 288),
as items 266 and 267 have it: one tar run across the swap, with the one prompt for the
next cartridge (it waits without a time limit and holds no device). A cartridge is taken
only by the next slice's sha256 at a position its seal covers (R8). A blank, foreign or
unreadable cartridge at the prompt is named and the prompt asks again (#521). **A restore
may start on any tape** and takes the files that tape holds whole (item 266), which under
item 283 is every file on it but one larger than a slice that lies across the boundary.
**"I do not have it" is an answer**: the restore goes on with what it has, and lists the
rest with the cartridges that hold it. A file whose run lies across the two cartridges is
then removed again if tar had begun it, and is listed. Nothing is kept in the work
directory between cartridges (item 267).

A later run on another tape of the same unit, into the same `--to`, adds that tape's files
and leaves alone the ones already restored and checked. Two things are said to the heir
when a unit is read in more than one run (both measured, #567): folder times are those of
the last run that touched the folder, and a hard-linked name whose first name is on a tape
not yet read is not restored. Such a name is listed with the cartridge that holds its
first name, and the repair restores it once that tape has been read.

**Traps:**
- A file that lies across two cartridges is held whole by neither alone. Started on the
  second tape without the first, it is listed, not restored.
- Quitting at the prompt, Ctrl-C, a hangup or a kill leave no work directory and no
  partial file behind.
- Read the old tests' section "span, across cartridges" (thirteen tests) for the cases.

**Done when:** a unit on three tapes restores across two swaps as one run, in either
copy's order; a file larger than a slice that lies across two cartridges restores through
the prompt, and is listed with no partial file left after "I do not have it"; a start on
the second tape alone restores what it holds whole and lists the rest; a second run on the
first tape then completes the unit, a hard-linked name listed by the first run and
restored by the repair; the suite is green.

### T4.13 File 1 and RECOVERY.md

**Owner:** worker. **Depends:** T4.12. **Carries out:** #522, #523, #526; #563's last
criterion; #477's and #481's standing rules for File 1; items 264, 266, 271, 278, 283, 288.
**Read:** the generated File 1 and RECOVERY.md of the current tree, whole; #522, #523, #526.

**Fence:** `src/volume/layout.rs` (File 1's and RECOVERY.md's generators and the tool
sentences), `src/volume/build.rs` (`RECOVERY_CATALOG_DB`), their tests.

**Build:** both texts rewritten for Layout 3: the manual path with `age`, `tar` and the
file table, run as written in a test (the "File 1 as written" tests of the old suite are
the model: each command in the text is executed by a test); which tars restore what; what a
restore on another host does with an owner or an ACL entry, and that the id map says who
the ids were; the one repair; that each data file is an archive of whole files, and which files are one
long file's run; that a unit may continue on another cartridge, the prompt and "I do not
have it", and what a tape read alone gives; per-unit directories
(#522); the right route for each key holder (#523); only messages `age` prints (#526); how to tell
a compressed unit, which tool reads each algorithm, and the manual path for one, run as
written by a test.

**Traps:**
- A literal of the current version goes stale at the next release. Write `tapectl-X.Y.Z`
  and "such as 2.0.0".
- File 1 is plaintext on tape: no unit name, file name, id or key fingerprint in it
  (invariant 3).
- Every command in either text is run by a test, as written, or it is not in the text.

**Done when:** no line of either text names dar; every command in them is run by a test;
the three issues' criteria are met; the suite is green.

### T4.14 The Heir Kit's sheet, the guides, the installers and the host

**Owner:** worker edits; the coordinator proves the scripts. **Depends:** T4.13, T0.2.
**Carries out:** item 275; #524; #563 (the early refusal in `first-run.sh`).
**Read:** #524; T0.2's note (the recipe); `contrib/hosts/home2-prep.sh` steps 6 and 7.

**Fence:** `src/crypto/escrow_kit.rs`; `docs/operator-guide.md`, `docs/troubleshooting.md`,
`docs/keys-and-recovery.md`, `docs/install.md`, `docs/quick-start.md`, `docs/concepts.md`,
`docs/walkthrough.md`, `docs/configuration.md`, `docs/development.md`, `README.md`;
`scripts/first-run.sh` (step 2, step 7's config, the texts), `contrib/hosts/home2-prep.sh`,
`contrib/hosts/home2.profile`.

**Build:** the cover sheet's steps and tool list for Layout 3, with the decompressor for
each algorithm carried named as needed only for a unit that states it (#524: no claim that
`restore raw-volume` works with no database unless a test shows it; no remedy that mints an
escrow identity); every guide's account of staging, restore and damage in tar's terms;
`first-run.sh` step 2 checks tar against the floor and no longer installs dar;
`home2-prep.sh` builds GNU tar by T0.2's recipe into `/usr/local`, pinned by sha256, leaves
the system's tar alone, proves ACL entries are written by number, and no longer builds dar.

**Traps:**
- Do not run `first-run.sh` or `home2-prep.sh`. Do not ssh anywhere.
- `docs/releases/`, `docs/runs/`, `docs/audits/`, `docs/research/` and the ADRs are dated
  records. They keep saying dar.
- `scripts/check-docs.py` holds every `tapectl ...` line in the guides to the binary.

**Done when:** `check-docs.py` passes; the cover sheet's tool list is held to the script's
tool table by its existing test; `bash -n` passes on both scripts.

### T4.15 dar leaves

**Owner:** worker. **Depends:** every task above. **Carries out:** item 241; D11's renames.

**Fence:** `src/dar/` (deleted), `src/lib.rs`, `src/error.rs`, `src/config.rs`,
`src/config_edit.rs`, `src/policy/*` (the dar checks), `src/cli/archive_set.rs`,
`src/db/migrations/001_baseline.sql`, `src/db/models.rs`, every file that still names a
dar column, `tests/test_dependencies.rs`, `.github/workflows/ci.yml`, `Cargo.toml` if a
dependency served only dar.

**Build:** delete `src/dar/` and what only it used (the seccomp filter, the slice template,
`dar_masks`); drop the columns and the table D11 names from the baseline and from every
query; remove `[dar]`, `preserve_fsa` and the keys D11 removes; **`compression` stays** (item
280), and what goes with dar is only the probe that asked the local dar which algorithms
it was built with (`policy::compression_capability`, `cli::archive_set`'s check) and the
values item 285 does not carry; rename the JSON keys; CI no longer installs dar; `tests/test_dependencies.rs` names tar at the floor, `setfacl` and
nothing of dar.

**Done when:** `grep -rniw dar src tests scripts contrib .github Cargo.toml` finds only
history in comments that the coordinator accepts, listed in the report; `grep -rn
'L3-pending' src tests` finds nothing; the suite is green with dar absent from `PATH`
(run it once that way and say so).

### T4.16 The gate and the suites on Layout 3

**Owner:** coordinator (a `scripts/` change is proven only on tape). **Depends:** T4.15, T4.17.

`scripts/mhvtl-verify-gate.sh`, `scripts/lifecycle-suite.sh`, `scripts/plaintext-scan.sh`,
`scripts/short-seal-acceptance.sh`, `scripts/span-acceptance.sh`, `scripts/lbp-acceptance.sh`,
`scripts/first-run-reentry-mhvtl.sh` and `tests/mhvtl_e2e.rs` are carried to Layout 3.
**One gate leg, with the real tools, for every mode the heir's script has** (the rule from
the review): `--info`, `--verify`, `--find-envelope`, `--list`, whole-unit `--restore`,
`--path`, `--paths-from` as the repair from a second cartridge, `--where`, `--find`, a
short tape, a unit across cartridges with a swap, a file larger than a slice across the
boundary, a start on a later tape, a second run that completes the unit, and "I do not
have it", **and a whole-unit and a one-path restore of a compressed unit for each
algorithm carried, with the real decompressor**. Legs that proved dar's stream (`heir_big_catalogue`, `heir_late_damage`,
`heir_slice_boundary_read`) go with it; `heir_file_check` and `heir_slice_from_copy` are
restated. The lifecycle suite's restore matrix keeps its twelve methods. Each run record
names the age version and the tar version. **Done when** the whole chain of T6.5 is green
on `layout-3`.

### T4.17 A name that is not UTF-8 (#569)

**Owner:** worker. **Depends:** T4.8, T4.12 (the heir's script is edited after its part
three). **Carries out:** item 297; #569. *Added 2026-11-19, when the CTO ruled #569 into
2.0.0.* **Read:** ADR-0012 item 297 and its determinations; `volume-format-v3.md` §1.5 (the
escape's fifth case), §1.6, §1.7, §7.2 to §7.4; #569; `src/volume/index.rs`'s `escape` and
`unescape`.

**Fence** (the coordinator narrows it at dispatch with a grep of the lossy conversions of a
path inside a unit): `src/volume/index.rs` (the escape), `src/volume/filetable.rs`, the
snapshot's walk and the exclusion match (`src/staging/`), the list given to tar
(`src/staging/archive.rs`, `src/tar/`), the stream reader's comparison (`src/tar/stream.rs`,
`src/tar/pax.rs`), the catalog's writers and readers of paths (`src/db/`), the restores'
lookup, disk check and repair (`src/volume/restore.rs`, `src/volume/restore/`), `unit
check-integrity`, the commands that take a path (`src/cli/restore.rs`, `src/cli/unit.rs`),
`src/volume/heir/RESTORE.sh` (the per-file check's unescape) with `tests/heir_restore.rs`
and `tests/common/heir/`, and their tests.

**Build:**
1. **One pair of functions.** `escape` takes a name's bytes and writes §1.5's form, `\xHH`
   for each byte outside a valid UTF-8 sequence; `unescape` gives the bytes back. Every path
   inside a unit passes through them, and through nothing lossy: `to_string_lossy` and
   `to_str` do not touch such a path anywhere.
2. **The snapshot** records each path in the escaped form; the catalog keeps that form
   (item 297). The exclusion patterns match a UTF-8 name as before; a name that is not UTF-8
   is matched in its escaped form.
3. **Staging** writes tar's file list from the unescaped bytes (`--null`), and the stream
   reader compares the stream's bytes with the list's. The pax `path` and `linkpath` records
   carry the raw bytes under `LC_ALL=C` (measured 2026-11-19, GNU tar 1.35); a record under
   `hdrcharset=BINARY` means the same bytes and is accepted. The file table, the id map and
   the index are written with the escape.
4. **tapectl's restores** find a member by its escaped path, check it on disk by its bytes,
   and repair it by the same. `restore file --path`, `unit check-integrity` and every other
   command that takes a path inside a unit take it as bytes from the command line and escape
   it for the lookup. Output and `--json` show the escaped form.
5. **The heir's script** unescapes a path only where it names a file on disk: the per-file
   check. `--path` and `--paths-from` take the escaped form, as `--list` and the
   damaged-file list write it. The unescape is one awk program, run under gawk, mawk and
   BusyBox awk with `LC_ALL=C`.
6. A unit's own root that is not UTF-8 is refused by name at `unit init`; unit, tenant and
   location names stay UTF-8 text, as today.

**Traps:**
- A UTF-8 name is written exactly as before on tape. In the catalog a name with a
  backslash, TAB, LF or CR now reads escaped, as on tape; a test names one.
- The id map's host names are text from the host: the same escape covers a name that is not
  UTF-8.
- `printf %b` is not the unescape: it reads other escapes (`\c` stops output). Write the
  bytes by their codes, and prove the awk program on all three awks.
- `sha256sum -c` takes raw bytes in a name; a name with a backslash or LF needs sha256sum's
  own escaped line, which T4.10's check already writes and reads.
- Nothing in a JSON document is a byte outside UTF-8: the escaped form is what `--json`
  carries.

**Verify:** a unit holding a name in Latin-1 (`caf\xe9.txt`), a folder so named with a file
in it, a hard link and a symbolic link whose target is such a name, and UTF-8 names with a
backslash, a TAB and an LF. Stage it, write it on `MemStore`, then:
- `restore unit`, and `restore file` with the name typed as bytes;
- `catalog rebuild`, and a second copy written from the rebuilt catalog;
- `unit check-integrity`, clean;
- the heir's `--list`, `--restore`, `--path`, `--paths-from` with the damaged-file list
  round-tripping the name, and the per-file check catching a damaged one.

Every restored name is compared byte for byte with the source. The test that writes the
unit fails on the parent (the staging is refused).

**Done when:** #569's criteria are met; the suite is green; the coordinator's Ubuntu 26.04
container run of the heir's tests passes as uid 1000.
---

## S5. Stage 5: every carried feature, proven on the new payload

**Starts when:** T4.16 is green. **Lands on:** `layout-3`, which is merged to `master` when
this stage is done. **Normative:** the Layout 3 set. "Simplify" removed compatibility, not
features (items 242, 246, 248): each feature below exists in 1.9.2 and must be shown whole
on Layout 3. Most of this stage is proof, with the fixes the proof finds.

Waves: T5.1 alone (it touches the write plan); then T5.2 and T5.4; then T5.3 alone (it
shares `src/cli/audit.rs` and `src/cli/report.rs` with T5.2, and its order of work is a
safety rule); then T5.5, T5.6, T5.7.

### T5.1 Every unit continues at the early warning, and the queue is filled to it (items 246, 288, 289)

**Owner:** worker, then the coordinator on tape. **Depends:** T4.16. **Carries out:** items 288 and 289, and what stands of items 121 to 147 (the
amendment of 2026-11-17 lists both); #567. **Read:** `volume-format-v3.md` §5.1 to §5.3
(what follows the warning, the one bound, a range) and `layout-session.md` "Layout 3"; ADR-0012's amendment of 2026-11-17, whole, and items 121 to
130, 138 and 144; `src/volume/layout_model.rs` `CapacityBudget::place`;
`src/volume/session.rs` `span_split`; `src/collection/selector.rs`.

**This is a build task, and then a proof.** The range's records on tape, copies counted
for each slice, the per-slice release of staging, `audit`'s `span_in_progress` and range
invariants, a rebuild of a range tape and tapectl's restore across cartridges were built
for units that opted in. They now serve every unit, and the policy above them changes.

**Build:**
1. **No unit is `cut`** (item 288). A unit the early warning interrupts gets a `range` row
   for the slices written; the rest stay staged and are owed. The write status `cut`,
   `span_split`'s cut branch, and every text and report that names a cut unit go.
2. **No unit opts in.** `allow_spanning` goes from `[defaults]`, from the archive set (its
   flag and its column), from the unit dotfile and its frozen copy, and `writes.may_span`
   with it, with the policy code that resolves, shadows or subsumes the key (D11). The
   baseline loses those columns and the status (D9).
3. **One class in the plan.** `CapacityBudget::place` lays out the owed slices first, the
   lowest first, as one unbroken run in slice order (item 122's first rule, which
   stands), then the queue in order. The two classes, the early start of a unit larger
   than a tape, and `after_the_tail` go (items 83, 94, 95, 147).
4. **The one bound** (D20) and **the batch** (D19).
5. `audit`'s `span_in_progress` names any version with slices owed, and its range
   invariants hold for every unit.

**The plan, exactly (determined).** Worked out by the coordinator on 2026-11-17 against
`docs/design/layout-session.md` ("The short ending", "Spanning") and the code, so that this
task needs no design judgment. It is what T1.2 writes into the session document in place
of those two sections' rules by class. If the code shows a line here wrong, correct it and
say so in the report; a correction that would change bytes on tape, the schema or a
command's spelling goes to the CTO first.

*What the plan works from.* Unchanged (items 122, 127): the catalog records no copy or
span identity, and the planner works from per-slice shortfalls. A slice's copies are the
distinct eligible volumes holding a `written` position of it in a `completed` or `range`
row. A slice is **owed** while its copies are below its set's copy floor.

*The plan order.* On each tape:
1. **Owed runs.** A stage set with a `range` row on a sealed volume and slices still owed
   is in progress. Its owed slices are laid out first, as one unbroken run in slice
   order, from its lowest owed slice to its highest. This is today's rule 1 with the word
   "spannable" taken out. Several sets in progress go in the order the write takes them.
2. **The queue.** Every other stage set the write takes, in the order the write takes
   them. There is no second class and nothing is deferred behind another set.
3. **The bound** (D20). On a drive that declares `post_warning_bytes`: sets are added
   while the plan's end is at or below the cartridge's nominal capacity. The set that
   carries the plan past it is planned to its end, and no set is planned after it. An owed
   run is held to the same rule. On a drive without the key: slices are added in order
   while the plan's end is at or below the fill ceiling, and the plan stops at the last
   slice that fits, inside a set if that is where it falls. No write is refused for the
   missing key.

*In the code* (`CapacityBudget::place`, `src/volume/layout_model.rs`). `PlanClass` keeps
`Owed` and loses the difference between `Whole` and `Spannable`: every queued set is placed
as a spannable one is today, by `span_placed` (committed under the fill ceiling, past the
ceiling within the bound, and the tail across it, planned to its end). The first tail ends
the plan, and what follows it is not on this tape. `Placed::Refused` and
`Placed::Deferred` are no longer reached by a set's size, and the baseline of whole units
(`whole_run`, `split`) goes. **One part is new work:** on a drive with no plan bound the
code refuses a set that passes the fill ceiling (`SpanNeedsPostWarningBytes`). It must
instead plan that set by its slices up to the ceiling, an end the plan chooses inside
the set, which no code does today.

A volume still holds at most one unbroken range of a stage set (item 122). With a copy
target of 2 at one place this order still gives item 93's layout: tape 1 ends in the
set's slices 1 to K; on tape 2 the owed run is the whole set, so tape 2 begins with it
and holds it whole; tape 3 begins with the owed run K+1 to N.

*What `may_span` did, without it.* Every place that read `writes.may_span` (the build's
`requires`, the seal's placeholder, confirm, resume, a rehydrated session) now takes the
branch it took when the flag was set. `requires` is empty (D1), so the build and the seal
state nothing for it.

*Confirm's split, for every row of the session,* on a full seal as on a short one:
- `completed` when every slice of the stage set, 1 to N, is `written` on this volume at
  or before `ended_after`;
- `range` when at least one is: the range is the row's `written` positions, and its
  positions past the end stay `pending`, as item 121 has them;
- **when none is, the row and its positions are deleted** in the confirm transaction. The
  plan listed the set and the seal withdrew it. Its slices have no copy from this tape,
  so the per-slice release leaves them staged, and the next write takes them as part of
  its queue. The `write_completed` event names such a set as planned and not written,
  where it named a cut unit.

`catalog rebuild` and the adoption of a lost write make the same split from the tape.
For a staging none of whose slices the tape took they write no `writes` row; what else
they record of it is what they record today beside a `cut` row.

*After a tape that ended in a range.* The command exits 0, names each set the tape ended
in with the slices it holds, and says that an ordinary `volume write` onto another
cartridge of the same generation continues it. That text exists for a spannable set
today and now serves every set.

*Refusals.*
- **Gone:** the refusal of a set past the fill ceiling on a drive without
  `post_warning_bytes` (D20 has what happens), and the refusal at stage time of a unit
  larger than one tape that does not allow spanning.
- **Stand:** one generation for one copy's span (item 86: a run that does not start at
  slice 1 is planned only on a cartridge of the generation of the volume whose range
  ends just before it); an owed slice whose staged file was released is refused, naming
  `volume read-slices --from`; a unit larger than the staging area is refused by name
  (item 85). Item 86's refusal can now meet any batch: its text must say which
  generation the next cartridge has to be.

*Promotion and the retire floor* (item 128), *copies, places and deposits* (items 127,
134, 138) and *the per-slice release* (item 129) are unchanged. They were written for a
set that may span, and every set now may.

*One planner.* Today a write has two: `build_write_layout` when no set in the batch
spans (it builds every staged unit, and refuses the whole write when they do not all
fit), and `build_span_layout` when one does (a probe build, `place`, the build in plan
order, and `read_back_span_plan`). The second becomes the only one. Every staged set is
classed by `plan_span_set`'s rule, which today serves a set that allows spanning or has
a range: `Owed` when it has a range row on a sealed volume and a slice below its floor,
and otherwise a queued set with its whole slice list. A write is never refused because
the staged sets do not all fit: the plan takes what the bound allows, and the rest
waits, staged.

*The batch* (D19). The selector is next-fit in the collection's order and stays so: it
never went back for a smaller unit. What changes is where a batch closes. Today it closes
before the first unit that does not fit. Now units are added while the plan's end is at
or below the bound, and the unit that carries it past is the batch's last: the bound is
the cartridge's nominal capacity on a drive that declares `post_warning_bytes`, and the
fill ceiling on one that does not. `plan_batches_spanning` does this today for units that
allow spanning, through `CapacityBudget::place`; it becomes the one path, and
`plan_batches`, `next_fit`'s close, `unit_limit`, `OversizedUnit` and `after_the_tail`
go. "Bounded by the staging area" (D19) is what the code does today and no new bound: a
unit larger than the staging filesystem is refused by name, and each unit's stage checks
the free space before it starts. The room that slices already staged will take on the
tape still comes off the batch's room (`already_staged_on_tape_bytes`), as today.

*The batch after staging* (item 289, kept for a unit that packs). The selector works from
sizes estimated before staging, and the write's plan from the staged sizes. An estimate
is always high for a compressed unit, so by the estimate alone its tape would end by its
plan, short of the warning. So `collection run` stages in rounds: after it has staged the
batch the selector chose, it asks the plan, and while the plan has no set that passes
the bound and a pending unit remains, it stages the next one and asks again. The run
then writes once, as today. `collection plan` shows the batch by estimate and says that
a run may take more.

*`volume rewrite`'s destination, in this task.* A rewrite plans under the fill ceiling
and never past it (item 142's reason stands: it may hold a unit's only copy). Today its
write is planned with `may_span` unset, and a unit the early warning interrupts is `cut`:
the rewrite stays open on the cut sets and the hold keeps their staged slices. With no
`cut` and no `may_span`, the interrupted set is a range on the destination, and two tests
that read `completed` have to read slices:
- **the hold** (`rewrite::holds`, `coverage::fully_read_back_copy`). Today a set is held
  while its rewrite is open and no eligible volume has a `completed`, verified write of
  it. A range is never that, so the hold would never lift. It becomes: a staged slice of
  a set the rewrite staged is held while the rewrite is open and no eligible volume
  holds that slice at a `written` position in a `completed` or `range` row that was read
  back in full. `pending_rewrite_sets` still records stage sets; the test is made slice
  by slice, and needs no new column.
- **the close** (`destination_facts`, `cut_pending`). A rewrite closes when every slice
  it staged passes that test, where today every set must be `completed` and verified on
  the new volume. Until then it stays open, and the stop names the ordinary `volume
  write` with a full readback that writes the slices not yet taken.
A source that holds a range is still refused in this task (`refuse_range_holder`); T5.3
lifts that.

*The schema.* `writes` loses `may_span`, the status `cut`, and the check that a `range`
row has `may_span` set. Its uniqueness on `(stage_set_id, volume_id)` stays, which is
what keeps one range of a staging on a volume. No column is added.

*One invariant more* under `audit`'s `short_seal` check, beside the range invariants: a
`completed` row holds a `written` position of every slice of its set. Nothing checks
that today, and a wrong `completed` row counts copies that do not exist.

**Fence.** This task is alone in its wave, and its fence is drawn by what it removes:
every file under `src/` and `tests/` that
`grep -rlE "allow_spanning|may_span|PlanClass|Spanning::|Split::Cut|ReleaseBlocker::Cut|'cut'"`
names, with `src/db/migrations/001_baseline.sql`, `src/collection/batch.rs` (the rounds),
`src/policy/coverage.rs` and `src/volume/rewrite.rs` (the hold and the close), and the
generated pages. On 2026-11-17 that was the planner (`src/volume/layout_model.rs`,
`write.rs`, `build.rs`, `session.rs`), the catalog's readers of a row's status
(`adopt_lost.rs`, `rebuild.rs`, `restore.rs`, `restore/span.rs`, `figures.rs`,
`src/staging/clean.rs`, `release.rs`, `mod.rs`), the policy key (`src/config.rs`,
`src/policy/*`, `src/unit/*`, `src/cli/archive_set.rs`), the selector and the batch
(`src/collection/*`, `src/for_location.rs`), and the commands' output (`src/cli/volume.rs`,
`collection.rs`, `catalog.rs`, `restore.rs`, `stage.rs`, `audit.rs`).
`scripts/span-acceptance.sh` and `scripts/short-seal-acceptance.sh` are the
coordinator's to run. The heir's script and File 1 are not in it: T4.10 to T4.13 wrote
them for the rule this task builds.

**Traps:**
- **W1 to W9 do not change.** Do not touch the block retry, the seal, its `ended_after` or
  the readback past the warning.
- `volume rewrite` and compaction still refuse a **source** that holds a range until T5.3.
  Do not lift that here: lifting it before T5.3's selection is in place erases slices
  (T5.3's first trap).
- Items 93, 134 and 139 stand: with a copy target of 2 at one location the plan must still
  yield the whole middle tape. Keep the proof of it.
- An owed run bound to a location (item 159, as built) is unchanged.
- The budget suites pin what the planner and the selector cost. Report a moved figure. Do
  not move a pin.
- A `allow_spanning` left in a config or a dotfile is then an unknown key and is refused
  as one. Do not add a refusal that names it (item 258(d)).

**Done when:** a unit interrupted by the test hook's early warning has a `range` row and no
`cut` row, and the next write lays its owed slices first; a batch queued to fill ends a
full tape with one unit across its end; on a drive without `post_warning_bytes` the tape
ends by its plan at the ceiling and the rest continues; a batch of units that pack is
staged in rounds until its plan passes the bound; a rewrite whose destination meets the
early warning leaves a range, stays open, and closes after an ordinary write with a full
readback has taken the rest; item 93's shape (two copies: tapes
1 and 3, or tape 2 alone) and a unit on three tapes restore whole through tapectl and
through the heir's script, in tests and on mhvtl (`scripts/span-acceptance.sh --mhvtl`);
`grep -rn 'allow_spanning\|may_span' src tests` finds nothing; the suite is green.
*Added at T4.16 (the coordinator):* the "PENDING T5.1" block of
`examples/short_seal_filler.rs`'s `compare` (a writer's `cut` read back by a rebuild as the
`range` it is) is deleted, and `scripts/short-seal-acceptance.sh --mhvtl` passes without it;
the `# T5.1 removes this` lines of the gate and the acceptances go with `allow_spanning`.

### T5.2 Two places, and the per-location writes (#550; items 147 to 160, 257)

**Owner:** worker. `audit` holds active units to two places through `[defaults] copies_at`
(item 257); `min_locations` no longer binds tape-only units alone, as #550's criteria say.
`stage for-location`, `volume write --for-location`, `report needs` and
`--keep-staging-for-locations` are proven on tar stagings. **Determined:** `stage
for-location` stages a version again by reading its slices back from tape, and today it
needs one live volume that holds every slice (`src/for_location.rs`, the read-back's
choice of a source). After T5.1 a version often lies on two cartridges, so it reads each
slice from a volume that holds it, asking for one cartridge after another, as tapectl's
restore does. **Fence:** `src/policy/*`,
`src/for_location.rs`, `src/cli/audit.rs`, `src/cli/report.rs`, their tests.
**Done when:** #550's criteria are met; `audit` is not clean with every copy of an active
unit on one shelf, shown by a test.

### T5.3 Compaction, `volume read-slices`, `volume rewrite`, and a range in each

**Owner:** worker. **Depends:** T5.1. **Carries out:** item 288's
supersession of item 133. These move ciphertext and check ciphertext hashes; none reads
the payload. Prove each on Layout 3 tapes: a compaction and a rewrite carry the file table
and the id map into the new volume's envelopes (they re-pack envelopes from the catalog,
where they once re-packed catalogue files from disk), and `volume rewrite`'s hold and
typed consent are unchanged.
**New, a build:** a rewrite and a compaction carry a volume that holds a range, as D21
has it. The slices are carried as they are, by ciphertext, in slice order; the new
volume's block lists every slice of the staging and gives positions to the ones it holds;
the typed consent names every version of which the volume is the only holder of some
slice; `refuse_range_holder` goes.

**A range carried, exactly (determined).** Worked out by the coordinator on 2026-11-17
against item 106, `layout-session.md` ("Later contacts", "Spanning"), `src/volume/rewrite.rs`
and the compaction functions of `src/volume/write.rs`, under D21. The same standing as
T5.1's: correct it where the code shows it wrong, and take to the CTO a correction that
would change bytes on tape, the schema or a spelling. T5.1 has already made the hold and
the rewrite's close tests of each slice; this task is about a **source** that holds a
range.

- *What is carried.* One query decides it, `live_recovery_slices`, shared by the
  rewrite's plan and `compact-read`. Today it takes `written` positions of `completed`
  rows whose snapshot is neither reclaimable nor purged. It takes those of `range` rows
  too. A source's `range` row is one unbroken run of a staging's slices (item 122), and
  it is carried as that run.
- *The read's own gate.* `compact-read` promotes a stage set to `staged` only when every
  slice of the set was read, and errors otherwise. For a range that can never hold. It
  becomes: every slice the source holds of the set was read. `volume read-slices` already
  stages a range's slices this way.
- *The destination's plan.* T5.1's one planner, held under the fill ceiling. The slices
  are written as the run they were read as; the stagings keep the order the source held
  them in. A rewrite adds nothing the source did not hold, unless `--with-staged` is
  given, and then too it plans under the fill ceiling.
- *Two sources that hold runs of one staging* (a compaction onto one destination). They
  go onto one destination only when their runs join into one unbroken run. Otherwise the
  candidate planner does not pair them, and a pairing asked for by hand is refused,
  naming the staging: a volume holds at most one unbroken range of a stage set.
- *The rows and the block.* The destination's rows come from confirm's split (T5.1):
  `completed` when it holds the staging whole, `range` otherwise. So a carried range
  stays a range, and two runs that join into the whole staging become a whole copy. The
  MANIFEST's block lists every slice of the staging and gives positions to the ones this
  volume holds (item 125).
- *`compact-finish`'s gate.* It retires the source only when every live slice the source
  holds is held by another eligible volume. Today it compares `completed` rows on both
  sides, so a source's range slices are not checked and a copy held elsewhere as a range
  does not count. It asks `sole_slices`, which already reads both kinds of row.
- *The consent* (`volume rewrite`). `sole_slices` and `versions_at_stake` already read
  `range` rows, so the consent is asked when it should be. Its facts change: for a
  version of which the source holds some slices, the line says which slices, and names
  the volumes that hold the rest. "Every slice is read from the source into staging" must
  be true of what the source holds, range slices included.
- *One generation* (item 86). A rewrite's destination is the source's own cartridge. A
  compaction's destination that takes a run not starting at slice 1 must be of the
  generation of the volume whose range ends just before it, as any write must.
- *The candidates.* `report compaction-candidates` and `audit`'s `compaction_candidate`
  leave out any volume with a range row and size a volume by its `completed` rows. They
  count both kinds of row.
- *What goes.* `refuse_range_holder` and its five calls (`volume rewrite`'s plan,
  `compact-read` twice, `compact-finish`, `volume compact`), and item 142's first half ("a
  spannable unit a rewrite carries is planned and recorded whole"). Its second half, the
  fill ceiling, stands.

**Traps:**
- **The order of work is a safety rule.** Remove a refusal only in the commit that also
  holds the selection of `range` rows and a test that carries a source holding only a
  range. With the refusals gone and the selection unchanged, a rewrite would not stage a
  range's slices, the consent would still name them, and the cartridge would be erased
  with them on it.
- A lead to confirm before building on it (`DEFECT NOT PRESENT` if it is not there):
  `volume rewrite --with-staged` appears to rehearse its write with spanning off and then
  write with the unit's own policy, which item 142 forbids. After T5.1 there is no such
  policy; the write must still stay under the fill ceiling.
- `compact-write` is an ordinary `volume write`. It already writes an owed run as a
  range. What it lacks is the source's side.

**Fence:** `src/volume/rewrite.rs`, the compaction functions, `live_recovery_slices` and
`refuse_range_holder` of `src/volume/write.rs`, `src/cli/volume.rs` (the two calls of
`refuse_range_holder` and their texts), `src/cli/report.rs` (the compaction candidates),
`src/cli/audit.rs` (`compaction_candidate`), their tests.
**Done when:** a rewritten and a compacted volume each restore whole and rebuild to the
same rows; a volume that holds the tail of one unit and the head of another is rewritten
and is compacted, and each unit still restores whole from its cartridges; a source that
holds only a range is rewritten, with the typed consent naming the version and the
volumes that hold its other slices; `compact-finish` refuses while a range slice of the
source has no other holder; the suite is green.

### T5.4 `tapectl setup`, and the warehouse procedure

**Owner:** worker. `setup` checks tar against the floor, prints what needs root and never
runs it, and its page (`docs/quick-start.md`) tells a friend on Debian 12 or Ubuntu 22.04
how to get a tar that meets the floor (item 274). The warehouse deposit of sealed volumes
is a documented procedure over ciphertext and is restated for Layout 3's restore.
**Fence:** `src/setup.rs`, `src/cli/setup.rs`, `tests/setup.rs`, `docs/quick-start.md`,
`docs/operator-guide.md` (the warehouse section).
**Done when:** `setup` on a host whose tar is below the floor names the floor and stops
before it makes a home, shown by a test with a stand-in tar; the quick start's steps pass
`check-docs.py`; the warehouse section's restore is the Layout 3 one.

### T5.5 The host scripts' low findings, and the record (#543, #545)

**Owner:** worker edits, the coordinator proves. #543's findings in `first-run.sh`, the
home pointer, the tape lock and `tapectl-op`, as its criteria list them. #545 is the
coordinator's: the three rulings adjusted while building have their item (261), and the two
stale texts are corrected.
**Fence:** `scripts/first-run.sh`, `scripts/install-systemd.sh`, `scripts/check-tapectl-op.sh`,
`contrib/systemd/tapectl-op.in`, and the doc #543 names as contradicting `tapectl-op`; the
`tests/first_run_*.rs` pins of a text this task changes.
**Done when:** each of #543's criteria is met or reported as needing the tape; `bash -n`
passes; the report lists what the coordinator must run to prove it (the rehearsal, and the
upgrade's backup and rollback once a 2.x predecessor exists).

### T5.6 The drill's event and its clock (#548's code; item 247)

**Owner:** worker. A command records that a heir drill was done (when, on which volume, by
which script version), and `audit` warns when the last one is older than a year or there
has been none. The event is a row the baseline holds. The command's spelling and the config key are
D18's, ratified (item 290). **Fence:** `src/cli/audit.rs`,
a new command module, `src/cli/mod.rs`, `src/main.rs` (the dispatch line),
`src/db/migrations/001_baseline.sql`, their tests, the generated pages.
**Done when:** `audit` warns with no drill recorded and with one older than the interval,
and is quiet after a fresh one, each shown by a test with its exit code; the command is in
`tests/dry_run_contract.rs` with one verdict.
### T5.7 Compression, proven for each algorithm carried (item 280, #565)

**Owner:** worker. **Depends:** T4.16. Compression is built in T4.5 (staging), T4.6 (the
MANIFEST), T4.8 (tapectl's restores) and T4.10 to T4.13 (the heir's path). This task proves
the feature whole, as the others of this stage are proven, and fixes what the proof finds.

For each algorithm carried: the three layers resolve it (`[defaults]`, an archive set, a
unit's dotfile, the most specific winning, and `none` when none is set); a compressed and
an uncompressed unit share one tape; `compression_level` is resolved by the same layers
and a level out of range is refused; a compressed unit continues on a second cartridge
and restores from both; a second copy of a compressed staging is the same slices, so a slice is replaced from
it by sha256; `volume rewrite` and a compaction carry a compressed unit unchanged; a
rebuild from tape restores the staging's algorithm; the capacity plan uses each slice's
size on tape and the early warning's rules hold with slices of unequal size.
**Fence:** tests only, in `src/policy/`, `src/volume/span_proofs.rs`, `tests/integration.rs`,
`tests/catalog_rebuild.rs`, `tests/format_v3.rs`. A source file is changed only for a defect
a proof shows: report the defect and the file first.
**Done when:** each sentence above has a test for each algorithm carried, on real stagings;
the suite is green.

---

## S6. Stage 6: reads and proof

**Starts when:** `layout-3` is merged to `master` (Stage 5 done, the pending list empty).
**Owner:** the coordinator, with fresh readers and the operator. No task here is handed to a
worker as a build task, except the fixes a read finds, which are issued and worked like
Stage 2's.

### T6.1 The adversarial reads (#551; items 248, 262)

One read-only agent for each area, given the code and the normative text and **not** given
the coordinator's view of what might be wrong. Each returns defects with file and line and a
concrete failing case. The areas:

1. the writer, the stream reader and the stream's checks (T4.1, T4.2)
2. the file table and the id map, the snapshot, staging on tar, compression (T4.3, T4.4,
   T4.5, T5.7)
3. what a tape carries, and a tape of another layout at contact (T4.6, T4.7)
4. tapectl's restores (T4.8)
5. `catalog rebuild` and the adoption of a lost write (T4.9)
6. the heir's script, File 1 and RECOVERY.md (T4.10 to T4.13), and the Heir Kit's sheet
   (T4.14)
7. spanning end to end (T5.1)
8. `copies_at` and the per-location writes; compaction; `volume rewrite` (T5.2, T5.3)
9. `tapectl setup` and the host scripts (T5.4, T5.5)

The coordinator verifies every finding of medium or high severity by reading the cited
lines, files what is real as issues under a new label, and works them as Stage 2 was worked.
**Stopping rule** (item 108's precedent): one read for each area. A fix of a high finding is
read once more by a fresh reader, and that is the last read of it. A finding that asks for a
design change goes to the CTO.

### T6.2 A fresh read of File 1 on a Layout 3 tape

A reader that has seen nothing of the project is given the first plaintext file of a real
mhvtl Layout 3 tape and the Heir Kit's sheet, and is asked to restore one unit with only
what they say. Its report is recorded on #548. SERIOUS findings go to the CTO; the rest are
fixed before the pins.

### T6.3 The Layout 3 pins

`tests/on_tape_golden.rs` gets its Layout 3 pins, **each in a commit of its own** that names
ADR-0012 item 243: File 0, `MANIFEST.toml` (a whole unit, a range block, a short seal), the
front index, the seal marker, the heir's script (the file in the tree, by sha256), File 1,
RECOVERY.md, the file table's text, the id map's text. No pin holds data-zone bytes made by
tar (the streams of two tar versions differ; #562).

### T6.4 The recorded tape (#552)

A small tape image written by the candidate, kept in the tree, restored and rebuilt from in
CI by every later build. The issue is the specification. The image is made through the real
staging path with a real tar, and the test names the tar version that made it.

### T6.5 The whole chain on the candidate

On the commit that is to be tagged, with nothing left to land:
`cargo test --no-fail-fast`; clippy for the gnu and musl targets and the `research` feature;
`check-docs.py`; the mhvtl gate, every check; `first-run.sh`'s rehearsal; the lifecycle suite
with `--all`; the erase proof; the plaintext scan; the short-seal and span acceptances with
`--mhvtl`; `mhvtl_lbp` on a TD6 drive; the heir suite in the `tapectl-u26` container as uid
1000; the pull request's CI. The run record names the age version and the tar version.
`upgrade-mhvtl.sh` has no 2.x predecessor and is not part of this chain (T3.4).

### T6.6 The real drive

Each of these needs the CTO's go-ahead, and runs on the production host with a scratch
static debug build and a scratch home, on a test cartridge:

1. the new payload's acceptance: a write, a full readback, tapectl's restore, the heir's
   script, a rebuild that agrees row for row. It includes a unit the size of the
   collection's largest, of real files: item 291 takes item 253's proof at that size from
   here, so a content mismatch or a tool's failure in this step reopens item 253
2. the short seal with Logical Block Protection armed, and `volume init`'s page clear (#532)
3. a unit continued at the real early warning with no opt-in, and the queue filled to it
   (items 288, 289; `scripts/span-acceptance.sh`)

The host needs a tar that meets the floor first (T4.14's step, run by the operator).

### T6.7 Release preparation

The version becomes 2.0.0. `docs/releases/2.0.0.md` is the operator's account. The handoff,
`CLAUDE.md`, the install guide, the quick start and the troubleshooting guide say what 2.0.0
is. #518 is closed as a Layout 2 defect (item 258(f)).

---

## S7. Stage 7: the acceptance run (items 263, 268)

**Owner:** the operator and the CTO. An agent prepares and records; it does not start any
step here.

1. **The tag.** `v2.0.0` is tagged on the commit of T6.5 when the build is complete and
   proven on test cartridges. CI builds its static binary. A tag is never pushed twice
   (#546).
2. **The fresh start** (item 244). The operator builds the host's tar (item 275), installs
   2.0.0 by `first-run.sh`, with a new catalog, new operator and tenant keys and new unit
   identities (the unit dotfiles deleted first). The escrow identity is adopted if T0.4
   passed. The Heir Kit carries the release's static binary.
3. **The cartridges.** L6-0001 and L6-0002 hold Layout 2 volumes: each is erased with
   `cartridge erase` and the typed label first (item 269). Both are written at real scale.
4. **The drill** (#548): a whole-tape restore at real scale with the kit's keys, and File
   1's steps followed on a real cartridge. The drill's event is recorded, which starts the
   yearly clock.
5. **The verdict.** If the drill passes, the cartridges are production and the on-tape
   authorization ends there (item 268). If it fails, they are test articles, the fix is a new
   release, the minor moves if on-tape bytes move, and the run is repeated by another fresh
   start with new keys and a new kit.
---

## Appendix A. Where every issue, item and rule goes

A task is done only when the issue's criteria it carries are met. An issue is closed by the
coordinator when every task named beside it has landed and the gate has run.

**The queue's issues** (label `review-2026-11-12`).

| Issue | Tasks | Issue | Tasks |
|---|---|---|---|
| #518 | T4.8, T4.11 (its criteria, carried by #555); closed as a Layout 2 defect at T6.7 | #542 | T4.4 |
| #519 | closed unbuilt (item 259) | #543 | T5.5 |
| #520 | T4.10 | #544 | T2.2 (parts 1, 3, 4), T2.4 (part 2) |
| #521 | T4.12 (the cartridge at the prompt); its `--swap-tapes` mode is gone (item 255) | #545 | T5.5 |
| #522 | T4.13 | #546 | T2.6 |
| #523 | T4.13 | #547 | T4.6 |
| #524 | T4.14 | #548 | T5.6 (the clock), T6.2, Stage 7 step 4 (the drill) |
| #525 | T4.10 | #549 | deferred (item 251) |
| #526 | T4.13 | #550 | T5.2 |
| #527 | T2.5 | #551 | T6.1 |
| #528 | T4.6, T4.11 | #552 | T6.4 |
| #529 | T2.3 | #553 | T4.10 |
| #530 | T2.4 | #554 | the epic; this plan |
| #531 | T2.1; its real-drive criterion in T6.6 | #555 | T4.5, and Stage 4 as a whole |
| #532 | T6.6 | #556 | T4.3, T4.8, T4.10 |
| #533 | T2.6 | #557 | T4.3, T4.6, T4.9 |
| #534 | T2.3 | #558 | T4.12 |
| #535 | T2.7 | #559 | Stage 7 (the order) |
| #536 | T2.2 | #560 | T4.7 |
| #537 | T2.2 | #561 | T4.1 (numeric owners), T4.4 (sockets), T4.10 (the work directory) |
| #538 | T2.4 | #562 | T1.1, and the tasks in the third table |
| #539 | T2.3 | #563 | T0.1, T0.2, T4.1, T4.2, T4.5, T4.13, T4.14 |
| #540 | T2.4 | #564 | T1.3, T4.3, T4.5, T4.6, T4.8, T4.9, T4.11 |
| #541 | T2.1 | #565 | T0.5, T0.6, T4.0, T4.5, T4.6, T4.8, T4.10, T4.13, T4.14, T4.16, T5.7 |
| #566 | closed: slices stay (item 282) | #567 | T0.7, T4.2, T4.3, T4.5, T4.6, T4.8 to T4.13, T4.16, T5.1, T5.3 |
| #569 | T4.17 (item 297) | #570 | T4.5b |

**ADR-0012's items from 239.**

| Item | Tasks | Item | Tasks |
|---|---|---|---|
| 239 format changes now | the plan | 260 a range is a source | T4.3, T4.8, T4.11 |
| 240 slices and their rules stay | T4.5 (the cut), T5.1; section 5 | 261 three behaviours as built | T5.5 |
| 241 a cut of a tar stream (its "plain cut" amended by 283) | T4.5, T4.15 | 262 the drill and the reads gate | T6.1, Stage 7 |
| 242 no reverse compatibility | Stage 3 | 263 the first run is the acceptance | Stage 7 |
| 243 the on-tape authorization | T4.0, T6.3 | 264 no zero-fill; whole members by byte range in tapectl's reader | T4.3, T4.8, T4.10 |
| 244 a fresh start | Stage 7 step 2 | 265 the table in the catalog | T4.3, T4.6, T4.9 |
| 245 the two cartridges | Stage 7 step 3 | 266 a start on any tape of a span | T4.8, T4.12 |
| 246 spanning from the first day | T4.12, T5.1 | 267 the heir's span restore streams | T4.12 |
| 247 three behaviours | T2.3, T5.6 | 268 the acceptance run's order | Stage 7 |
| 248 gated on total completion | Stages 6 and 7 | 269 a tape of another layout | T4.7, Stage 7 step 3 |
| 249 four confirmations | T2.2, T2.3, T2.4, T2.5 | 270 numeric owners, no sockets | T4.1, T4.2, T4.4 |
| 250 no notification built in | T2.7 | 271 the heir's work directory | T4.10 |
| 251 a second drive does not gate | #549 deferred | 272 the id map | T4.3, T4.5 |
| 252 no file left out by default | T4.4 | 273 the standard is not lowered | T0.1; section 5, rule 10 |
| 253 GNU tar writes the stream | T0.3, T4.1, T4.2, T4.5, T6.6 (item 291) | 274 the floor binds every writer | T4.1, T4.2, T4.5, T5.4 |
| 254 the file table | T4.3, T4.6 | 275 the host's tar is built | T0.2, T4.14 |
| 255 the heir has one repair | T4.8, T4.11 | 276 the map in each tenant envelope | T4.6, T4.9 |
| 256 hard links stay links | T4.2, T4.3, T4.8, T4.11 | 277 what the map holds | T4.2, T4.5 |
| 257 two places held by `audit` | T5.2 | 278 a restore shows the map | T4.8, T4.11 |
| 258 what "simplify" removes | (a) T4.6; (b) T3.1; (c) T3.4; (d) T3.2, T3.3; (e), (f) T6.7 | 279 the plan in stages | this file |
| 259 #519 closed unbuilt | done | 280 compression, optional, for select archives | T0.5, T4.5, T4.6, T4.8, T4.10, T4.13, T4.16, T5.7; section 5, rule 11 |
| | | 281 the design is not sized to the present corpus | T0.3; section 5, rule 12 |
| 282 slices stay | T4.5 | 286 tapectl itself compresses | T0.6, T4.5 |
| 283 a slice ends at a member's start | T0.7, T4.2, T4.3, T4.5, T4.6, T4.8, T4.10 to T4.13; section 5, rules 4 and 13 | 287 an archive may set a level | T4.5, T5.7 |
| 284 each slice compressed alone | T0.5, T4.5, T4.8, T4.10; section 5, rule 11 | 288 every unit continues at the early warning | T4.6, T4.9, T4.12, T5.1, T5.3, T6.6 |
| 285 gzip, xz and zstd are carried | T0.5, T0.6, T4.0, T4.5, T4.15 | 289 the queue is filled to the warning | T5.1, T6.6 |
| 297 a name that is not UTF-8 is carried by its bytes | T4.17 | | |

**#562's rules for the format document.** Each is written into the format document by T1.1
and built where shown.

| Rule | Built in |
|---|---|
| The list is NUL-separated, `--null --verbatim-files-from`, names prefixed `./` | T4.1 |
| `exthdr.name` is fixed | T4.1 |
| No pin or fixture holds data-zone bytes across tar versions | T6.3; section 5, rule 8 |
| An ACL is stored once | T0.1 (which record), T4.1 |
| tar's exit 1 and 2 refuse the stage; tapectl's own before-and-after check decides | T4.1, T4.5 |
| A stream hash that differs from the version's recorded sha256 is refused, but for the dotfile | T4.5 |
| The seccomp filter goes | T4.5, T4.15 |
| `slice_size` is a multiple of 512 | T4.5 |
| A member starts at its first extension header; size from the pax record | T4.2 |
| The stream's members are the snapshot's list, in order, with those sizes | T4.2, T4.5 |
| Rows in stream order; a folder is two ranges | T4.3 |
| Every tape holding a slice of a staging carries its whole table | T4.6, T5.1 |
| The table is part of what the seal withdraws; "this tape holds F" | T4.3 (`held_whole`), T5.1 |
| A hard link's row, its check and its repair | T4.2, T4.3, T4.8, T4.11 |
| `index/paths.tsv` states no sha256 for the unit dotfile | T4.6 |
| Each range is its own pipeline | superseded by item 283: the heir's script gives tar whole slices (D10) |
| A slice that fails ends that tar run; the next starts after it | amended by item 283: a bad slice of whole members is left out and the run goes on; only a bad piece of a long member's run ends the run (D10; T4.10) |
| File 1 says which tars restore what | T4.13 |
| Rule S2, rule 6 and `may_end_early` without the `short-seal` entry | T1.1, T4.6 |
| The magic and the layout version both move | T4.6 |
| The upgrade harness has no 2.x predecessor yet | T3.4 |

## Appendix B. What waits on the CTO

1. **The go**, stage by stage. Nothing else is open before it. T0.3 alone does not wait
   for it (item 291). On the go: Stage 0's other measurements (T0.1, T0.2, T0.5, T0.6,
   T0.7) and Stage 2's first wave start; the measurements run one at a time, T0.7 and
   T0.1 first. Stage 3 follows Stage 2. Stage 1 is written when T0.1 and T0.7 are in,
   its section on compression when T0.5 and T0.6 are.
2. **A measurement that fails its rule.** T0.1 (neither ACL record is restored by GNU tar
   1.30 and bsdtar), T0.3 (item 253), T0.5 (item 285), T0.6 (item 286's fallback), T0.7
   (item 283). Each task says what goes to the CTO and stops there.
3. **A question a task stops on**: anything that would change bytes on tape, the schema
   or a command's spelling and is not decided by ADR-0012, D1 to D22 or the format
   document.
4. Each real-drive run of T6.6, and the tag.
5. A SERIOUS finding of T6.1 or T6.2, and a finding that asks for a design change.
