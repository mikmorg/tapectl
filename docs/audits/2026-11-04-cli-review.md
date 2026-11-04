# The CLI review — 2026-11-04

**What.** A review of tapectl's command line for usability, legibility and current CLI practice,
asked for by the CTO on 2026-11-04 during the grilling of handoff (i)'s accrued decisions:
"now is a good time to review our CLI commands, focusing on the most common/important ones".
Its findings were put to the CTO as questions Q19 to Q33 and ruled the same day: ADR-0012
items 198 to 221 (2026-11-04). They are 1.9.0's work, and handoff (i)'s copy 1 waits for it
(item 205).

**Method.** Three agents ran against the debug build of 1.8.1 at `d1299da8`
(`tapectl 1.8.1 (v1.8.1-4-gd1299da8, 2026-11-03)`), each in its own throwaway home under
`/scratch` on vm-desk1, never against a production home:

- **A, behaviour:** a new operator's journey through the non-tape commands (init, tenants,
  locations, a collection, snapshot and stage, reports, audit, catalog, backup, the Heir Kit,
  config), with deliberate mistakes to test the error messages.
- **B, the surface:** the command tree and its conventions, from `--help` and the code:
  naming, flags that repeat a positional, which commands honour `--json` and `--dry-run`,
  help text, exit codes, output, and every CLI word against `CONTEXT.md`.
- **C, the tape commands,** on mhvtl's drive A2 (an LTO-8 emulation, resolved by-id): the
  copy loop (`volume write --init --full-confirm --move-to`), verify, an interrupted copy and
  `volume resume`, `cartridge erase` with its typed consent, restores, the heir's three File 1
  commands in a fresh home, and `collection run`. It was added when A and B came back without
  having run a tape command.

**The focus list** was counted, not chosen: every `tapectl …` and `tapectl-op …` line in
`docs/runs/`, `docs/handoff.md`, `docs/operator-guide.md` and `docs/install.md`, by subcommand.
The most used were `config check` (25), `volume write` (24), `audit` (24), `volume verify`
(21), `volume init` (18), `cartridge erase` (18), `status` (17), `init` (14), `staging clean`
(12), `volume resume` (11), `db backup` (11) and `restore unit` (10); the systemd units add
`audit`, `db backup`, `drive poll`, `report health` and `report verify-status`.

**How to read the reports below.** The briefs asked for a non-breaking path for every
change, so the reports propose aliases and new leaves beside old spellings. The CTO then ruled
that a renamed spelling is removed at once and refused by name (items 203, 209), except the
three commands File 1 prints. Read each alias proposal as a rename with a refusal. Before the
findings went to the CTO, the coordinator checked four against the code or by running them: the
half-registered unit, `audit --action-plan`'s recipe, `unit check-integrity`'s exit code and the
completions panic. The fifteen defects are item 220. The reports are kept as the agents wrote them. Their `/scratch`
paths (the throwaway homes, `journey.log`, `phaseB.out` and `phaseC.out`) were on vm-desk1 at
the time and were not kept.

## tapectl CLI review, half A: behaviour (the operator journey)

Binary `/scratch/cargo-target/debug/tapectl` 1.8.1 (v1.8.1-4-gd1299da8). Every call ran through a wrapper that
forces `TAPECTL_HOME` under `/scratch/tapectl-cliux-a` and logs `rc<TAB>home<TAB>argv`
(`/scratch/tapectl-cliux-a/journey.log`). No tape device was opened: the one backend I configured points at a path
that does not exist (`…/no-such-nst`).

Homes used:
- `home`: the journey;
- `home-bad` and `home-ro`: copies of it, for the error cases;
- `empty`: never initialised.

Scope limits: nothing here touches tape, so I reached no ADR-0008 Tier-2 prompt and do not judge prompt wording.
`collection sync` asked nothing with or without `--yes`. "Inferred" marks what I read in code or docs but did not run.

### 1. The journey as run (exit code first)

**Step 1.**
- `0 init` (no `--operator`: my uid is a login account), `0 tenant add alice`, `0 location add onsite`,
  `0 location add offsite --description "bank box"`, `0 location list`.
- `0 config check`, `0 status`, `0 host check`.

**Step 2.** Tree: `src/photos/{2019,2020,2021}`, plus `2022 trip/` (with a space) and a loose `loose.txt`.
- `0 config add collections name=photos root=…/src/photos tenant=alice`.
- Accepted with `--dry-run`, each exit 0: root `…/src/nope`, `tenant=alcie`, `root=relative/path`,
  `name="my movies"`.
- Refused: `2` for `depth=2`.
- `1 collection sync --dry-run`, `1 collection sync --yes`, `1 collection status`.
- `2 collection plan` (no backend), `2 collection plan --generation LTO-6`.
- `0 unit list`, `2 unit status`, `0 unit status photos/2019`, `2 unit status 2019`.

**Step 3.**
- `0 snapshot create photos/2019`, `0 stage create photos/2019`, `0 staging status`.
- `2 snapshot create photos/2020 photos/2021` (it takes one name).
- `2 stage create photos/2020` (not snapshotted yet), then `0 snapshot create` ×2 and
  `0 stage create photos/2020 photos/2021`.
- `0 report pending`, `0 report needs`, `2 audit`, `2 audit --action-plan`, `0 status`.
- `2 volume plan` (prints the plan, then refuses: no backend).
- `2 collection run --dry-run` (needs `--collection`),
  `2 collection run --collection photos --label L6-0001 --dry-run` (no backend).

**Step 4.**
- Catalog and reports: `2 catalog ls`, `0 catalog ls photos/2019`, `0 catalog search img_2`, `0 catalog search '*.txt'`,
  `0 catalog locate photos/2019`, `0 catalog stats`, `0 report summary`.
- Backup and kit: `2 db backup`; `0 db backup --to …/backup/tapectl.db` ×2 (the second silently overwrote);
  `2 db backup --to …/backup/`; `2 db backup --to …/nodir/x.db`; `0 db fsck`; `2 key escrow-kit`;
  `0 key escrow-kit --out …/kit`.
- Config: `2 config get defaults.slice_size`, `2 config show defaults.slice_size`, `0 config show`,
  `0 config add backends.lto name=lto6 device_tape=…/no-such-nst generation=LTO-6`.
- `post_warning_bytes`: `2 config set post_warning_bytes 99008M`, `2 config set backends.lto.post_warning_bytes 99008M`,
  `0 config set 'backends.lto[lto6].post_warning_bytes' 99008M` (then `0 config remove`).
- Plan with the fake drive: `2 collection plan` (sg not derivable),
  `0 config set 'backends.lto[lto6].device_sg' …/no-such-sg`, `0 collection plan` ("nothing pending").
- After adding `2023/` and renaming the folder to `2022_trip/`: `1 collection sync`, `1 collection status`,
  `0 collection plan`, `0 report dirty` ×2 (before and after adding a file to 2019).
- `0 staging clean`, `2 staging clean --dry-run`, `0 report fire-risk|verify-status|copies`, `0 volume list`,
  `0 cartridge list`.

**Step 5, mistakes.**
- Typos: `2 volum list`, `2 unit lst`, `2 report need`, `2 unit list --tenant alcie`, `2 unit status 2019`.
- Unknown unit: `70 audit --unit photos/nope`, `70 audit --bogus`, `0 report copies --unit nope` (no output),
  `2 catalog ls nope`, `2 snapshot create nope`.
- Bad config: `2 config set defaults.slice_sise 2G`, `2 config set defaults.min_copies two`.
- `2 init` on an initialised home.
- `empty` home: `2 status`, `2 config check`, `2 unit list`, `70 audit`, `2 report summary`, `2 collection status`.
- `--json` on `unit list`, `config check`, `audit`, `staging status`, `status`, `report needs` and
  `collection sync/status/plan`; `2 unit status nope --json` (the error is plain text).
- `home-bad`:
  - a root that does not exist: `1 sync`, `2 status`;
  - tenant `alcie`: `1 sync`, `1 status`;
  - a tree with permission problems: `1 sync`, `2 snapshot create ro/b`.
- `home-ro`, the same tree from a clean copy: `1 sync --json`, `0 unit status ro/a`, `1 sync` after fixing the modes,
  `0 snapshot create ro/a`, `2 unit init …/src/ro/a`.

### 2. Findings

#### F1. `collection sync` half-registers a unit it cannot write a dotfile for, and nothing repairs it (HIGH)
`src/ro/{a (mode 555), b, c (mode 000)}`, `dotfiles` default true. One sync in a clean copy of the home printed:
`collection "ro": 1 created … 3 pending` / `error: …/src/ro/a: Permission denied (os error 13)` /
`error: …/src/ro/c: Permission denied (os error 13)`.
- `unit list` then shows **all three** units, and `unit status ro/a` works.
- After I fixed the modes (the documented fix is the ACL grant), every later sync says
  `error: …/src/ro/a: nested unit detected: …/src/ro/a is inside existing unit "ro/a" at …/src/ro/a`.
- `unit init …/src/ro/a` refuses the same way.
- No dotfile is ever written. The unit stays identified by its path while the config says `dotfiles = true`.

