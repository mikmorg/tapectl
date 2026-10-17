# Spanning and the cross-volume index (R4): a design draft for the CTO's grilling

**Status: superseded where ruled.** The CTO ruled on 2026-10-16 (ADR-0012 items 76-87): D2 (a),
D3 (b), I1 (c), I2 (b, units and every path), I3 (b); and, after discussion, D1 is **shape A, opt-in
per unit** (items 82-87): spannable units are packed last and cut at the drive's early warning,
the slice in flight finishing, then the short seal (#420). Shape B (parts), D4 and D5's B branches,
D7 and the parts-specific parts of D8-D14 below were not chosen; read them as history. The ADR is
the authority; this note is the reasoning. Rulings will be
recorded in ADR-0012 and the normative format docs, as the plaintext-free-staging note's were.
Issues: #409 (spanning), #418 R4 (the cross-volume index).

Draft, 2026-10-16, against master `6ea133c` (1.2.0 assumed landed). Read-only research:
nothing was built, run or re-pinned. Citations are `file:line` on that tree. "ADR-0012
item N" means the numbered item in `docs/adr/0012-copies-are-identical-content-cartridges-are-known-by-serial.md`.

## 1. What is ruled and what is being asked

**Ruled, and built on here.** A drive that runs out mid-write never splits a unit:
handover to a second cartridge was rejected, and a later short seal (#420) writes the cut
unit whole to the next cartridge (item 9, :1152). Readers refuse a tape whose `requires`
names a feature they do not know, and ignore keys they do not know (item 15, :1177;
`volume-format-v2.md` §1.2). A unit larger than one cartridge is refused today (#395);
"planned spanning (the planner splits a unit's slices across a named set of cartridges)"
was to be designed once a unit passes about half a cartridge (item 17, :1188). The rest
stands as ruled: 1 GiB slices (item 10), no parity (item 13), no plaintext catalogue
(item 14), and a failed stage starts over under `dar -c -` (2026-10-07 item 11, :1255).
Units are never split by hand (CLAUDE.md; #12's 2026-09-30 note). A Copy is identical
content counted per Version (CONTEXT.md "Copy"), and the ADR-0003 and ADR-0008 consent
points stand.

**Asked.** On 2026-10-16 the CTO said "We should design spanning now" (#409) and, of #418
R4, "Design and build it (1.3.0)". This draft gives the facts, the decision tree and a
recommendation for each decision. The CTO decides. One fact changes the question. On
home2, staging does not have room for any unit that would need spanning (§2). So the
first decision is not how to split a unit's slices. It is **what is divided**: one dar
archive's slice stream, or the unit's files, into several dar archives underneath the
unit. Item 17's parenthesis describes #395's option (a). #395 never offered the second
shape. The CTO's own constraint admits it: "if we do need to it needs to be hidden
underneath" (#12, 2026-09-30 comment). The second shape is put as an option in D1. It
departs from item 17's parenthesis ("splits a unit's slices") and from "units are never split
by hand"; whether "hidden underneath" covers it is the CTO's call.

## 2. The problem in numbers

**Cartridges** (decimal, `src/media.rs:145-159`). The one-tape budget is capacity × the
97% fill ceiling, less the 50 MiB reserve (`config.rs:507-509`, `:562-583`, `:614`).

| Generation | Capacity | One-tape budget | Half a cartridge | 1 GiB slices per budget |
|---|---|---|---|---|
| LTO-5 | 1.50 TB | 1.45 TB | 0.75 TB | ~1,355 |
| LTO-6 | 2.50 TB | 2.42 TB (`troubleshooting.md:1220`) | 1.25 TB | ~2,258 |
| LTO-7 | 6.00 TB | 5.82 TB | 3.00 TB | ~5,420 |
| LTO-8 | 12.0 TB | 11.64 TB | 6.00 TB | ~10,840 |

Measured on home2's real drive, a fresh LTO-6 took 2,501,995,134,976 bytes before ENOSPC,
and early warning sits about 99,008 MB before it
(`docs/runs/2026-09-23-real-drive-rehearsal.md:137-164`).

**Real units.**
- The whole first production tape is `keepsake`, about 1.3 TB, one unit per folder
  (ADR-0012 :942-946).
- The largest unit is `keepsake/video` at 470 GiB
  (`docs/research/2026-10-06-plaintext-free-staging.md:145`). That is ~505 GB: 20% of an
  LTO-6 and 34% of an LTO-5 (#12, 2026-09-30 comment), about 470 slices.
- **The half-cartridge trigger of item 17 is not met** on either generation: 470 GiB is
  below 0.75 TB. The CTO chose to design early. Nothing on home2 needs spanning today.
- The repo has no per-unit sizes for the 1.1.0 fresh-start tapes L6-0001 and L6-0002.
  No `docs/runs/` file covers 2026-10-14/15.

**Rates on home2** (`docs/runs/2026-10-12-1.1.0-real-drive-proof.md:54-63`):
- staging 39-41 MiB/s per job on a debug musl build, before #441's single source read;
  a re-measurement is owed;
- tape write 100.6 MiB/s, and ~140 MiB/s with the host quiet;
- full verify 52.5 MiB/s.

At 40 MiB/s one LTO-6 budget takes ~16 h to stage. A 3 TB unit takes ~20 h, a 5 TB unit
~33 h. Writing 2.42 TB takes ~4.6 h at 140 MiB/s.

**Staging, the binding constraint.**
- A stage set must be whole on disk before any write:
  - a set becomes `staged` only after its last slice (`staging/mod.rs:1020-1071`);
  - a write takes whole staged sets (`write.rs:6592-6668`);
  - a write refuses a set that is not whole, because it "would seal a partial unit as a
    full copy" (`write.rs:6687-6732`).
- home2's staging is "~2.3 TB free" and "holds one tape's encrypted slices"
  (`contrib/hosts/home2.profile:61-62`). That is less than one LTO-6 budget (2.42 TB).
- **Under today's pipeline, no unit large enough to need spanning can be staged on
  home2.** This holds whatever the planner does, unless staging grows.

**Today's refusal path.**
- `stage create` refuses before the stage set row and before dar
  (`staging::check_unit_fits_one_tape`, `staging/mod.rs:1573-1604`, called at :581). It
  compares the plaintext walk size (:1578) with the largest configured drive's budget,
  and says the unit "waits for planned spanning, which is not built yet" (:1595-1599).
- `collection plan` returns no batches at all when any unit is oversized
  (`collection/selector.rs:158-172`; `collection/plan.rs:168-227`).
- `volume write`'s capacity gate is the backstop (`layout_model.rs:508-521`; note :288-306).
- Docs and code disagree: `operator-guide.md:472`, `design-review.md:131-133` and
  `v2-open-questions.md:523-524` say #395 admits units "up to half a cartridge". The code
  refuses only above one whole budget; half a cartridge is item 17's design trigger, not a
  cap. Correct the docs with the next change.

## 3. The design tree

Each decision has its question, its options with their costs, a recommendation, and what
it depends on. Round 1 holds only decisions with no unsettled prerequisite.

Two shapes recur:
- **A (slice spanning).** One dar archive. Its slices 1..K go on member 1, K+1..N on
  member 2, and so on. Each member is an ordinary sealed, self-describing tape for the
  slices it holds.
- **B (parts).** When a version is larger than one budget, its file list is partitioned
  underneath the unit into P parts. Each part is its own dar run, so its own stage set and
  archive, and each fits one cartridge. Each tape holds whole parts and restores them
  alone. The vocabulary would gain **Part** (CONTEXT.md). A version has P = 1 part, as
  every version has today.

### Round 1

**D1. What is divided?**
- **A, staged whole.** Staging must hold the whole unit's ciphertext before member 1 is
  planned (§2). Advantages: one source read window (~20 h for 3 TB), after which the
  source may change; one dar catalogue; a single file of any size fits. Costs: staging
  must equal the largest unit (on home2, a bigger staging array); a failed stage loses the
  whole unit's hours (#409, §7); losing any one member breaks a full restore of that copy;
  the heir changes tapes inside one dar stream (D9); a file wholly in member 1 needs the
  last slice, on the last member, for a direct read (research :410-415, MEASURED).
- **A', pipelined.** dar's pipe stays open, and tapectl stops reading it while member 1's
  copies are written. Then slices 1..K are released and staging resumes. Staging is
  bounded at one member. Its costs: the dar process and the source read window last the
  whole multi-day, multi-cartridge run, which is B's cost without B's independence, plus
  a long-lived dar under backpressure, which nothing has tested.
- **B, parts.** Staging, a failed stage and a lost cartridge are each bounded by one part.
  Every tape stays "complete in itself" (`threat-model.md:214-218`, §5). The heir never
  swaps tapes inside a dar run, and each part restores alone. Its costs:
  - **The version is read in P windows** spread over the run. Each part's stage checks
    the source against the snapshot's file list: missing, size and content
    (`staging/validate.rs:57-77`). A changed file refuses its part. The version then
    cannot complete, and a new version re-stages every part. The source must hold still,
    or a filesystem snapshot must hold it, for the whole run. item 75's hook gives that
    only if the hook's snapshot outlives the run (D7).
  - The walk records no hard links (`src/db/files.rs:13`). Keeping a link group in one
    part needs a new walk field. Without it, a group split across parts restores as
    separate copies.
  - A single file larger than one budget is still refused.
  - #12's differential, if it ever opens, needs a reference per part.
  - `write.rs:6592-6668` puts every staged set on one tape. Choosing parts per tape is
    new code.
  - It departs from item 17's wording.

  **The tie-breaker is the CTO's: will staging be sized to the largest unit, or to one
  cartridge?** If to the unit, A is coherent and its single read window is a real
  advantage. If to one cartridge, B dominates and A' is B with worse failure modes.

  **Recommend B**, with staging sized to one cartridge. That is home2's shape, and a
  unit's growth is unbounded where a cartridge is not. Depends on nothing.

**D2. What is a Copy of a divided version?**

Today a Copy is any eligible volume holding a completed write of any stage set of the
snapshot: `copy_count_expr` counts distinct eligible volumes (`policy/coverage.rs:630-661`,
filtered by `eligible_writes`, :553-581) and never reads `write_positions` or
`num_slices`. Under either shape, a version split across two volumes therefore counts **2 copies**
where it has 1. It counts 1 the moment member 1 seals. Retiring either member leaves
`copies_after = 1`, so the absolute zero floor (`operations.rs:763-947`, :772) never
fires. The only slice-aware predicate in the codebase is compact-finish's gate 1
(`write.rs:6274-6291`).

Options:
- **(a) Least-covered piece.** A version's copies are the minimum, over its pieces, of
  the distinct eligible volumes holding that piece, counted as today (cartridge
  distinctness stays the plan-time check, `coverage.rs:1350-1423`). A piece is a part under B and a
  slice under A, through `write_positions`. Members of different copies may combine:
  copy 1's part 1 with copy 2's part 2 restores the unit. That is true under B, whose
  parts are fixed content, and under A, whose staged ciphertext is identical on every
  copy (stage once, write N). The same minimum gives locations (a lost place costs one
  copy of each piece it holds). With one piece it reduces exactly to today's count.
- **(b) Planned sets only.** A set id groups the members of one copy, and a copy counts
  only when all of its own members are live. It is stricter and simpler to explain. It
  undercounts the mixed combinations above, and it needs a set id in the catalog and on
  tape.
- **(c)** A divided version never meets policy, and is always named by `audit`. This is
  safe and useless.

**Recommend (a).** It is the existing rule applied per piece: "a unit is as covered as
its least-covered live version" becomes "a version is as covered as its least-covered
piece".

**The consequence, plainly.** Under A, losing one member loses that copy of the whole
version. Under B, losing one part's cartridge loses that copy of that part's files only,
and the other parts still restore. `threat-model.md` §5 ("No tape needs another to be
read, and losing one costs exactly its own copies") stays true per part under B; under A
it must be amended.

Per #12's closure, a Copy that becomes set-aware lands as its own ADR first. This is that
ADR. Depends on nothing. D1 only changes what a piece is.

**D3. When does spanning ship?**
- **(a)** Build it in 1.3.0 with R4: one minor version and one golden re-pin.
- **(b)** Design both now. Build R4 in 1.3.0 with its unit map already carrying
  `part`/`parts` (P = 1 everywhere), and build spanning when a registered unit passes the
  trigger, as its own minor version with its own re-pin.

The costs. (a) puts the coverage generalization (D2) into the same release as a
read-only index. That generalization touches every consent gate: release, retire,
`cartridge retire`, `mark-erased`, compact-finish, `audit` and rebuild. The one high
finding of the 1.1 adversarial review was a copy-count error, "a rebuilt unsealed tape
counted as a copy" (CLAUDE.md, 1.1.2). With no unit near the trigger (§2), that code
would have no production user. (b) costs a second re-pin and a second minor version
later.

**Recommend (b).** Keep the spanning design ratified and its `requires` name reserved, so
nothing in 1.3.0 forecloses it. Depends on nothing.

**I1. What does R4 index, and for whom?** (#418 R4)

First, a correction to the design review's premise. The review said "the operator
envelope on every tape carries the whole `catalog.db` as of that write, so the newest
tape already indexes every earlier one" (`design-review.md:121-127`). The code says
otherwise: `catalog.db` holds **this write's stage sets only** (`write.rs:1612-1614`,
`build_catalog_snapshot(conn, &stage_set_ids, …)`; `db/catalog_snapshot.rs:16-22`;
`ontape_catalog.rs:753-763`; `volume-format-v2.md:211`), and the research note says "each
tape indexes only itself" (research :395-399). So no tape indexes another, for any key. Only the Heir Kit's catalog snapshot does. R4 is
worth more than the review allowed.

Options:
- **(a)** A tenant index only: each tenant envelope lists that tenant's units on all
  volumes.
- **(b)** (a), plus the operator envelope's `catalog.db` made whole-archive: every
  tenant's units, versions, slices and per-file rows, as of the write.
- **(c)** (a), plus an **operator unit map**: which volumes hold which version and part,
  for every tenant, with no per-file rows of other writes.

The costs: (b) grows every tape by the whole catalog (12.2 MiB per 100k files,
`docs/releases/1.1.0.md:36`, times 2 for the backup), and invites `catalog rebuild` to
import claims about tapes it never touched, which ADR-0001 forbids (claims need contact;
D14, I5). (c) gives the escrow holder "which tapes hold what" from the newest tape alone,
for a few KiB per hundred units.

**Recommend (c).** Depends on nothing.

### Round 2 (each depends on Round 1; seven decisions, so asked in two batches of four and three)

**D4. How is the split chosen?** (depends on D1)
- *Under B (recommended).*
  - The partition is fixed once per version, at `snapshot create`, while the walk is in
    hand. It is never fixed by the drive.
  - Rule: the version's entries in path order, cut into P = ceil(size / part budget)
    parts of roughly equal size. Never cut a directory's direct entries across a
    boundary when that can be avoided, and never a hard-link group (needs D1's walk
    field).
  - The part budget is the largest configured drive's one-tape budget, the figure
    `check_unit_fits_one_tape` uses (`staging/mod.rs:1581-1586`).
  - Even split versus fill-first: a fill-first part 1 (2.42 TB) does not fit home2's
    2.3 TB staging. An even split of a 3 TB unit gives two parts of 1.5 TB, and each
    tape keeps ~0.9 TB for whole units.
  - Record the boundaries, as path ids per part, in a `snapshot_parts` table. Recomputing
    them under a later budget would give different parts.
- *Under A.*
  - K is fixed when member 1's Layout is built, from that volume's `capacity_bytes`.
    Capacity is known only at `volume init` (ADR-0010), and `collection plan` knows only
    the generation.
  - Fill-first to the ceiling. The planned K is the only split.
- **Recommend:** B, an even split, boundaries recorded at snapshot time.

**D5. May a divided version share a cartridge with whole units?** (depends on D1)
- *B:* yes, naturally. A part is a placement item like a unit in the next-fit selector
  (`selector.rs:153-195`). Each tape stays complete for what it holds. Two parts of one
  version on one cartridge is allowed (a later LTO-8 copy). It still counts once per part
  under D2(a).
- *A:* (i) members carry nothing else; (ii) only the last member takes whole units after
  the tail; (iii) a free stream, one tape ending unit X and starting unit Y, which widens
  a lost tape's blast radius to two divided units. Recommend (ii) under A.
- **Recommend:** B, sharing allowed.

**D6. The fence: which `requires` feature, on which tapes, with what in plaintext?**
(depends on D1, D3)
- A fence is needed. A 1.1/1.2 `catalog rebuild` would record a part or member as the
  version's only stage set and count it a copy: `ensure_stage_set` sets
  `num_slices = unit.slices.len()` (`rebuild.rs:1958-1970`), and `ensure_position` assumes
  "every slice the manifest lists is on it" (`rebuild.rs:2101-2102`). A 1.1/1.2 RESTORE.sh
  would restore part 1 and print RESTORE COMPLETE; on a member under A it dies at
  `pick_slices` (`layout.rs:2037-2048`), or dar asks for slice K+1.
- Options: (a) `requires = ["parts"]` (B) or `["spanning"]` (A), in File 0 `[volume]` and
  the seal `[seal]`, **only on a tape that carries a piece of a divided version**; (b) the
  same on every tape written by that release; (c) a per-unit flag in MANIFEST only, which
  fences nothing, because old readers ignore unknown keys (§1.2 rule 3).
- Plaintext identity. Under B nothing more is needed. Under A, a trailing File 0 table
  (`[span] member = i, members = M`) would let `--info` say "this tape alone does not
  restore every unit on it" with no key. It reveals only that a unit larger than a
  cartridge exists, a size fact already accepted (`volume-format-v2.md:345-356`).
- **Recommend:** (a), no plaintext set id, and the feature named for D1's answer. R4
  fences nothing (§5).

**D7. The read window and staging under B** (depends on D1)

Options:
- **(a)** Stage part i, write its copies, release it, then stage part i+1. Staging needs
  one part. The source must hold still across the run.
- **(b)** The same, but a version with P > 1 is staged only under a `collection run`
  whose snapshot hook (item 75) holds one filesystem snapshot from part 1's stage to part
  P's. This needs the hook's lifetime to span a multi-part run (today the hook runs "once
  before the batch" and releases "after").
- **(c)** Stage all parts before any write (needs staging equal to the unit, which is A's
  cost).

**Recommend (a)**, with (b) required for a collection that sets a snapshot hook.
`stage create` alone, outside a run, asks (ADR-0008 Tier 2, the `check_staging_space`
pattern, `staging/mod.rs:1847-1926`) before staging part 1 of a P > 1 version that the
source will hold still: a terminal asks, a run nobody can answer refuses, and `--yes`
confirms in advance. keepsake is static: its newest data mtimes are 2020-2023 (#12 note).

**D8. When is a part's staging released?** (depends on D1, D2)
- Today the release is per unit and all-or-nothing: `release_if_covered` requires that no
  given unit is under its copy floor (`staging/release.rs:82-102`, :118-161; item 63), and
  `clean_staging` needs every write of the set completed (`staging/clean.rs:82-90`). Under
  B with D7(a), part 1 must be released before part 2 is staged, while the unit is still
  at 0 copies.
- Recommend: under P > 1, judge **per part**: release a part's stage set when that part
  meets the version's copy floor, tape-only multiplier included (`coverage.rs:766-780`).
  This amends item 63 (:1467), which fixed the release at the unit's least-covered live
  version.
- Under A the hazard is worse: member 2's `writes` rows do not exist until its own plan
  (`session.rs:242-280`), so the guard could free slices still destined for it. Plan
  every member's rows up front, or make the guard slice-aware.

**I2. The index's shape** (depends on I1)

Options:
- (a) Every live version's isolated dar catalogue: ~19.7 MiB per 181k entries
  (research :441). Usable by an heir with `dar -l`. The heaviest.
- (b) **Text.** `index/units.tsv`, one row per version and part: unit name, uuid,
  version, part/parts, slice count and bytes, each holding volume's label, uuid and
  cartridge serial, and slice `sha256_encrypted` ranges. Plus `index/paths.tsv`: path,
  unit, version, part. awk- and grep-able by RESTORE.sh and an heir, with no new tool.
- (c) SQLite like `catalog.db` generation 3, filtered to the tenant. Compact, but it needs
  `sqlite3`, which is not on the heir's tool list (`volume-format-v2.md:516-559`).
- (d) A `dar_manager` database, ~2.0 MiB per 181k entries per archive (research :443).
  It answers "which archive holds this file" with dar's own tool, but maps archives by
  number, not by tape.

**Recommend (b)**, both files in the tenant envelope (and so its byte-copied backup,
`build.rs:332`), and `units.tsv` across tenants in the operator envelope (I1c). No
physical location goes in: "where the cartridge is" is never on the tape
(`volume-format-v2.md:338-343`).

**I3. Which versions does the index list?** (depends on I1, D2)
- Options: (a) every version ever; (b) every current version with at least one completed
  write on a sealed, unretired volume, plus this write's own; (c) only the newest version
  per unit. Reclaimable and purged versions are not restore targets; (c) hides a live
  older version, and every current version counts as live (CONTEXT.md "Current").
- **Recommend (b).** List each version's holders, quarantined volumes included and marked
  as such: a quarantined medium may still read.

### Round 3 (depends on Round 2)

**D9. The heir's restore across tapes** (depends on D1, D6)

*Under B:*
- Each part is an ordinary restore from its own tape. RESTORE.sh restores part i into
  `--to/UNIT` (item 19). The second part lands in a directory holding the first.
  RESTORE.sh refuses a non-empty `--to` unless `--overwrite`, which is dar `-w`
  (`layout.rs:1811-1812`, `volume-format-v2.md:553-558`).
- Options:
  - (a) allow a non-empty `--to/UNIT` when the manifest says `parts > 1`, without `-w`,
    since the parts' paths are disjoint;
  - (b) a `--part-of-existing` flag.
- Recommend (a). It amends item 18 ("an empty `--to` unless `--overwrite`") for this one
  case. Then:
  - RECOVERY.md and `--list` say "part i of P; restore every part into the same
    directory, in any order";
  - `--find-envelope` lists the other parts' holders from I2's `units.tsv`;
  - shared parent directories take the metadata of whichever part is restored last.

  dar's behaviour on existing directories under `-O -Q -N` is unverified (§9).

*Under A:*
- RESTORE.sh already makes every FIFO before dar starts (`layout.rs:1806-1808`) and feeds
  them in order (`:1830-1844`), with dar in `--sequential-read` (`:1811-1818`).
- So a cross-tape restore is one dar run with a pause: FIFOs for all N slices, feed 1..K
  from member 1, prompt for the next member, accept the inserted tape only when its front
  index holds slice K+1's `sha256_encrypted` (a keyless match that survives a member
  re-written to another cartridge), and feed on. dar blocks on the unopened FIFO meanwhile.
  A spool fallback needs scratch space for the whole unit.
- A sub-option, if A: a copy of the last slice (≤ 1 GiB) on every member, so a `--path`
  restore from one member has the slice dar asks for first (research :410-415).

**D10. tapectl's own restore across tapes** (depends on D9)
- Today `restore unit` reads one volume's slices of one stage set and never checks the
  count (`select_write_positions`, `restore.rs:2724-2784`; `get_write_positions`,
  :2844-2874; "Exactly ONE snapshot version of a unit on one volume", :2679).
- tapectl drives no changer (`src/tape/cleaning.rs:5`). Under B, restore each part from a
  holder the catalog names, prompting for each cartridge by label and serial; under A,
  prompt mid-stream as D9.
- Recommend: `restore unit` and `restore volume` refuse, by name, a version they cannot
  complete from the volumes given. An operator can still restore one part with
  `--part N`.

**D11. Mixing LTO generations** (depends on D1, D4)
- *B:* each part's tape is independent, so mixing is harmless. A part sized for LTO-6
  is refused by an LTO-5 cartridge's capacity gate.
- *A:* member i's share follows its own capacity (D4). But an heir needs drives that read
  every member: an LTO-8 drive reads only LTO-7 and LTO-8 (`media.rs:208`), so an LTO-5
  member plus an LTO-8 member has no common drive. Recommend, under A, that all members of
  one copy share one generation.

**D12. #420's short seal** (depends on D1 and on #420's build)
- Item 9 rules: "every whole unit counts, and the one cut unit is written whole to the
  next cartridge".
- *B:* the rule applies to a part as to a unit, and nothing new is needed.
- *A:* a short seal inside a member's range would be the drive choosing the split.
  Recommend: the member's piece counts as nothing on the short tape (its `writes` row is
  not `completed`), and the whole planned range is re-written on the next member. The
  planned split stays the only split.

**D13. Gates and audit** (depends on D2)
- `audit check_copy_count` (`cli/audit.rs:761`) and `check_location_*` (:821, :891) take
  the per-piece minimum through `coverage.rs`, with no inline SQL (CLAUDE.md, Policy).
- `check_verify_age` uses `MAX` over any holding volume (`audit.rs:940-952`). That lets a
  full verify of one member refresh the clock for the whole version. Change it to the
  minimum over pieces of the newest full readback among each piece's holders.
- A new finding names a version with some pieces sealed and some on no live volume, with
  the missing parts or slice range.
- `versions_at_stake` excludes one volume at a time (`coverage.rs:1507-1547`).
  - Under D2(a), excluding a volume removes its pieces, and `copies_after` is the
    per-piece minimum.
  - `volume retire`, `cartridge retire` (`operations.rs:1260`, per volume at :1384) and
    `mark-erased` (:1742) then inherit the zero floor.
  - `cartridge erase` refuses any volume not given up (`erase.rs:633`), unchanged.
- `copy_distinctness` (`coverage.rs:1350-1423`) becomes per piece.
- Confirm and verify do not change under either shape. The chain walk is per volume and
  keyless (`store.rs:827-879`), and `volume verify` builds its Layout from the tape's own
  front index (`write.rs:4881-4930`). Only the dating of a version's freshness changes
  (the minimum above).

**D14. Rebuild from one member** (depends on D1, D6, I1)
- The rebuild takes units from the operator envelope's manifest (`rebuild.rs:640-646`).
- *B:* each part is a whole stage set, so the rebuild records the part as it is and
  learns P from the manifest. The version reads `copies = 0` for the parts not yet
  rebuilt. This is correct, and D13's finding names them.
- *A:* `ensure_stage_set` must take N from the manifest, not from the slices on this
  tape. adopt_lost's whole-set check refuses any member (`adopt_lost.rs:337-355`), so it
  becomes per member.
- Recommend: the rebuild reports, from I2's unit map, which other volumes this tape names
  and the catalog lacks, as a to-do list for disaster recovery.

**I4. Growth and reporting** (depends on I2)
- `paths.tsv` is about the path bytes plus ~20 B a row: ~100 MB per million files per
  tenant, ×2 for the backup, 0.008% of an LTO-6. RESTORE.sh's work directory holds the
  decrypted envelopes in `${TMPDIR:-/tmp}`, and 1.2.0's space check counts them (item 71),
  so a larger envelope raises that need.
- Recommend: no cap. `volume plan` prints the index size; the capacity gate is the only
  limit.

**I5. What the rebuild does with the index** (depends on I1, I2)
- Recommend: **read, never import**. Rows about tapes this rebuild never touched are
  hearsay (ADR-0001: claims need contact). They feed D14's to-do list only.

## 4. R4, the cross-volume per-tenant index

**What it is.** Each tenant envelope carries an index of that tenant's archive as the
catalog knew it at write time: every live version of every unit, which volumes hold it,
and (I2b) every path. A tenant key holder with any one tape, ideally the newest, learns
which other tapes to find. The operator envelope carries the cross-tenant unit map (I1c).
Today each tape indexes only itself (§3 I1); the Heir Kit is the only cross-volume index,
and it needs the escrow key.

**The decisions** are I1-I5 above: tenant index plus operator unit map; text TSV; every
current version with a live write; no cap, reported; the rebuild reads it, never imports
it. Two more facts bear on it:
- *Staleness.* Each tape's index is as of its own write (`sealed_at`) and knows nothing
  written after it. RESTORE.sh and the guide say "use your newest tape's index".
- *Isolation.* The tenant index rides only that tenant's envelope (t+op+esc). A tenant
  learns the labels of tapes holding its own units, and nothing of another tenant's.

**Does R4 subsume spanning's set manifest?** Partly, and under B almost wholly. Fixed
content goes in the manifest, as correctness needs: `part`/`parts` under B, the full
slice list with hashes under A. Where the other pieces are is R4's unit map, always as of
the write: part 1's tape cannot name part 2's tape, which does not exist yet, and part 2's
tape names part 1's. So R4's `units.tsv` carries `part`/`parts` (`member`/`members` if
D1 is A) from its first release (D3b), and spanning adds no second index.

**One format change, one feature, one re-pin?**
- R4 needs no `requires`:
  - it is a new tar member, which older readers ignore;
  - Rust reads envelope members by name (`envelope.rs:280-300`);
  - RESTORE.sh untars all members and reads named files (`layout.rs:1589`).

  These are the same grounds that let tenant backups and `catalog.db` generation 3 skip a
  fence (`volume-format-v2.md:201-204`, :281-284).
- Only spanning fences anything, and only on tapes carrying a piece (D6a).
- If D3(a), one release and one re-pin carry both. If D3(b), as recommended, 1.3.0 is
  R4's re-pin alone (RESTORE.sh, RECOVERY.md, File 1 text) with `requires = []`, and
  RESTORE.sh's "needs tapectl" text stays as item 73 left it. Spanning's release moves
  that text (item 20).

**Recommend:** R4 in 1.3.0 as I1c/I2b/I3b, with `part`/`parts` columns from the start.

## 5. Format changes, concretely (`volume-format-v2.md` terms)

**R4 (1.3.0, no `requires`).**
- Tenant envelope, new tar members: `index/units.tsv` and `index/paths.tsv`, after
  `catalogs/` in member order (`build.rs:1011-1026`). The backup is a byte copy of the
  primary (§1.3), unchanged.
- Operator envelope, new member: `index/units.tsv`, across all tenants. `catalog.db`
  stays per-write (generation 3, §1.4 unchanged).
- Front index: unchanged.
- File 0, seal: unchanged.
- §2 isolation table: index content is encrypted-only.
- A new §1.5 defines both files: columns, one header line, tab-separated, and an escape
  rule of its own (`\\`, `\t`, `\n` as two-character sequences), since a path may contain
  a tab or newline.
- RESTORE.sh gains `--where UNIT` and `--find PATH`, which read the newest opened
  envelope's index. RECOVERY.md and File 1 name the index.
- What a 1.1/1.2 reader does: untars and ignores the members, and reads as today.

**Spanning under B (its own release if D3b; feature `parts`).**
- File 0 `[volume]` and seal `[seal]` carry `requires = ["parts"]`, only on a tape
  holding a part of a version with P > 1 (§1.2 rule 3).
- `KNOWN_FEATURES` gains `"parts"` in both readers: `format.rs:249` and RESTORE.sh at
  `layout.rs:1133`.
- MANIFEST.toml `[[units]]` gains `part = i`, `parts = P`, `version_bytes`, and
  `part_entries` (the count of entries in this part). `from_toml` already tolerates
  unknown keys (`manifest.rs:138`). The fence keeps older readers off.
- The front index and slice framing are unchanged. Each part is a normal dar archive
  with its own catalogue at `catalogs/<uuid8>_v<V>p<i>.1.dar`. The name carries the part
  so two parts can share an envelope (D5).
- RECOVERY.md and File 1 gain "A unit in parts".
- What a 1.1-1.3 reader does: RESTORE.sh dies at `check_requires` naming `parts` and
  points to the tape's own file 2 (`layout.rs:1137`, :1152-1171); `restore raw-volume`
  and `catalog rebuild` refuse (`raw.rs:163`, `rebuild.rs:538`); the chain walk reads the
  seal's refusal as inconclusive, `SealUnreadable` (`store.rs:938-948`).
- **A gap to close in the same release:** tapectl's `restore unit`/`restore volume`
  never check `requires` (`restore.rs` `corroborate_at_contact`, :1271-1292). Add the
  `FormatClaims` check at that contact.

**Spanning under A (feature `spanning`).**
- The same `requires` placement.
- MANIFEST `[[units]]` gains `num_slices = N` and `member = i`.
- `[[units.slices]]` lists all N slices with `sha256_encrypted`. `tape_position` is
  present only for slices on this tape (key meaning unchanged, rule 4).
- Optional File 0 trailing table `[span]` (D6). §1.1's "`[media]` … must stay last"
  would need its wording changed to "new tables follow `[media]`". The safety argument,
  that RESTORE.sh's `toml_val` is first-match-wins across concatenated tables
  (`volume-format-v2.md:81-84`), holds for any trailing table whose key names are new.

## 6. Catalog, policy, CLI and restore impact, by module

| Module | B (recommended) | A |
|---|---|---|
| migrations | `snapshot_parts(snapshot_id, part_index, first_path_id, last_path_id, planned_bytes)`; `stage_sets.part_index` (NULL = whole) | `writes` per member as today (`UNIQUE(stage_set_id, volume_id)` `001_initial.sql:275` fits); members' rows planned up front |
| `snapshot`/walk | partition at `snapshot create`; hard-link groups from `st_ino`/`st_nlink` (new, D1) | none |
| `staging` | dar `-[` include list per part (dar man: `--include-from-file`; newline-separated, NUL forbidden, no escaping, so a path containing a newline goes as a `-g` argument instead); `check_unit_fits_one_tape` (`mod.rs:1573`) becomes "fits one part", and refuses only a single file over budget | the check is lifted; staging-space check (`mod.rs:1847-1904`) against the whole unit |
| `staging/release.rs` | per part (D8) | slice-aware guard (D8) |
| `collection/selector.rs`, `plan.rs` | parts are placement items; `OversizedUnit` only for an unplaceable single file; `largest_unit` shows parts | spanned unit fills to the ceiling, tail shared (D5) |
| `volume/write.rs` | `find_staged_data` (:6592) chooses parts per tape; `check_staged_slices` unchanged (each part whole) | a BuildUnit takes slices 1..K (`build.rs:219` already allows a subset); `check_staged_slices` (:6687) becomes per member |
| `volume/build.rs`, `manifest.rs`, `layout.rs` | MANIFEST part fields; RECOVERY.md and File 1 text; RESTORE.sh merge into `--to/UNIT` | full slice list; RESTORE.sh mid-stream swap |
| `volume/format.rs` | `KNOWN_FEATURES` += `parts` | += `spanning` |
| `policy/coverage.rs` | `eligible_writes`/`copy_count_expr`/`location_count_expr` (:553-709) grouped by part, then the minimum; `versions_at_stake` (:1507) per part | the same through `write_positions`, generalizing compact-finish gate 1 (`write.rs:6274`) with eligibility |
| `cli/audit.rs` | `verify_age` per piece (D13); new incomplete-version finding | the same |
| `cli/operations.rs` | retire, `cartridge retire`, `mark-erased`, compact-finish inherit through `versions_at_stake` | the same |
| `volume/restore.rs`, `cli/restore.rs` | restore every part, prompt per cartridge, `--part N`; `requires` check at contact | mid-stream prompt; `requires` check |
| `volume/rebuild.rs`, `adopt_lost.rs` | part from the manifest; to-do list from the index (D14) | N from the manifest; per-member adopt |
| `db/ontape_catalog.rs` | `stage_sets.part_index` copied, an additive column (the shape probe is unchanged; rebuild reads the part from the manifest) | stage_slices for the whole set, as today (`:955-960`) |
| R4 | `build.rs` writes `index/*` from new queries in `policy::coverage` (holders per version and part) | the same |

Budget suites: `catalog_budget` pins statements and scans per catalog command. The
per-piece minimum adds a grouping. A changed pin is a cost to explain (CLAUDE.md, Budget
suites).

## 7. #409's resolution under this design

- #409's remaining part is "a late failure discards the whole stage"; its fsync half
  landed in 1.0.7 (#409 comments). Resuming dar cannot fix it: "dar makes an archive only
  from its beginning" (`troubleshooting.md:1080-1081`), and a second run draws new
  internal names (:1082-1089; 2026-10-07 item 11).
- *Under B:* a failed stage costs one part, at most one budget (~16 h at today's
  measured rate), which is exactly today's worst case for the largest admitted unit.
  Spanning makes it no worse. **Recommend closing #409** as "bounded by design: a failed
  stage restarts one part", with the operator guide's source-stillness advice
  (item 68).
- *Under A:* the exposure grows with the unit (~33 h for 5 TB), and nothing underneath
  fixes it. Re-scope #409 as an accepted cost of A.
- In neither branch is #12's differential the answer. #12 bounds the cost of re-archiving
  a changed version. It does not shorten a first stage.

## 8. Test strategy

- **MemStore, ungated:** a split version is 1 copy, not 2; mixed members from two copies
  count 2; retiring one member hits the zero floor; release per part; a rebuild of one
  part gives `copies = 0` with the missing parts named; verify_age is the minimum.
  `tests/format_v2.rs`'s keyless synthetic heir gains a two-tape case (front indexes
  alone; hash-matching across tapes under A).
- **Microcosm scale** (`v2-open-questions.md` §8, bytes shrunk 1024×): a virtual drive's
  `capacity_override` (ADR-0010) makes a small unit larger than one tape, forcing
  P = 2-3 without terabytes.
- **mhvtl gate.** The gate already moves cartridges with `mtx`
  (`scripts/mhvtl-verify-gate.sh:481-489`) on the LTO-8 emulation. A new leg writes a
  P = 2 version as two copies (four tapes), restores through RESTORE.sh from each tape
  into one `--to/UNIT` and diffs, then runs `tapectl restore unit` with the harness
  answering the cartridge prompt by running `mtx`. Under A the same leg does the
  mid-stream swap. Each leg is judged by test name (#471).
- **Golden pins.** `tests/on_tape_golden.rs` pins RESTORE.sh and MANIFEST.toml (:97,
  :276), and a failing pin is a CTO decision. 1.3.0's R4 re-pin covers RESTORE.sh,
  RECOVERY.md and File 1; spanning's release adds a pinned MANIFEST with `parts = 2`.
- **Heir suites.** `tests/heir_restore_sh.rs` fakes `mt`, `dd` and `age`; only the gate's
  `heir_*` legs and home2's real-drive proof run real tools (item 70), and both need the
  two-tape case.
- **Budget suites.** `cost_budget` pins tape motions per restore (a per-part restore adds
  one load per cartridge, nothing else); `catalog_budget` covers the coverage change.

## 9. Facts that need the real drive, home2, or dar 2.7.21

- **[home2]** Staging free space today, and whether it can grow (`home2.profile:61`:
  ~2.3 TB). Decides D1.
- **[home2]** The largest registered unit per planned collection, and whether any source
  over ~1.2 TB is coming (operator knowledge). Decides D3.
- **[home2]** The staging rate after #441, a re-measurement the design review owes; it
  sizes #409's exposure. Also the sizes of L6-0001 and L6-0002's fresh-start contents,
  which the repo does not record.
- **PRELIMINARY, coordinator, dar 2.7.13, direct extraction only (no `--sequential-read`,
  no FIFOs, no `-A`), three files:** two `-[`-list archives of one tree extracted into one
  directory with `-O -Q -N` and no `-w` both exit 0 and `diff -r` equals the source;
  `-[` lists recreate parent directories with their mtimes; the shared parent shows as
  "1 inode(s) not restored (overwriting policy decision)" in dar's summary, not as a
  per-file "(user choice)" line, so RESTORE.sh's fatal grep (`layout.rs:781`) would not
  fire. Still to confirm under RESTORE.sh's sequential, FIFO-fed mode.
- **[dar 2.7.21, no drive]** (B) Under `-O -Q -N` without `-w`, does extracting archive
  2 into a directory holding archive 1's files and their shared parents succeed with no
  prompt and no "not restored (user choice)"? (B) Do `-[` lists create the parent
  directories and their metadata? (A) Does dar wait without a timeout on an unopened
  FIFO for hours? (A) Does direct-access `-A` need slice 1?
- **[real drive, scratch cartridge]** A full B cycle (P = 2, two cartridges) with a real
  heir restore; (A) an unload and load while dar waits mid-restore.
- **[real drive]** #420's post-early-warning block count (design review §3), which
  decides D12's room.
- **[home2]** Whether any LTO-5 or LTO-7+ cartridges will join the fleet (D11).

## 10. Round 1 questions for the CTO

1. **What does a unit larger than one cartridge get divided into?**
   - (A) Its one dar archive's slices, across a set of tapes. Staging must hold the whole
     unit, and home2's 2.3 TB holds less than one LTO-6 budget (2.42 TB).
   - (A') The same, with dar kept running while the first tape's copies are written.
     Staging is one tape, but the source must hold still for the whole multi-day run.
   - (B) Its files, partitioned underneath the unit into parts that each fit one
     cartridge, each its own dar archive. Staging, a failed stage and a lost tape are each
     bounded by one part. Every tape still restores alone. But the version is read in
     several windows, so the source must hold still for the whole run. This shape was not
     among #395's options. It departs from item 17's parenthesis ("splits a unit's slices")
     and from "units are never split by hand"; whether your "hidden underneath" (#12) covers
     it is your call.

   The tie-breaker is whether staging will be sized to the largest unit or to one
   cartridge. **Recommended: B, with staging sized to one cartridge.**

2. **What is a Copy of a divided version?**
   - (a) The version is as covered as its least-covered piece (part or slice), counted
     over eligible volumes as today, so pieces from different copies combine. It reduces
     to today's count when there is one piece. Losing one cartridge costs one copy of the
     pieces on it.
   - (b) Only the members of one planned set count together.
   - (c) A divided version never meets policy.

   Today a split is refused (`check_staged_slices`), so today's count is right. Once a
   version can be split, today's SQL would count it as 2 copies on two tapes, and its zero
   floor would never fire. This becomes the "Copy is set-aware" ADR that #12 required.
   **Recommended: (a)**, with `threat-model.md` §5 amended to "losing one costs exactly
   its own copies of the pieces it holds".

3. **When does spanning ship?**
   - (a) Built in 1.3.0 with R4: one release, one re-pin.
   - (b) Design ratified now. 1.3.0 ships R4 alone, its index already carrying
     `part`/`parts` (`member`/`members` under A). Spanning is built when a registered
     unit passes the trigger, as its own minor release behind `requires = ["parts"]` on
     the tapes that carry a part.

   No unit is near the trigger: keepsake/video is 20% of an LTO-6 and 34% of an LTO-5.
   **Recommended: (b).** The coverage change touches every consent gate and should not
   ride an index release.

4. **What does R4 index, and for whom?**
   - (a) Each tenant's envelope lists that tenant's units on all volumes.
   - (b) (a), plus the operator envelope's `catalog.db` made whole-archive.
   - (c) (a), plus a small operator unit map: which volumes hold which version, for every
     tenant, with no per-file rows.

   A correction first: no tape indexes another today. `catalog.db` holds only its own
   write's stage sets (`write.rs:1614`), contrary to the design review's premise.
   **Recommended: (c)**: tenant index and operator unit map as tab-separated text, no
   `requires` (older readers ignore the new members). `catalog rebuild` reads the map,
   never imports it.