*Inferred* from `src/collection/sync.rs` ~264-284: `init_unit_with_config` inserts the row, the dotfile write fails,
and nothing rolls the row back. This is what happens whenever a sync runs before the service user's ACLs are
granted.

**Change:**
- Insert the row and write the dotfile in one transaction.
- When a row already exists at this exact path without a dotfile, write the dotfile instead of reporting it as
  nested.
- Name the operation and the fix: `cannot write .tapectl-unit.toml in <dir> as user <name>: grant
  setfacl -m u:<name>:rwX <dir>, or set dotfiles = false`.
- Count `created` from the rows actually inserted.

**Blast radius:** binary only ("nested unit detected": src 1, docs, tests and scripts 0).

#### F2. Adding a collection: the command exists, nobody can find it, and it checks nothing (HIGH)
`config add collections name=… root=… tenant=…` works. Its only documentation is configuration.md:185. The
`[[collections]]` section, the operator guide and `collection --help` (sync/status/plan/run) never mention it, and
with nothing configured `collection` says `add a [[collections]] block to config.toml` (src/cli/collection.rs:296).

These all printed `would add …` with exit 0, and `config check` then said `config: valid`:
- a root that does not exist;
- a relative root;
- tenant `alcie`;
- the name `my movies`;
- a second collection over the same root.

What followed:
- **tenant `alcie`:** sync printed `error: …/x1: tenant not found: alcie` once per folder, and `collection status`
  showed that collection as clean (`0 pending, 0 dirty, 0 missing, 0 under-copied`).
- **missing root:** `collection status` exits 2 for **every** collection
  (`error: collection "movies": root … does not exist`).

**Change:** add `tapectl collection add <NAME> <ROOT> --tenant T [--unit-depth N] [--exclude G]… [--archive-set S]
[--no-dotfiles]`. It:
- refuses a missing or relative root, an unknown tenant or archive set, a bad name and an overlapping root;
- then runs a sync dry run;
- prints the ACL lines the service user needs (operator-guide.md:97-100, filled in with the real user and ancestors).

Also add `collection remove`. Have `config check` check tenant and root existence, and have `collection status`
report a broken collection per collection instead of aborting.

**Blast radius:** additive ("config add collections": docs 3, src 1). `config add collections` stays.

#### F3. "pending" means three things, and staged units drop out of the collection loop (HIGH)
After staging 2019-2021 by hand:
- `collection status` said `0 pending, 0 dirty, 0 missing, 3 under-copied`;
- `collection plan` said `nothing pending`;
- `report pending` said `stage sets that still owe a copy:` and listed the same three units.

After I dirtied 2019, `collection status` said `2 pending, 1 dirty` while `collection plan` batched **3** units.

CONTEXT.md defines Pending as never-archived **or** dirty, and its _Avoid_ list includes "pending as a synonym
for never-archived alone", the sense `collection status` uses. `report pending` uses a third sense (stage sets).
`report dirty` prints `new — never archived`, and "new" is also an _Avoid_ word. Nothing on the collection path
says the staged units need `volume write`.

**Change:**
- `collection status`: `3 pending (2 never archived, 1 dirty), 3 staged awaiting a write, …`.
- `collection plan`: name the staged-but-unwritten units and the command that writes them.
- `report pending`: keep the name, title the output "staged, awaiting copies".
- `report dirty`: say `never archived`.

**Blast radius:**
- text: `pending, N dirty` in docs 3, `new — never archived` in src 2;
- JSON key `"pending"` (tests 3, src 18): keep it with its current meaning and add `never_archived` and
  `staged_unwritten` beside it.

#### F4. `audit --action-plan` gives a staged unit a fix that fails at step 2 (HIGH)
For a freshly staged unit, audit printed:
`[copy_count] photos/2020 … fix: tapectl snapshot create photos/2020 && tapectl stage create photos/2020 &&
tapectl volume init <LABEL> && tapectl volume write <LABEL>`.
Running that fix gave `unit "photos/2020" is unchanged since v1; no snapshot created` and then
`error: no unstaged snapshot …`, exit 2.

Cause (read in code): `additional_copy_action` falls back to that recipe when no snapshot is `current`
(src/cli/audit.rs ~610-622). `no_archive_action` already handles the staged case.

Other problems in the same output:
- The same unit also gets `[no_archive] … no current snapshot or tape copies`. `current` is an internal status
  (sealed on tape), so the line reads as "never snapshotted".
- Every zero-copy unit is therefore reported twice.
- The recipe repeats five times, once per unit, and uses the long form rather than
  `volume write <L> --init --full-confirm --move-to <loc>`.
- Plain `audit` never mentions `--action-plan`.

**Change:**
- Branch the `copy_count` fallback on a live stage set.
- Fold `no_archive` into `copy_count` at zero copies.
- Word it `never written to tape (v1 staged, awaiting a write)`.
- Collapse identical actions into one line that names N units.
- End plain `audit` with a pointer to `--action-plan`.

**Blast radius:** message text (docs 1, src 1). The `no_archive` id, which a test at audit.rs:3818 pins, and the
JSON `check` ids stay.

#### F5. The home's source is invisible, and the not-initialised hint can create a second archive (HIGH)
`--home` help says `Defaults to ~/.tapectl`. It leaves out `/etc/tapectl/home` and the precedence order.

`status` and `config check` show the home only indirectly (`session logs: …/home/logs`, the staging path). Nothing
says why that home was chosen.

On an empty or missing home:
- every catalog command says `error: tapectl is not initialized — run \`tapectl init\` first` and names no path
  (main.rs:331);
- `status` blames ops-group permissions: `cannot read the session logs … They belong to the user tapectl runs as …`.

Why the CTO types `--home` (*inferred* from startup.rs and install-systemd.sh):
- `tapectl-op` sets `TAPECTL_HOME`, so it never needs `--home`.
- As root the pointer is never read, so the home falls to `/root/.tapectl`.
- Any `~/.tapectl` of the invoking account wins over the pointer.

So on home2, "not initialized" most likely means "wrong home", and following the hint creates a fresh archive.
handoff.md:677's `sudo -u tapectl -H tapectl --home …` looks like habit rather than need (*inferred*: the pointer
exists by then).

**Change:**
- `config check` and `status` print `home: /srv/…/home (from /etc/tapectl/home)`.
- The not-initialised error names the path and the rule that chose it.
- When euid is 0 and the pointer exists, it says `as root /etc/tapectl/home is ignored — use tapectl-op`.
- It offers `init` only for creating a new archive.
- `--home` help lists the whole order.

**Blast radius:** "not initialized" in docs 4, tests 1, src 2. No flag changes.

#### F6. Planning needs a drive it never touches, and `config check` doesn't warn (HIGH)
With no backend:
- `collection plan`, `collection plan --generation LTO-6` and `volume plan` all print `no LTO backend configured …
  Add a [[backends.lto]] section to your tapectl config (by default ~/.tapectl/config.toml)` plus the pasted
  template. The file was really `/scratch/…/home/config.toml`, and `tapectl backend add` goes unmentioned.
- `volume plan` prints the whole plan, then exits 2.
- `config check` says `config: valid` and never says that writes will refuse.

With a backend whose sg node can't be derived, the plans print `refusing to resolve an LTO backend for a write: …`.
A plan is not a write. `--generation`'s help promises "No cartridge need be loaded", and `config check` calls the
same condition `normal if the drive isn't attached`.

**Change:**
- Plans take capacity from `--generation`, or from the backend's `generation`, without resolving the device or sg.
- Every "no backend" error prints the real config path and leads with `tapectl backend add …`.
- `config check` notes `no [[backends.lto]]: volume write and collection run will refuse`.

**Blast radius:** binary.

#### F7. Config keys are hard to discover; `post_warning_bytes` is the worst case (HIGH)
What happened:
- **Bare key:** `config set post_warning_bytes 99008M` printed a TOML excerpt of a file the operator never wrote,
  then `unknown field post_warning_bytes, expected one of dar, backends, …`, and repeated it as
  `- unknown key: post_warning_bytes`.
- **List path:** `backends.lto.post_warning_bytes` → `name one entry: backends.lto[<name>] or backends.lto[<index>]`,
  without naming the only entry, `lto6`.
- **Typo:** `slice_sise` lists 16 keys instead of saying "did you mean slice_size".
- **Bad value:** `min_copies two` → `expected i32`.
- **No `config get`:** typing it gives clap's tip `a similar subcommand exists: 'set'`. `config show` takes no key
  and prints the file, not the effective values (the collection's `unit_depth` and `dotfiles` don't appear).
- **Unset, unmentioned:** `init`'s commented backend block omits `post_warning_bytes`, and `config check` says
  nothing when it is unset. handoff.md:677 had to spell the command out.

**Change:**
- Resolve a bare leaf against the schema and suggest the full path (`backends.lto[lto6].post_warning_bytes`).
- With exactly one backend, accept `backends.lto.KEY`.
- Suggest by edit distance, and name types plainly ("a whole number").
- Drop the rendered-file excerpt from set/add errors.
- Add `config get KEY [--json]`: the effective value and its source.
- `config check` notes the unset key and points to `scripts/lto-profile.sh`.
- Add the commented line to the init template.

**Blast radius:** additive. `config show` stays (docs 16, tests 5).

#### F8. Exit codes and `--json` errors are not scriptable (MED)
- An unknown unit exits 2 (`catalog ls`, `snapshot create`, `unit status`) but 70 in `audit`. A usage error exits
  2, but 70 in `audit`. Both 70s are documented in audit's own help, so each command is consistent with itself;
  the commands are not consistent with each other.
- `report copies --unit nope` prints nothing and exits 0, so a typo reads as "no data".
- `collection sync` and `status` exit 1 both for errors (bad tenant, permission denied) and for a stray
  `loose.txt`, so one loose file pins the exit code at 1.
- Exit 2 also means an audit violation and `volume write`'s "nothing written".
- Under `--json`, errors are plain text on stderr and stdout is empty (`audit --json --unit nope`,
  `unit status nope --json`).

**Change:**
- An unknown `--unit` or `--tenant` is an error everywhere.
- Sync and status exit 1 for warnings and 2 for errors.
- Under `--json`, errors go to stdout as `{"error": "...", "exit_code": N}`.
- Publish one exit-code table in `docs/cli/README.md`.

**Blast radius:** binary and docs. I did not count the tests that pin exit codes.

#### F9. `collection run` lacks what `volume write` has (MED)
`collection run` requires `--collection` even when only one is configured. Its `--label` must already be
`volume init`-ed, and it has no `--init` or `--move-to`. The documented one-command copy is
`volume write L --init --full-confirm --move-to X`, so the day-to-day loop is `volume init`, then
`collection run …`, then `volume move`.

**Change:** take the collection as a positional argument, defaulting when only one is configured, and pass
`--init` and `--move-to` through.

**Blast radius:** "collection run --collection" in scripts 5, docs 3. Keep the flag beside the positional
(clap `conflicts_with`).

#### F10. Sync's dry run predicts a different result, and collection warnings omit the fix (MED)
- **Dry run vs real run:** the dry run said `4 created … 0 pending`. The real run said `3 created … 3 pending` and
  `error: …/2022 trip: invalid unit name "photos/2022 trip": the character ' ' is not allowed`. *Inferred*
  (sync.rs ~276): names are validated only on the real path.
- **Warnings without a fix:** `OUTSIDE ANY UNIT (not archived): 1 entry under the root belongs to no unit /
  loose.txt (file)` (exit 1 on every run) and the invalid-name error never say what to do. configuration.md does
  (rename, or add the name to `exclude`).

**Change:**
- Validate names in the dry run and say `would create 3; 1 refused`.
- Append `move it into a unit folder, or: tapectl config add 'collections[photos].exclude' loose.txt`. I ran that
  command with `--dry-run` and it was accepted. That it silences the report is configuration.md's claim; I did
  not run it.

**Blast radius:** "OUTSIDE ANY UNIT" in docs 3, src 2.

#### F11. Output legibility: tables, sizes, timestamps, plurals (MED)
- Boxed tables rule every row, so 3 units take 11 lines. `unit list` is 93 columns with short scratch paths, and
  `COLUMNS=80` changes nothing.
- `staging status` has a `Size (MiB)` column showing `0` for 109 KiB.
- Three timestamp formats:
  - `2026-11-04 03:34:07`, with no zone (unit, tenant and staging tables, JSON `created_at`);
  - `2026-11-04T03:33:37+00:00` (catalog ls);
  - `…T03:34:16.205Z` (status).
- One cartridge is `2.50 TB` in `config check` and `2.21 TiB budget` in `collection plan`. ADR-0012 says
  capacities are decimal.
- Plurals: `1 slices`, `2 copy/copies`, `batch(es)`, `cartridge(s)`. escrow-kit prints `524742 bytes`.

**Change:** borderless, aligned, width-aware tables; human sizes; one timestamp format with a zone; decimal
capacities; real plurals.

**Blast radius:** "copy/copies" in src 19, scripts 3, docs 3, tests 1; "Size (MiB)" in src 1. Check scripts that
grep this text.

#### F12. `status` is not the overview, and its history is noisy (MED)
- On a fresh home, `status` prints only `running: nothing`. The overview with next-step hints is `report summary`
  (`Staging: 3 stage set(s) held, 3 still owe a copy (\`tapectl report pending\` lists them)`).
- `status` repeats a whole failure message. For `stage create photos/2020 +1 more`, it lists
  `check / archive / recheck / catalog / finalize` twice and never says which unit each belongs to.
- In `status --json`, `"outcome": "failed — no unstaged …"` mixes an enum with prose, and `phases` are
  preformatted strings.

**Change:**
- When the catalog is readable, `status` ends with the `report summary` lines.
- Phases are labelled per unit and shown only with `-v`.
- JSON gets `outcome: "failed"`, a separate `error`, and numeric phase fields.

#### F13. Looking up a unit or tenant (MED)
- `unit status 2019` → `unit not found: 2019`, with no suggestion of `photos/2019`.
- `unit list --tenant alcie` → `tenant not found: alcie`, with no list of tenants. The archive-set error does list
  the registered names.
- `unit status` with no name refuses.
- `unit status photos/2019` shows UUID and path, but no version, stage state, copies or next step. To see those
  the operator needs `catalog locate`, `report needs` and `staging status`.

**Change:**
- Did-you-mean suggestions and registered-name lists.
- `unit status` adds `Latest: v1 staged 03:34 UTC; copies 0 of 2 (2 places); next: volume write`.

**Blast radius:** additive (`unit status` appears 28 times in docs).

#### F14. One fact, four wordings, and `min_locations` is invisible (MED)
The same shortfall reads:
- `audit`: `has 0 copies, needs 2`;
- `fire-risk`: `ZERO COPIES (needs 2)`;
- `copies`: `0 copies, 0 locations [tapes holding any version: -]`;
- `needs`: `0 of 2 copies, 2 short` / `offsite: no target, 0 copies`.

None of these says that `min_locations = 2` needs two **different** places. `fire-risk` exits 0 where `audit`
exits 2.

**Change:** one shortfall phrase everywhere (`coverage::Shortfall::describe`, which `collection` already uses),
including `2 places`.

#### F15. Smaller items (LOW)
- **One unit at a time:** `snapshot create` takes one NAME (`unexpected argument 'photos/2021'`), while
  `stage create` takes several. Accept `NAME…` and `--all-dirty`.
- **`staging clean`:** it refuses `--dry-run`, and a real run prints
  `cleaned 0 stage set(s), 0 files removed, 0 B freed` while 3 sets stay held. List `kept: photos/2019 v1
  (0 of 2 copies)` from the gate's own reasons.
- **Noise:**
  - The ssh/tmux warning fires on a 0.8 s `stage create` of 109 KiB.
  - A tenant mismatch prints a tracing line
    (`…Z WARN tapectl::collection::sync: … unit=photos/2019 tenant=alice collection_tenant=alcie`) and then the
    formatted warning, five times each. Keep tracing at debug when a user-facing line exists.
- **`init`:**
  - No next steps. Add `next: tenant add, location add, backend add, collection add, key escrow-kit`;
    `key escrow-kit`'s `still to do, and only you can do it:` is the model.
  - Piped into `| head`, it still printed the full `AGE-SECRET-KEY-1…` to stdout. Printing it once is ADR-0005 and
    stays; add a stderr warning when stdout is not a terminal.
- **Flags for the only operand:** `db backup --to FILE` and `key escrow-kit --out DIR` each take one argument.
  `--to <dir>/` is refused instead of writing `<dir>/tapectl-<UTC>.db`, and an existing backup is overwritten
  without a word. Add a positional beside the flag and accept a directory. Blast: "db backup --to" in scripts 7,
  docs 19, tests 2, contrib 3; "escrow-kit --out" in scripts 2, docs 15. The flag stays.
- **JSON shape:** `unit list --json` dumps the database row (`tenant_id: 2`, `archive_set_id`), not the tenant
  name. `config check --json` and `audit --json` print one line where the others pretty-print. JSON timestamps
  have no zone.

**What worked well:**
- clap's did-you-mean for subcommands.
- `db backup`'s refusals say what to do.
- `stage create`'s missing-snapshot error names both remedies.
- `config set/add/remove` have dry runs, write atomically and report `no change`.
- `report summary` and `key escrow-kit` end with next steps.
- `host check` is aligned and fits in 80 columns.

### 3. Ranked top 10

1. **F1:** a sync before the ACL grant leaves units that every later sync calls "nested", with no repair. Fix the transaction and the message.
2. **F2:** `collection add`, with validation and the ACL lines printed. The CTO asked for it, and `config add collections` checks nothing.
3. **F4:** `audit --action-plan`'s fix for a staged unit fails at its second command. Also merge the duplicate `no_archive` warning.
4. **F3:** "pending" has three meanings, and staged units vanish from `collection status` and `plan`. Align with CONTEXT.md.
5. **F5:** name the resolved home and its source. "Not initialized — run init" with no path is dangerous on a host with a pointer.
6. **F7:** add `config get`, leaf-key suggestions (`post_warning_bytes`), did-you-mean and the template line.
7. **F6:** planning should need no drive. Errors should name the real config path and `backend add`, and `config check` should warn when there is no backend.
8. **F8:** consistent exit codes, JSON errors under `--json`, and no silent exit 0 for an unknown `--unit`.
9. **F9:** `collection run` gets a positional collection plus `--init` and `--move-to`, as `volume write` has.
10. **F10:** sync's dry run must agree with the real run, and each collection warning should carry its fix.

## tapectl CLI review B: the surface and its conventions

Binary: `/scratch/cargo-target/debug/tapectl` = `tapectl 1.8.1 (v1.8.1-4-gd1299da8, 2026-11-03)`; repo HEAD 2ea40aed (only CONTEXT.md differs).
What I ran (all with `TAPECTL_HOME=/scratch/tapectl-cliux-b/home`, which **does not exist**; I did not create it): `--help`/`-h` on the root and about 12 subcommands, `--version`, `completions`, `host check [--json]`, and `status`/`audit`/`report summary`/`volume verify` (all refused "not initialized"). No command produced catalog data, so output shapes come from reading code. Peer behaviour (gh, restic, borg, zfs, git, cargo) is cited from upstream documentation, not run here. Anything marked *inferred* was not proven by a run or a read.
**Frozen spellings:** all three File 1 commands exist in this binary's clap tree (`init --operator --escrow-public-key`, `catalog rebuild --from-volume --device --key`, `restore unit --unit --device --to --key`). No proposal changes any of them; F8 and F10 add spellings beside them.
**About aliases:** a clap `visible_alias` renames only within its own parent command (`unit info`, `catalog list`, `report reclaimable`, `--systemd-unit`, `--destination`/`--to`). An alias that crosses groups (`stage clean`, `drive add`, `unit export`, `volume import`) needs a new leaf that calls the existing code. Every new leaf fails `tests/dry_run_contract.rs` until it has a TABLE verdict, and docs/cli and docs/man must be regenerated.

### 1. Tables

#### 1a. Global flags on the focus commands
Sources: the `--dry-run` column is `tests/dry_run_contract.rs` TABLE, which gives a verdict for every one of the 124 leaves (43 honour, 55 read-only, 26 refuse). The `--json` column is a per-arm scan of every dispatcher. I read the arms that do not use `--json`. `--yes`: `main.rs:524-656` passes `cli.yes` to only 8 dispatchers (unit, collection, snapshot, stage, volume, cartridge, db, quick-archive).

| command | --json | --dry-run | --yes reaches it | note |
|---|---|---|---|---|
| config check / show | yes | read-only | no | check exits 0/2 only |
| config set | yes | honours | no | |
| init | yes | honours | no | |
| status | yes | read-only (not passed) | no | |
| host check | yes (single-line JSON) | read-only | no | runs before init |
| audit | yes | read-only (not passed) | no | |
| volume init | yes | **refuses** | passed, unused (its `--force` is a File 0 override) | |
| volume write / resume | yes | **refuses** (points at `volume plan`) | yes | |
| volume verify | yes | **refuses** (exit 3) | no | |
| volume move | yes | honours | no | |
| volume identify | **ignored**: prints File 0 text (volume.rs:2406-2429) | read-only | no | 10 references in scripts/ |
| cartridge erase | yes | honours | by design no (typed `--confirm-label`) | |
| staging clean | yes | **refuses** | **no**: `staging::run` gets no `yes` | only `--force` |
| stage create | yes | refuses | yes | |
| snapshot create | yes | refuses | passed, unused | |
| restore unit | yes | honours (its own `--dry-run`, sharing the global's clap id) | no | |
| location add | yes | honours | no | |
| db backup / fsck | yes | honours | passed | |
| drive status / eject | yes | read-only / honours | no | |
| key escrow-kit | yes | refuses | no | `key export` ignores `--json` (bare key) |
| report verify-status/copies/needs | yes | read-only (not passed) | no | |
| collection sync / status / plan / run | yes | honours (local OR global) / ro / ro / honours | run only | |
| completions | n/a | read-only | n/a | |

`--verbose` only raises the tracing level to DEBUG (main.rs:166-175); no command reads it, so "Enable verbose output" over-promises. `--quiet` only picks the progress display (main.rs:368). Under `--json`, errors are plain `error: …` on stderr (error.rs:670). The one exception is `volume retire`, whose refusal is a JSON object on stdout.

#### 1b. Exit-code conventions (all read in code; documented centrally in docs/troubleshooting.md §Exit codes)
| convention | commands | codes |
|---|---|---|
| ordinary | everything not below | 0 ok, 2 error (clap's usage error is also 2), 75 catalog busy |
| verdict 0/1/2 | audit (errors 70), host check, db fsck, drive poll (+75), drive cleaned, collection sync/status/plan | 1 = warnings/tripped/refused unit |
| config check | | 0 loads, 2 does not; advisory findings never move it (config.rs:18-25) |
| verify | volume verify | 0 / 2 medium proven bad / 3 inconclusive (every error and usage error) |
| write family | volume write/resume/compact-write/compact/rewrite, collection run (+1), quick-archive | 0/2/3/4/5/6/75 |
| erase | cartridge erase | 0/2/3/4/6/75 |
| drive | drive status 0/2, drive eject 0/2/75 | |
| **no verdict** | `unit check-integrity` | **0 even with BITROT/MISSING/SIZE_MISMATCH** (operations.rs:307-315) |

In `--help`: audit, verify, the write family, erase, host check and the drive commands state their codes. config check, db fsck and collection sync/status/plan state theirs only in troubleshooting.md.

#### 1c. Verbs by group
| verb | where | odd one out |
|---|---|---|
| list | tenant, key, unit, snapshot, stage, cartridge, archive-set, location, volume, volume deposit | `catalog ls` |
| info | tenant, archive-set, location, cartridge, volume, stage | **`unit status <NAME>`** (the details view) |
| status | staging, collection, drive, top-level (sessions), `report verify-status` | `unit status` means info |
| show | config | |
| add / create / register / init / generate | add: tenant, location, backend, config, deposit, cleaning-cartridge. create: snapshot, stage, archive-set. register: cartridge. init: root, unit, unit init-bulk, volume. generate: key | five creation verbs |
| delete / remove / purge / clean / retire / mark-* | delete: tenant (soft), snapshot (hard). remove: config, deposit. purge: snapshot. clean: staging. retire: volume, cartridge (+unretire). mark-: tape-only, reclaimable, erased | `drive cleaned` is a record named like an action |
| import / export | key, db, top-level | top-level `export --unit` (slices) and `import --label` (a volume row) are not inverses |
| sync | collection (filesystem→catalog), archive-set (config→catalog) | |
| check-type verbs | config check, host check, unit check-integrity, volume verify, db fsck, audit | |

Registries live in four places: locations in the catalog (`location add`); collections in config only (`config add collections`); drives in config (`backend add` or `config add backends.lto`); archive sets in both (`archive-set create/edit`, and `config add archive_sets` followed by `archive-set sync`).

### 2. Findings

**F1. Global flags are interleaved with each command's own flags in every subcommand's help. HIGH.** `restore unit --help` lists, in order: `--json`, `--unit`, `--from`, `--to`, `-v`, `--device`, `-q`, `--version`, `-y`, `--config` (with its 4-line NOTE), `--scratch`, `--home`, … This is because clap orders flags by declaration index, and src/cli has no `help_heading` or `display_order` anywhere. The friction: the operator has to read past 7 irrelevant flags and the `--config` NOTE to find the command's own. Change: put `help_heading = "Global options"` and a high `display_order` on the 7 global args (mod.rs:37-79). Then check in `-h` that the command's own options print first: clap prints custom headings in the order it first meets them, so the heading alone may land ahead of "Options" (*inferred*; not built). Blast radius: help text only, plus regenerating docs/cli and docs/man (cli_md_fresh.rs, man_matches_help.rs). No spelling changes. Follows gh's separate "INHERITED FLAGS" section and cargo's grouped option headings.

**F2. `-h` is not a summary, and the help has no examples. HIGH.** Clap takes the first paragraph of a doc comment as the `-h` text, and most of these doc comments are one paragraph:
- 14 of 143 command summaries run past 120 characters (collection run 468, volume abort 434, status 321, volume verify 263).
- `volume write -h` prints whole paragraphs for `--loaded`, `--init` and `--full-confirm`; `volume write --help` runs to 1,283 words.
- Clap is built without `wrap_help` (Cargo.toml:10), so nothing wraps to the terminal width.
- Help text cites internals: `cli::consent` (5 times), "`CleanScope::Units`'s own contract in `src/staging/clean.rs`" (staging clean), "`docs/design/layout-session.md`'s Aborted row" (volume abort), "§7" (collection plan).
- There are no examples: there is no `after_help` anywhere; the few examples are buried in prose.

Change: give each doc comment a one-line first paragraph followed by a blank line, which splits about from long_about and help from long_help without deleting any text. Enable `wrap_help`. Move code paths out of help text. Add an `after_long_help` EXAMPLES section to the ~10 focus commands. Blast radius: docs regeneration; help_text.rs pins only issue numbers, `/dev/nst` and "receipt", so it keeps passing (*inferred*). Follows clig.dev ("-h concise, --help full"; "lead with examples") and gh's EXAMPLES sections.

**F3. Command order is declaration order. LOW-MED.** The root lists init, tenant, key, unit, collection, snapshot, stage, staging, volume, cartridge, archive-set, audit, catalog, location, report, restore, export, import, quick-archive, backend, drive, db, config, host, status, completions. The daily commands (status, audit, report) are scattered. Inside `volume`, `plan` sits between `compact-write` and `compact-finish`, and `list`/`info` come last. Change: set `display_order` in workflow order. Optionally, a gh-style grouped `help_template` (Set up / Sources / Write / Check / Restore), with a test that every subcommand appears in it. Blast radius: none on spellings.

**F4. `--json` gaps and inconsistent shapes. MED.**
- `volume identify` ignores `--json`.
- `collection sync|status|plan --json` print a bare array, but `{"collections": []}` when no collection is configured (collection.rs:292-295). That is one command with two top-level shapes, and lifecycle-suite.sh:3225 iterates the array.
- About 38 JSON prints are pretty-printed and about 32 are single-line (grep counts; a few are in tests).
- Errors are text even under `--json` (see 1a).

Change: give identify a JSON object (the File 0 fields, the catalog note, the contradiction); print `[]` for the empty case; write a one-paragraph JSON contract (additive keys only, timestamps as in F16). Blast radius: additive. Peers: OpenZFS 2.3 `zfs list -j` wraps output in an object carrying `output_version`; restic `--json`.

**F5. `--force` has four meanings, and `--yes` silently does nothing on most commands. MED.** ADR-0008 makes `--force` and `--yes` both answer Tier 2; keep that. The friction is what `--force` means from one command to the next:
- **Tier-2 consent in advance, OR'd with `--yes`:** unit mark-tape-only (unit.rs:458), snapshot mark-reclaimable, volume compact-finish/compact (volume.rs:2820), cartridge retire.
- **A lifecycle override:** cartridge mark-erased (`pending_erase`).
- **Wider scope:** `staging clean --force` (all sets, and below-floor releases); `snapshot delete --force` (even if staged).
- **An override of the File 0 identity check:** volume init/write.

Siblings disagree: `cartridge retire --force` exists, but `volume retire` takes only `--yes`. `--yes` is accepted and ignored on every dispatcher not listed in 1a (for example `staging clean --yes`). Change, wording only: open each `--force` help with its class ("Tier-2 consent in advance, same as --yes:" / "Widens what is cleaned:" / "Overrides the File 0 identity check:"). Name in the `--yes` help the commands that read it. Blast radius: help text. Follows clig.dev's "use standard flag names consistently".

**F6. `staging clean --dry-run` refuses. MED.** It is the deletion command where a preview helps most. The stated reason (staging.rs:419) is that a preview would have to reproduce the min_copies gate exactly. The codebase already previews a gate that way: `volume abort` applies the change in a transaction it rolls back, then shows the facts (write.rs:3514-3520). The same idiom fits `unit mark-tape-only` and `snapshot mark-reclaimable`. Peers: `restic forget --dry-run`, `borg prune --dry-run --list`, `zfs destroy -nv`. Blast radius: the dry_run_contract TABLE verdicts change (refuses → honours), plus a dry_run_global row test.

**F7. Size units: output follows the rule; input does not round-trip. MED.** Output is right: data sizes are printed in binary units (KiB…TiB) and capacities in decimal (KB…TB) (util.rs:200-247). One outlier: `staging status` "Size (MiB)" integer-divides (staging.rs:314), while `stage list` shows the same rows through `format_bytes_binary`. Input does not match:
- Both parsers accept only K/KB, M/MB, G/GB, T/TB (binary: staging/mod.rs:2763-2768; decimal: media.rs:359-365).
- `GiB` is refused by both, so the operator cannot type back the unit tapectl prints.
- `G`/`GB` means 2^30 in `slice_size` and `--enospc-buffer`, but 10^9 in `--capacity` and `capacity_override`. That is the "means two things" problem the ADR-0012 comment in media.rs:331 sets out to avoid.

Change: the binary parser also accepts KiB…TiB, and warns (without refusing) when given KB/MB/GB/TB ("read as GiB"). The decimal parser is unchanged. Blast radius: the two parsers only; every existing config still loads.

**F8. Units are named three ways, and `--unit` means two things. MED.**
- Positional in unit/snapshot/stage/catalog commands.
- A required flag in `restore unit|file --unit`, `export --unit` and `volume read-slices --unit`.
- In `host check --unit`, a *systemd* unit (host.rs:24-26).

Change: add an optional positional `[UNIT]` beside `--unit` (`conflicts_with`, `required_unless_present`). The frozen `restore unit --unit U --device D --to P --key K` keeps working. Add `--systemd-unit` as a visible alias on host check. Blast radius: `restore unit --unit` appears in scripts 31, docs 15, src 11, all of which keep working. Follows restic's `restore <snapshot> --target`.

**F9. Destination and source flags are spelled differently between siblings. LOW-MED.**
- `--to` means a location (volume move, cartridge move, deposit add), a directory (restore, export), a file (db backup), a tenant (tenant reassign), or a volume label (`volume compact --to`).
- `volume compact-write` spells that same volume label `--destination`.
- The source volume is positional in `restore volume` and `volume rewrite`, but `--from` in `restore unit|file` and `read-slices`.

Change: let the compact siblings share one spelling through a hidden alias each way. Blast radius: `compact-write --destination` appears in scripts 2, docs 7, src 5. Follows clig.dev ("same flag, same meaning").

**F10. Arguments the parser could require, or default. LOW-MED.**
- `collection run --collection` is required even when only one collection is configured. `--device` already defaults to the only configured drive, and that rule fits here; a positional `collection run movies` would too.
- `collection run --label <LABELS>` is a `Vec` that is refused at runtime when empty or when more than one is given (plan.rs:832, 980). The usage line hides that it is required and calls it plural.
- `--full-confirm-first` says it "reads back the same copy `--full-confirm` does" today.
- `catalog rebuild --from-volume` is checked at runtime (catalog.rs:653). `required = true` would keep the frozen spelling and put it in the usage line.
- `key list --tenant` is required; it could list every tenant.

Blast radius: none; no spelling changes.

**F11. `stage` and `staging` overlap. MED.** `stage list` and `staging status` print the same `stage_sets` rows with different headers ("Encrypted"/"Size (MiB)", "Staged At"/"Staged"), different size renderings and different empty-cell marks ("-" stage.rs:140, "" staging.rs:306). `report pending` is a third view of the same rows. Change: add `stage status` and `stage clean` as new leaves that call the `staging` code, and render both from one row type. Alternatively, move both into `StageCommands` and keep `staging` parsing through `#[command(visible_alias = "staging")]` on `Stage`. Blast radius: `staging clean` appears in scripts 30, docs 121, tests ≥15, src 147; the old spelling keeps working either way. Plus a dry_run_contract TABLE row for each new leaf. Follows clig.dev ("avoid similarly named commands").

**F12. "Under-covered" is answered four ways, and one about is wrong. LOW-MED.** `report fire-risk` says "below min copies/locations", but it only judges copies against the copy floor (report.rs `fire_risk_rows`: `copies == 0` / `copies < floor` / `continue`). Per location is `report needs`; `report copies` gives counts; `audit` judges everything; `collection status` counts "under-copied". Change: fix the about, cross-link the four commands, and add a `copy_floor` key to the JSON (see G11).

**F13. Verb outliers. LOW.**
- `unit status <NAME>` is the only details view not called `info`; add `unit info` as a visible alias. Follows gh's `view` and kubectl's `describe` used the same way across every resource.
- `catalog ls` sits among `list` commands everywhere else; add a `list` alias.
- The root `export` (a unit's slices to a directory) and `import` (a volume row) look like a pair but are not inverses; add `unit export` and `volume import` as new leaves that call the same code (these cross groups; plus TABLE rows).
- The top-level `status` reads like zpool or git `status` (the archive's health) but shows sessions; add a "see also: audit, report summary" line.

**F14. `backend add` adds a drive. LOW-MED (also G2).** The `backend` group has one leaf, which writes `[[backends.lto]]`, and a `drive` group already exists. Change: a `drive add` leaf that calls `backend add`'s code (this crosses groups, so it is not a clap alias). Blast radius: `backend add` appears in scripts 19, docs 50, tests 16, src 34, and all of them keep working; the config key is unchanged; one dry_run_contract TABLE row.

**F15. `unit check-integrity` exits 0 on a failed check. MED.** It prints BITROT, MISSING and SIZE_MISMATCH counts and still exits 0 (operations.rs:307-315); every other check verb in 1b has a verdict code. Flipping the default would break lifecycle-suite.sh:3043, which treats non-zero as failure after it mutates files on purpose. Non-breaking path: an opt-in `--exit-code` flag (exit 1 on any difference), as in `git diff --exit-code`; flip the default later together with the suite. Also: state config check's, db fsck's and the collection commands' exit codes in their `--help`.

**F16. Tables and timestamps. LOW-MED.**
- All 13 list tables use tabled's default ASCII boxes (`+---+`, `|`).
- `volume list` headers are UPPERCASE; every other table uses Title Case.
- `cartridge list` labels the generation "Type"; `volume list` says GENERATION.
- Timestamps are SQLite `datetime('now')` (`YYYY-MM-DD HH:MM:SS`, UTC) and unlabelled, except `cartridge journal`'s "Captured (UTC)" and one catalog line. `status` and the session logs use RFC 3339 with `Z`, and JSON carries the SQLite form.
- grep finds no pager and no width handling in src/ (no PAGER, terminal_size or COLUMNS use).
- In tapectl's favour: its own output has no colour, and the tracing output honours NO_COLOR and the TTY (main.rs:181-185).

Change: whitespace-aligned tables (`Style::blank`), as `zfs list`, `restic snapshots` and gh print them; one header case; "(UTC)" in timestamp headers; RFC 3339 `Z` in JSON. Follows clig.dev ("plain, line-based output that grep/awk can read"), with `zfs list -H` as the machine-table precedent.

**F17. You cannot tell which home tapectl resolved. MED.** The precedence (startup.rs:21-23) is `--home` > `TAPECTL_HOME` > the parent of `--config` > an existing `~/.tapectl` > `/etc/tapectl/home` (never as root) > `~/.tapectl`.
- The `--home` help says only "Defaults to ~/.tapectl … Also settable as TAPECTL_HOME". It omits the pointer, `TAPECTL_HOME_POINTER`, and the rule that an existing `~/.tapectl` beats the pointer. home2's home is `/srv/archive_meta/tapectl/home` (item 197).
- "tapectl is not initialized — run `tapectl init` first" (main.rs:331) does not name the path, and `config check` prints "config: valid" without naming the file.

Change: name the home and where it came from in that error, and as config check's first line ("home: X (from TAPECTL_HOME)"); give `--home` the full precedence in its long help. Follows git's "not a git repository" message and restic's "repository … opened". Blast radius: **`contrib/systemd/tapectl-op.in:38-69` hardcodes `--config|--home` as the only global flags that take a value, and hardcodes the list of tape subcommands** (tested by scripts/check-tapectl-op.sh). Any new global flag that takes a value, and any alias of a tape command (F8, F9, F11), must be added there, or the command silently runs at normal priority. Also, briefly: `status` on a missing logs directory reports ENOENT as an `[ops]` group permission problem (reviewer A owns the wording).

**F18. Completions and man pages. LOW.**
- `tapectl completions zsh | head -3` panics with BrokenPipe and exits 101. main.rs:262 hands `std::io::stdout()` straight to `clap_complete::generate`, bypassing tapectl's closed-pipe-safe output.
- Neither the completions nor the 148 man pages are installed by first-run.sh or install-systemd.sh (grep finds no match). The operator types through `tapectl-op`, which would also need `complete -F _tapectl tapectl-op` (*inferred*).
- `--dry-run` has no `-n` short form (clig.dev standard names; zfs and rsync use `-n`).

### 3. Glossary mismatches against CONTEXT.md
- **G1 Pending. HIGH.** `collection status` reports "N pending, M dirty", and pending there counts only units with no snapshot (collection/status.rs:15, `PendingReason::New`). CONTEXT **Pending**: _Avoid_ "pending as a synonym for never-archived alone". `report pending` and `volume plan`'s "pending staged data" use a third sense, staged but not written. Fix: add a `never_archived` JSON key beside `pending`, and change the text to "N never archived, M dirty (N+M pending)". The JSON array shape is consumed by lifecycle-suite.sh:3225; `report pending --json` by lifecycle-suite.sh:1473. Both changes are additive.
- **G2 backend.** `backend add`, "Tape drive backends", `import --backend` (32 occurrences in docs/cli). CONTEXT **Store** lists backend under _Avoid_, and the thing added is a drive (F14).
- **G3 Barcode as the handle.** Every `cartridge … <BARCODE>` takes the barcode, and `cartridge info` looks up by `WHERE c.barcode = ?1` only. The exception is `journal --serial`, and `volume init --cartridge` is "by barcode" too. `drive cleaned --cartridge` takes a serial, so it already follows the glossary. CONTEXT **Cartridge Identity** is the chip serial, and **Barcode** lists "identifier" under _Avoid_. Fix: accept a barcode or a serial, with the metavar `<CARTRIDGE>`.
- **G4.** `restore raw-volume` is described as "the emergency/heir path". **Heir Path**: _Avoid_ "emergency restore".
- **G5.** "end of tape" (volume write's exit 5), "end-of-tape" (`backend add --enospc-buffer`, `stage for-location --device`). **Early warning**: _Avoid_ "EOT and end of tape"; say "hard end" or "early warning".
- **G6.** `report supersedable`: "Superseded snapshot versions…". **Current**: _Avoid_ "superseded". The act is `mark-reclaimable`, so propose a `report reclaimable` alias.
- **G7.** `cartridge list` column "Type" holds `media_type`. **Generation**: _Avoid_ "media type as the name of the concept"; `volume list` already says GENERATION.
- **G8.** `unit status --dirty` prints "dirty/clean/new". **Never Archived**: _Avoid_ "new".
- **G9.** `unit check-integrity` reports a same-size edit as "BITROT": it compares size, then hash, never mtime (operations.rs:212-231). **Dirty**: the disk is authoritative, so an edited file is dirty, not rot. Propose "CHANGED".
- **G10.** "damaged or partial tape" (`catalog rebuild --tenant`). **Unsealed**: _Avoid_ "partial".
- **G11.** The JSON key `min_copies` in `report fire-risk` and `report pending` actually carries the copy floor (`copy_floor(...)`). **Copy target**: "copy floor" is not `min_copies`. Fix: add `copy_floor`.
- **G12.** The `catalog` group is "Browse and search file catalog", yet `catalog rebuild`, "catalog busy" (exit 75) and `db backup` treat the catalog as the whole database. CONTEXT has no **Catalog** entry, and none for the CLI's main nouns either: Unit, Volume, Cartridge, Location, Tenant, Snapshot.
- **G13.** Help uses "ID thunk" 7 times and "File 0" 35 times for the same thing; CONTEXT uses "ID thunk" without defining it.
- No action: `--jobs` means parallelism, not Write Session's "job". `--receipt` on `deposit add` is explicitly allowed (CONTEXT, Receipt; pinned in tests/help_text.rs).

### 4. Top 10, ranked
1. F1: a "Global options" heading. One file, no spelling change, every `--help` gets shorter.
2. F2: one-line first paragraphs, `wrap_help`, EXAMPLES on the focus commands.
3. G1: pending means never-archived plus dirty (an additive JSON key).
4. F15: `unit check-integrity --exit-code` (opt-in now, default later).
5. F17: name the resolved home and its source; document the full precedence; keep tapectl-op in step.
6. F7: accept KiB…TiB on input; warn on SI suffixes in binary fields.
7. F6: a real `staging clean --dry-run` through a rolled-back transaction.
8. F5: classify each `--force` in its help; say where `--yes` is read.
9. F11 + F13 + F14: same-parent visible aliases (`unit info`, `report reclaimable`, `catalog list`) and delegating leaves (`stage status|clean`, `drive add`). No renames.
10. F4 + F16: fix identify's `--json` and the `{"collections": []}` shape; whitespace tables, one header case, timestamps labelled UTC.

## tapectl CLI review C: the tape commands, run against mhvtl

Binary `/scratch/cargo-target/debug/tapectl` 1.8.1 (v1.8.1-4-gd1299da8). Every call went through a wrapper that forces
`TAPECTL_HOME=/scratch/tapectl-cliux-c/home` (the heir: `…/heir`) and logs `rc<TAB>argv` to `/scratch/tapectl-cliux-c/journey.log`.
Raw output: `/scratch/tapectl-cliux-c/phaseB.out`, `phaseC.out`.
- **Drive:** mhvtl A2 only, resolved by-id (`scsi-XYZZY_A2-nst` → `/dev/nst1`, sg4, `IBM ULT3580-TD8`).
- **Changer:** `mtx -f /dev/sch0 status` fails (`READ ELEMENT STATUS Command Failed`); `mtx -f /dev/sg3` works, and A2 is its DTE 1.
- **Config:** defaults untouched (`min_copies = 2`, `min_locations = 2`); no `capacity_override` (the chip gave 12.00 TB).
- **Data:** `photos/2019` and `2020` (1.2 MiB each), `2021` (201 MiB, so a Ctrl-C could land mid-write), `2022` (0.3 MiB).
- **Lock:** every tape command ran under `flock /scratch/tapectl-build.lock`. I waited 04:10–05:11 behind lcsas, then
  ran phases C and D in one 1.5-minute hold.
- **Prompts:** typed consents went through `script -qec` on a pty; Tier-2 answers were not needed.
- **Not run:** `volume write --json`, `volume resume --full-confirm`, a Ctrl-C that stops mid-data (mine reached a safe
  point after the seal, see T3), `volume write --loaded`, a restore across cartridges.

### 1. Commands as run (exit code first)
- **Setup.** `0 init`, `0 tenant add alice`, `0 location add onsite`, `0 location add offsite`.
  `2 backend add --name vtl --device-tape /dev/tape/by-id/scsi-XYZZY_A2-nst` (no `--generation`), then
  `0 backend add … --generation LTO-8 --dry-run` and `0 backend add … --generation LTO-8`.
  `0 config check`, `0 config add collections …`, `0 collection sync --yes`.
  `0 snapshot create` ×2, `0 stage create photos/2019 photos/2020`, `0 volume plan`.
- **Copy 1 (E01001L8).** `0 drive status`, `0 drive status --json`,
  `0 volume write L8-0001 --init --full-confirm --move-to onsite` (2 s), `0 volume verify L8-0001 --full`,
  `0 volume info L8-0001`, `0 volume identify`, `2 audit`, `0 volume list`, `0 cartridge list`.
  `0 report verify-status`, `0 report copies`, `0 volume list --json`, `0 volume info L8-0001 --json`.
- **Catalog-only mistakes.** `2 volume move L8-0001 --to ofsite`, `2 volume move L8-0009 --to offsite`,
  `0 volume move L8-0001 --to onsite` (already there), `2 volume info L8-0009`, `0 staging clean`.
  `2 restore unit --unit photos/2019 --to r1 --dry-run` (no `--from`), `0 … --from L8-0001 --dry-run`,
  `2 restore unit --unit 2019 …`, `2 restore unit --unit photos/2021 --from L8-0001 …` (not on that tape).
- **Mistakes with L8-0001 loaded.** `2 volume write L8-0001` (sealed), `3 volume verify L8-0009`, `3 volume verify l8-0001`.
  `2 volume init L8-0002` and `2 volume write L8-0002 --init --full-confirm --move-to offsite` (forgot to swap).
  `2 cartridge erase` (no dry run first, live copy), `2 cartridge erase --dry-run`, `0 drive eject`.
- **Empty drive.** `0 drive status`, `2 volume write L8-0002 --init …`, `3 volume verify L8-0001`, `2 volume identify`,
  `0 drive eject` (`nothing to eject`).
- **Reusing a foreign tape (E01002L8, sealed `UPG-1` from another catalog).** `0 drive status`, `0 volume identify`,
  `0 cartridge erase --dry-run`. Then `2 printf 'UPG-1\n' | cartridge erase` (piped), `2 cartridge erase` on a TTY typing
  `L8-0002`, `0 cartridge erase` on a TTY typing `UPG-1`, `0 cartridge list`.
- **Copy 2, interrupted.** `4 volume write L8-0002 --init --full-confirm --move-to offsite` with SIGINT 0.5 s into the
  data, then `0 status`, `0 volume info L8-0002`, `0 volume list`, `0 volume resume L8-0002`, `0 volume info L8-0002`,
  `2 audit`, `0 staging status`.
- **Restore (L8-0002 loaded).** `0 volume verify` (no label), `3 volume verify L8-0001` (wrong tape).
  `0 restore unit --unit photos/2019 --to r1` (no `--from`; output identical to source by `diff -r`).
  `2` the same into the now non-empty r1, `2 restore unit … --from L8-0001` (wrong tape).
  `0 restore file --file img_1.jpg …`, `2 restore file --file nope.jpg …`.
- **Heir (fresh home).** `0 init --operator mikmorg --escrow-public-key age1…`,
  `0 catalog rebuild --from-volume --device <by-id> --key <operator key>`, `0 unit list`,
  `0 restore unit --unit photos/2019 --device <by-id> --to heir1 --key <key>` (identical), `2 audit`.
- **Collection run.** `0 collection sync --yes` (adds 2022), `2 collection run --collection photos --label L8-0003`
  (not initialised), `0 drive eject`, mtx to E01004L8 (sealed `SPA-0002`), `0 drive status`,
  `0 cartridge erase --dry-run --json`, `0 cartridge erase` (typed). Then `0 volume init L8-0003`,
  `0 collection run --collection photos --label L8-0003`, `0 volume info L8-0003`, `2 audit`, `0 report copies`,
  `0 staging status`, `0 staging clean`, `0 volume move L8-0002 --to offsite`.

Every exit code matched its documented table (troubleshooting.md §Exit codes): verify 3 for an unknown label or wrong
tape, write 4 when interrupted, erase 2 when refused and 0 on a dry run, eject 0 when nothing is loaded.

### 2. Findings

#### T1. The one-command copy's summary omits the seal, the readback and the right next command (HIGH)
Copy 1 ended:
```
volume "L8-0001" write completed
staging kept: not every unit written has its policy's copies yet, and the next copy reuses these staged slices:
  unit photos/2020 has 1 copy, needs 2 …
  write another copy with `tapectl volume write <label>`; `tapectl staging clean` releases the units that are covered
volume "L8-0001" moved to "onsite"
```
- **No verdict on the tape.** Nothing says *sealed*, or that the full readback ran and passed. The session log has it
  (`phase end: confirm 154 ms 7.5 MiB`), and `volume info` lists a `[full] passed` row, but the terminal does not.
- **The wrong next command.** The hint omits `--init`, which the blank cartridge needs, and names no location, although
  `min_locations = 2` sends copy 2 elsewhere. `collection run` ends the same way: `swap in the next cartridge and run
  tapectl volume write <label> to write another`.
- **Noise first.** Before the summary come an LBP paragraph, a 5-line chip-wear block (mhvtl's `lifetime written
  61648601088 MiB`) and a feed-ratio warning.

**Change:** keep `write completed`, which mhvtl-verify-gate.sh and first-run-reentry-mhvtl.sh grep. After it, add:
- `sealed and confirmed: 2 units, 2 slices, 2.3 MiB; full readback passed (11 files)`;
- from the coverage query, `next copy: load a blank cartridge; tapectl volume write <NEW-LABEL> --init --full-confirm
  --move-to offsite`.

Move the wear block and the feed ratio after the summary, or behind `-v` unless a threshold trips.
**Blast radius:** binary. `write another copy`: src 1, docs 2; `swap in the next cartridge and run`: src 1, docs 2. No
scripts or tests.

#### T2. Forgot to swap: the refusal hides which tape is loaded, and that it is a live copy (HIGH)
With L8-0001 still loaded (sealed, the only copy of two units), `volume init L8-0002` and `volume write L8-0002 --init …`
both exit 2 with one 900-character paragraph:
`error: refusing to write volume "L8-0002": the loaded cartridge already carries a SEALED volume — a valid seal marker
parses at tape position 10. ADR-0003: … If this cartridge should be reused: retire its current volume if this catalog
holds it (…volume retire <label>), then tapectl cartridge erase … — or erase the tape in the drive (… mt -f <device>
rewind; mt -f <device> weof 1 …) yourself and run tapectl cartridge mark-erased … Never degauss …`
- tapectl has just read File 0, and the uuid matches its own row. `cartridge erase`, a second later, says so:
  `holds volume "L8-0001" (matched by uuid), which the catalog holds as sealed (condition ok) — it counts as a live copy`.
  This message says "if this catalog holds it" and names no label.
- After a copy, the likeliest cause is a cartridge not swapped. The message offers three ways to destroy the tape and
  none to put it away.
- `scripts/first-run.sh:2308` wraps this exact refusal with "The likeliest case … is that the TEST cartridge is still in
  the drive: then do NOT erase anything here — unload it". The binary should say that itself.

**Change:** lead with `the loaded cartridge holds volume "L8-0001" (sealed 04:00 today; this catalog's copy of 2 units).
To write L8-0002, load a blank cartridge: tapectl drive eject`.
- Offer `cartridge erase --dry-run` only when the catalog does not hold the volume, or holds it retired.
- Move the `mt` recipe and the degauss warning to troubleshooting.md.

**Blast radius:** keep the phrase `already carries a SEALED volume`: first-run.sh:2300 routes on it (scripts 2, tests 2,
docs 3, src 1).

#### T3. An interrupted one-command copy drops `--move-to` silently, and a bare `resume` downgrades `--full-confirm` (HIGH)
I sent SIGINT to `volume write L8-0002 --init --full-confirm --move-to offsite` 0.5 s into its 201 MiB slice. It finished
the slice, sealed, and stopped (good: `stopping at the next point the work can be resumed from`). Exit 4. Then stderr
gave two hints that disagree:
- `volume "L8-0002" was initialized by this command … tapectl volume write L8-0002 (without --init …) writes it`;
- `error: … interrupted after seal, before confirm … run tapectl volume resume L8-0002 --full-confirm`.

The first offers `volume write`, the second says the tape is sealed; one of them is wrong (I did not run the `volume write`).

`volume info L8-0002` then contradicted itself: `Status: initialized`, `Sealed: 2026-11-04 05:12:15`,
`Last write: (never written)`.

I ran the bare `volume resume L8-0002`, as an operator would from memory. It printed `volume "L8-0002" write resumed and
completed` and did a quick confirm only, with no word that the write had asked for a full one. The tape stayed `Location: (not placed)`: `volume resume` has no `--move-to`, and
nothing said the move was skipped. `audit` later flagged `no_full_verify`. `report copies` showed `photos/2021: 2 copies,
1 location` until I ran `volume move` by hand. The one command the operator typed promised the readback and the shelf;
after one Ctrl-C, both depend on remembering. Exit 4's error line did name `--full-confirm`; nothing named the move.

**Change:**
- Record the interrupted command's `--full-confirm` and `--move-to` in the session row, and have `resume` honour them.
  Failing that, add `resume --move-to`, and end resume's output with `not moved: the write asked for --move-to offsite;
  tapectl volume move L8-0002 --to offsite`.
- Print one hint, chosen by state: after the seal, only the resume line.
- In `volume info`, show the session state on the `Status` line (`initialized — sealed, confirm pending`).

**Blast radius:** binary; additive flag. `volume resume` appears 45 times in scripts and 128 in docs, and all keep working.
`interrupted after seal` and `was initialized by this command`: src only.

#### T4. Covered units are written again, and the summary doesn't say so (MED)
Copy 2 brought 2019 and 2020 to 2 of 2 copies, but 2021 was short, so the all-or-nothing release (staging/release.rs:15)
kept all three staged. The next `collection run --label L8-0003` printed `about to write … photos/2019 v1 … photos/2020 v1
… photos/2021 v1 … photos/2022 v1`, and ended `batch 0: 1 unit(s) staged, 1 copy/copies written`. `report copies` then
showed `photos/2019: 3 copies`. Nothing at plan time said 2019 and 2020 were already covered.

On LTO-6 that is real tape and hours. The fix today is `staging clean` between copies, which the hint names only in passing.

**Change:** mark each `about to write` line `(copy 3; floor 2)`; the summary says `4 units written: 1 new (photos/2022),
3 already at their floor (held in staging)`; when a covered unit would be re-copied, name `staging clean` first.

The release rule itself is a design ruling and stays.
**Blast radius:** binary text (`copy/copies written`: src 2).

#### T5. `--full-confirm`, then `volume verify --full`: a second full read nobody is told about (MED)
Copy 1 ran `--full-confirm`, and `volume verify L8-0001 --full` then read the whole tape again. `volume info` shows two
`[full] passed` rows 13 s apart. operator-guide.md:367 calls the second "a second, independent read-back"; on a full
LTO-6 that is another 3-4 hours. first-run.sh step 13 writes without `--full-confirm` and verifies once. `--full` is
verify's default tier, yet the docs spell `volume verify <L> --full` 32 times (scripts 20).

**Change:** T1's summary says the readback is recorded; `verify` on a volume read back in full under a day ago prints
`last full readback 04:00 (the write's --full-confirm); reading again`; the docs pick one loop.

**Blast radius:** docs (operator-guide.md, handoff.md (i)); one line in the binary.

#### T6. A mistyped label: no suggestion, case-sensitive, ssh warning first (MED)
- `volume verify L8-0009` and `volume verify l8-0001` print `error: volume not found: L8-0009` and exit 3. Each first
  printed the 2-line ssh/tmux warning, then failed at once.
- `volume move … --to ofsite` already does it right: `location "ofsite" not found (known locations: offsite, onsite)`.
- `volume write L8-0001` (sealed) says `To write this cartridge again, run tapectl volume init <new-label> on it`. The
  likelier intent is "I retyped the last label", and the hint points at re-initialising a cartridge that holds live copies.

**Change, text only:**
- Keep the exit codes: 2 is verify's "medium proven bad", so a typo must never get it.
- `volume not found` gains `did you mean L8-0001?` (case-insensitive) or `known: L8-0001 (sealed, onsite), …`.
- The ssh warning prints after the label resolves.
- For a sealed target: `L8-0001 was sealed on <date>; a new copy needs a new label`.

**Blast radius:** `volume not found` (scripts 3, tests 2, docs 3, src 7); the text is kept and a suffix added.

#### T7. The barcode is the chip serial, and `drive eject` names no volume (MED)
- Each write and erase registered the cartridge as `barcode = medium serial` (`E01001L8_1775794348`), though the operator
  typed `L8-0001`, the sticker.
- `drive eject` printed `ejected cartridge E01001L8_1775794348 (chip E01001L8_1775794348) … tapectl cartridge move
  E01001L8_1775794348 --to <location>`: the same string twice, and no volume label.
- handoff.md (i) step 9 has the CTO relabel by hand (`cartridge relabel <serial> L6-0001`).

**Change:** ADR-0012 keeps identity on the serial. Add `--barcode` to `volume init` and `write --init`, or default the
barcode to the volume label when no cartridge has it (the CTO's own practice). Eject prints `ejected L8-0001 (cartridge
E01001L8_…); it belongs at onsite`.
**Blast radius:** `auto-registered from MAM` (scripts 1, docs 6, src 1); `ejected cartridge` (docs 2, src 3).

#### T8. `collection run` on a new label says only "not found" (MED)
`collection run --collection photos --label L8-0003` → `error: volume not found: L8-0003`, exit 2. The label is meant to be
a fresh tape, and `--label`'s help says it must already be `volume init`'d; the error should say so:
`L8-0003 is not initialised: load a blank cartridge, then tapectl volume init L8-0003`. (Reviewer A's F9 asks for
`--init`; this is the error text meanwhile.)

#### T9. Erase consent: the facts come twice, and once after the error (LOW)
- **Piped stdin:** the refusal (`non-interactive session and no --confirm-label given … Re-run with --confirm-label UPG-1
  if that is the cartridge you mean to erase`) prints *before* the 12-line identification block it rests on.
- **TTY prompt:** `short ERASE the tape in /dev/tape/by-id/scsi-XYZZY_A2-nst (a volume tapectl wrote) — type UPG-1 to
  proceed:`. It is clear, but "a volume tapectl wrote" loses the block's key line, `catalog: knows neither this volume nor
  this cartridge`. After consent, the whole block prints a second time (the read-write reopen).
- A wrong word gives `aborted, "L8-0002" is not "UPG-1"`, which is good.

**Change:** facts, then the error; the prompt says `… (UPG-1, another catalog's volume) — type UPG-1 to erase it:`;
after the reopen, print `chip serial unchanged` instead of the whole block.

**Blast radius:** src only (`to proceed`: src 2).

#### T10. Smaller items (LOW)
- `volume list`'s `MIN COPIES 1` (the least-covered unit, volume.rs:3737) sits beside `min_copies = 2`; show `1 of 2`.
- `volume info`'s history says `(11/11 slices passed)` for 11 files, and `[quick] … (2/2 slices passed)` for the front
  index and the seal. `report verify-status` repeats the LBP sentence on every row.
- `Units carried: 2 (…, 2026-11-04 03:59:13)`: the date has no label.
- `volume move L8-0001 --to onsite` (already there) prints `moved`. `staging clean`: `cleaned 0 stage set(s),
  0 files removed, 593.9 KiB freed`.
- `volume plan` with no flag plans `(1 copy/copies)` under a 2-copy policy.
- `volume identify` prints its verdict after ~55 lines of File 0 text.
- `restore unit --dry-run` without `--from` refuses, though the catalog knows the only holder.
- Raw tracing lines reach the operator: `…Z WARN tapectl::dar::restore: restoring as a non-root user …`. The heir gets
  `WARN tapectl::volume::write: device … matched no configured [[backends.lto]] entry; drive health (sg_logs) was not
  collected` twice, which is alarming on a recovery path that has no config by design.
- Also: the wrong-tape error could say where L8-0001 is (`onsite`); `backend add` requires `--generation` though
  `drive status` reads `ULT3580-TD8` from the same node.
- On mhvtl, the feed-ratio warning (`63.578 native bytes per data byte`) and the chip lifetime figures are noise; label
  them when the chip says `linuxVTL` or `capacity_override` is set.

#### What worked well
- **`drive status`:** one screen, plain words (`free — no tapectl command has this drive`, `cartridge loaded, ready,
  writable`, `this cartridge is unknown to this catalog`), the same facts in `--json`.
- **`cartridge erase --dry-run`:** label, uuid, writer, seal and totals, the catalog's verdict, and
  `confirm with: --confirm-label UPG-1`. The live-copy refusal names `volume retire L8-0001`. The init after an erase says
  `blank proven by this catalog's verified erase … no --force needed`.
- **Wrong tape:** `wrong tape: this command names volume "L8-0001", but the loaded cartridge's File 0 identifies volume
  "L8-0002". Load "L8-0001"'s cartridge, or re-run naming "L8-0002".`
- **Loaded tape named:** `volume verify` and `restore unit` without a label print `…, the loaded tape`.
- **Heir path:** File 1's three commands ran as printed. The rebuild lists the volumes it could not see
  (`L8-0001 … load each and run tapectl catalog rebuild`) and tells the heir to `volume verify` the tape's claims.

### 3. Ranked top 5
1. **T3:** an interrupted `volume write --init --full-confirm --move-to` resumes without its readback or its move, says
   nothing, and its two hints disagree. Carry the flags into the session, or say what was dropped.
2. **T2:** the forgot-to-swap refusal should name the loaded volume as this catalog's live copy and say "eject, load a
   blank", not offer three ways to erase it. first-run.sh already patches this in shell.
3. **T1:** the write's summary should say sealed and read back, and give the real next command (`--init`, the location
   still short), including `collection run`'s.
4. **T4:** say at plan time when a unit is already at its floor, so a third copy is a choice.
5. **T5:** choose one copy loop. With `--full-confirm`, the docs' `volume verify --full` is a second full read of the tape.

### State left
A2 holds **E01004L8** (sealed volume L8-0003 of this test catalog). E01001L8 (L8-0001) is back in slot 1 and E01002L8
(L8-0002) in slot 2. The erases overwrote two leftover test volumes from other runs: `UPG-1` on E01002L8 (2026-11-03,
the label suggests the upgrade harness) and `SPA-0002` on E01004L8 (the label suggests the span gate; both inferred). The lock is released.
