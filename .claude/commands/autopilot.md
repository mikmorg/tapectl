# Autopilot — drive the tapectl build queue, escalating only real decisions

You are running tapectl's build queue on autopilot: land ONE task end-to-end
per iteration, or escalate it, then continue. **Since 2026-09-15 a "task" may
be a WAVE of up to three file-disjoint items run concurrently** — see the
PARALLELISM entry, which governs; the unit of completion is the wave, run to
landed-or-escalated, never left half-integrated. Invoked as `/loop /autopilot`
(self-paced) or `/loop <interval> /autopilot`. The loop — not this iteration —
decides when work stops; your job each firing is one task, run to *landed* or
*escalated*, never to half-done.

You are the **coordinator/PM**. You dispatch implementation to sub-agents, and
you own review and integration. The user is the **CTO**: they decide design,
not mechanics. Do not ask them anything you can settle from the design docs.

Read before the first pick: `CLAUDE.md` (build rules, reference set),
`docs/design-errata.md` (never implement against a superseded section), and
the normative design set named in the Policy block below.

## Policy (edit this block as reality changes — nowhere else)

- **HOW TO RUN THE QUEUE — CTO rulings 2026-09-15. Read with the queue entry
  below; these six answers govern it.**
  1. **No separate verification pass; verify at pickup.** The adversarial check
     on the issue bodies never ran (session limit, twice), so every citation in
     every issue is a lead. `worktree-agent.md` process step 2 is now the
     substitute and is **mandatory in every worker prompt**: re-confirm the cited
     defect exists before writing anything, and report `DEFECT NOT PRESENT`
     rather than inventing work. A stale citation is a re-scope for you, not a
     problem for the worker to paper over.
  2. **Two issues do not land unattended: #153 and #147.** They change what the
     system tells the operator immediately before data is deleted. Land each on a
     branch and show the CTO the diff **and** the before/after terminal output of
     the affected commands on a fixture; merge only on their word. Every other
     issue follows standing integration policy (cherry-pick, gate, push).
  3. **#160 is split three ways** — #160 (catalog: serial write-once +
     `cartridge relabel`), #192 (tape: File 0 `cartridge_identity_source` + the
     no-serial `--cartridge` requirement), #193 (corroboration at every contact).
     Land in that order. #192 is the only one touching on-tape bytes; it gets a
     worker to itself.
  4. **No config migration.** The strict-keys work (#171/#172/#173) invalidates
     any config in the field, and that was already ruled acceptable. Whichever of
     the three lands last owns two things instead: the load error names the
     offending key and says to re-run `tapectl init`, and `first-run.sh` detects
     an unloadable config and offers to regenerate it.
  5. **The home2 real-drive rehearsal runs EARLY and independently of this
     queue** — whenever the CTO's week allows, not after the queue drains. It
     measures hardware facts (ENOSPC, the #182 MAM unit), which no queued defect
     can distort, and it is the only step that can invalidate an assumption
     before the whole queue of work is built on it. The *final* pre-write rehearsal
     still happens as ratified. Autopilot never starts a rehearsal itself: the
     drive is not on this VM.
  6. **Run the loop; accept the interruptions.** The account's session limit will
     stop the loop mid-queue. That is a pause, not a loss, because each item is
     committed and pushed before the next begins and this Policy block plus the
     memory checkpoint tell a fresh session exactly where things stand. Do not
     shrink the work to dodge the limit.

- **PARALLELISM — up to three workers, never two builds.** This VM has 9 GB of
  RAM and 16 cores: the bottleneck is memory during linking, and two concurrent
  cargo builds OOM-kill each other. The rule is therefore **per-worker
  `CARGO_TARGET_DIR` plus a shared `flock /scratch/tapectl-build.lock` around
  every cargo invocation** (`worktree-agent.md`, Build lock). Reading, editing
  and thinking run in parallel; only linking serializes. **The coordinator takes
  the same lock**: your own gate and the mhvtl gate both build, and a bare one
  beside a worker's link is the failure this prevents. Choose concurrent items
  so **no two touch the same file** — `src/volume/write.rs` is the hot one, named
  by #154, #160, #192, #193, #155 and #147, so at most one of those runs at a
  time. Pure-Markdown items (#180, #181) and pure-`scripts/` items (#156, #179)
  need no build at all and can always ride alongside.

  **Choosing concurrent items: grep the call sites, don't trust the module.**
  A wave is disjoint only if no two items touch the same FILE. H1 was planned
  as disjoint and was not: #159's ruling reached `quick_archive`, which lives
  in `src/cli/operations.rs` beside #153's `mark-tape-only`. Before
  dispatching, grep each symbol an item must change (`grep -rn "<symbol>("
  src/`) and fence on the answer. Where an overlap is real, give the file to
  one worker and have the other REPORT the edit for the coordinator to apply
  at integration — that is what kept H1 running three-wide.

  **The high tier, in waves** (→ means the next wave waits):
  - ~~**H1: #153 ∥ #159 ∥ #154**~~ — **LANDED 2026-09-15, all three closed.**
    Master `e346a5d`, 1107 tests, mhvtl gate 26/26 GREEN. #153 went to the
    CTO review gate and was merged on their word after a display fix. Three
    corrections worth carrying: #153's stated fix was unimplementable (the
    count expressions return SQL strings, not values); a second untested
    multi-`'current'` fixture lurked in `audit.rs` pinning the bug as
    expected behaviour; and #154 had a FIFTH refusal it did not name —
    `into_validated` re-checks capacity against the open drive, so
    `bind_late` now sits immediately before `plan()`. **#154's line numbers
    moved:** `bind_late` is at ~883, not ~748. Any issue body citing the old
    position is stale, not wrong — tell the worker so it does not stop on
    `DEFECT NOT PRESENT` over a line number.
  - *(original H1 plan, for reference)* **#153 ∥ #159 ∥ #154.** File-disjoint — `policy/coverage.rs` +
    `operations.rs`; `staging/` + `unit/` + `collection/`; `volume/write.rs`
    ordering. #153 goes to the CTO review queue when done and **must not block
    the wave** — that is the main reason to run three here.
  - → **H2: #160** alone (`binding.rs`, `cartridge.rs`, and `write.rs`'s serial
    guard). **#154 is merged, so this is unblocked.** Ride #180 alongside —
    but #180 names `cartridge relabel`, so it can only be written once #160
    creates that command. (#181 is already landed and closed, as are #189 and
    #191, which rode H1.)
  - → **H3: #192** alone (on-tape bytes; `layout.rs`, `format.rs`,
    `tests/format_v2.rs`, `docs/design/volume-format-v2.md`). Ride #156/#179
    alongside. Stop and report if `tests/on_tape_golden.rs` turns out to pin
    File 0 — a re-pin is a CTO decision.
  - → **H4: #155**, then **#193**, serially: both sit on `binding.rs`/`write.rs`.
  - → **H5: #147** (needs #153 merged for `operations.rs` and for "zero" to mean
    zero per version). Second CTO review gate.

  **Then #170 alone**, before the rest of the mediums: the `--generation` rename
  touches every CLI file and would conflict with almost any concurrent change.
  Everything filed after it writes the new spelling.

  **After that**, group the remaining mediums and lows three at a time by
  disjoint area, e.g. config (#171→#172→#173 serial, then #174, #186 serial on
  `backend.rs`) ∥ pipeline (#161, #175, #176, #177, #152) ∥ docs/comments (#189,
  #190, #191, #188). Cartridge/tape items (#162, #163, #164, #167, #183, #184,
  #187) and restore items (#158, #165) both depend on the identity set, so they
  come after H4.

- **THE QUEUE EMPTIED ON 2026-09-17, THE MANDATED REVIEW RAN, AND IT REFILLED IT.
  This is the process working, not a setback — read this entry before the one
  below, which describes the original 42-issue queue.**
  Master reached `c309509` with **zero open issues under the label, none parked,
  1425 tests, 0 clippy warnings, mhvtl 26/26, and lifecycle `--all` 338 checks /
  329 passed / 0 failed** — every criterion rule 4 names.
  Policy rule 7 then required the adversarial review of the full diff since
  `5d4cc43` before the CTO's real-drive rehearsal. It ran as a 21-agent
  workflow: ten dimension finders, an adversarial verifier per dimension told to
  REFUTE and to default to "not a defect" when unsure, and a completeness critic.
  Record: `docs/audits/2026-09-17-preproduction-review.md`.
  **29 confirmed, 33 rejected** — the verifiers killed more than half, which is
  the ratio to want. Filed as **#206-#226**: 2 high, 9 medium, 10 low.
  Work them in severity order as usual. **#208 first** (`check_tape_contact`'s
  identity-MATCHES branch never consults the tape's own seal pointer, so a
  sealed tape can be overwritten *without* `--force`) — an ADR-0003 violation
  with a data-loss outcome that **the harness cannot catch**: every ADR-0003
  scenario drives the identity-MISMATCH branch, so a green `--all` says nothing
  about it. Its acceptance must include a lifecycle scenario, not just a unit
  test. Then **#206** (a reverted or reclaimable unit can mint a version
  byte-identical to one already on tape, breaking the premise per-version copy
  counting rests on) — labelled high because its severity is UNKNOWN until
  someone proves reachability, and an unknown in the coverage-misstatement class
  is triaged early or not at all.
  **Three lessons about reviewing, worth more than the findings:**
  1. The dimensions that paid were the ones drawn from what this session kept
     finding, not from generic categories: operator-facing text naming commands
     that would be refused, tests pinning defects as correct, and
     single-derivation discipline. Choose dimensions from your own recent
     misses.
  2. **The completeness critic earned its place.** Four subsystems no dimension
     reached, including `scripts/` — 669 changed lines, the only end-to-end gate
     for the first write, containing four separate commits that fixed the
     harness for asserting states it never created, and **nobody reviewed those
     fixes** (#223). That was my design error: I told reviewers never to RUN
     anything in `scripts/` and then never assigned anyone to READ it. "Do not
     execute" and "do not examine" are different instructions.
  3. A grouped filing pass silently dropped one finding (caught by auditing
     filed-against-found, #222). Group issues if you like, but reconcile the
     counts afterwards.

- **THE PRE-PRODUCTION QUEUE — GitHub label `review-2026-09-13`, 42 issues
  (40 filed 2026-09-14; #160 split three ways on 2026-09-15). Nothing ships to a production tape until it is empty, documentation
  included.** The 2026-09-13 adversarial review
  (`docs/audits/2026-09-13-post-redesign-review.md`, 59 confirmed, ~44 distinct)
  was grilled question by question and **every ruling was ratified on
  2026-09-14**. The decisions are in
  `docs/adr/0012-copies-are-identical-content-cartridges-are-known-by-serial.md`;
  ADR-0010 and ADR-0011 carry dated corrections to their own text; `CONTEXT.md`
  has the vocabulary (*Copy*, *Version*, *Cartridge Identity*, *Barcode*); the
  process rulings are in `docs/handoff.md` "Decisions already ratified". Every
  issue states its ruling as **the** fix.
  1. **Work the label in severity order**, and inside a severity in the
     dependency order the issues state. **The high tier is now eight issues and
     runs in waves — see the PARALLELISM entry above, which supersedes the
     serial order this rule originally gave.** In short: #153 (copy count per
     Version — the one that can lose data), #159 (a Version is minted only when
     content changed) and #154 (bind after the contact check) run together;
     then the identity set #160 → #192 → #155 → #193; then #147 (the inverted
     consent tiers, which needs #153 merged for "zero" to mean zero per
     version).
     The `consent-path` label marks the twelve issues that touch ADR-0008 tiers
     or cartridge identity (#147, #154, #155, #158, #160, #161, #162, #163,
     #164, #165, #192, #193): sequence those together and gate them together.
  2. **Correct facts, never decisions.** A ratified ruling is not re-opened by
     autopilot, even where the audit's own "Fix:" bullet disagrees with it —
     several do, and the issues say so. If implementing exposes a genuine new
     fork that no ruling answers, park it (rule 4). If an issue's *facts* are
     wrong (a drifted line number, a claim the code no longer supports), correct
     the record in the issue and carry on.
     **Verify, don't trust — this applies to the issue bodies themselves.** They
     were drafted against ADR-0012 and their citations re-checked by their
     drafters, but the independent adversarial pass that was supposed to follow
     **never ran** (the account's session limit killed it twice). So every
     file:line in an issue is a lead, not evidence: grep it before acting on it,
     exactly as you would a sub-agent's report. Where an issue says "ratified",
     check it names the ADR paragraph it rests on; a step that only *applies* a
     ruling is ordinary engineering you may exercise judgement on (#166 and #178
     carry explicit comments drawing that line, and are the pattern for the rest).
  3. **Gate per item only for write-path and restore-path changes** (`src/volume`,
     `src/tape`, `src/store.rs`, RESTORE.sh — every issue's Acceptance section
     says which it is). For those, run
     `TAPECTL_GATE_TAPE=/dev/nst1 TAPECTL_MHVTL=1 bash scripts/mhvtl-verify-gate.sh`
     after integrating that item. **Do NOT wrap that command in an outer
     `flock /scratch/tapectl-build.lock` — it deadlocks.** This block told you
     to until 2026-09-16, and doing so hung for 13 minutes before the tree was
     read: `flock` locks are per-open-file-description, not per-process-tree,
     so there is no reentrancy, and the script's own internal
     `flock … cargo build` waits forever on the lock its own ancestor holds.
     The script takes the lock itself, scoped to `cargo build` alone and
     released before the tape legs — which is strictly better than an outer
     wrap, since the tape legs link nothing and must not block a worker for
     two minutes. The rule generalises: **take the build lock around cargo
     invocations, never around a script that takes it for you.** Wrap your own
     bare `cargo fmt/clippy/test` that way whenever a worker is live — those
     are cargo invocations, so the lock is yours to take. Everything else is gated in batches: the
     fmt/clippy/test gate after **every** integration as always, the mhvtl gate
     after each batch of non-tape items lands and before push. The lifecycle
     suite (`scripts/lifecycle-suite.sh --scenario first-year --device /dev/nst1
     --erase short --single-cartridge --i-will-lose-the-cartridge <barcode>`)
     runs once after the `consent-path` set has landed and once when the queue is
     empty — and it takes the build lock internally too, so it is run bare for
     the same reason the mhvtl gate is.
     `--scenario compaction` needs FOUR distinct cartridges and so cannot run
     under `--single-cartridge`; run it multi-slot on mhvtl. **Check `ls -l /dev/tape/by-id/` first**: `scsi-XYZZY_A*-nst` is
     mhvtl (nst1–nst4 at last check), `scsi-HUJ808A5L4-nst` is the REAL LTO-6 and
     is currently detached from this VM, physically on home2. **Never nst0.**
  4. **Park decisions and keep working; never report the queue empty while
     anything is parked.** A parked item keeps its label and gains a comment
     headed "PARKED — needs CTO" with the options and your recommendation; the
     batch goes to the CTO per "Talking to the CTO". "Queue empty" means: zero
     open issues under the label, none parked, gate green, mhvtl gate 26/26,
     and the lifecycle suite green — which as of 2026-09-16 means **a full
     `--all` pass, not just `first-year` 45/45**. "lifecycle 45/45" named only
     the `first-year` scenario, and issue #198 found `compaction` RED on master
     precisely because nothing routine ran the rest. Worse, working #198 showed
     the suite's own "second copy" idiom (`stage create <unit> --version N`
     after a write) is refused by `stage create` and so **cannot ever have run
     green** — `retire-and-reuse` uses it too. Treat the unrun scenarios as
     unknown, not as passing. That item was #203 and it is **CLOSED**: the
     first-ever `--all` run happened on 2026-09-16 and, after two harness
     fixes, is **GREEN — 338 checks, 329 passed, 0 failed, 9 skipped**, on
     master `b865764`, multi-slot on mhvtl with `--erase short`. All thirteen
     scenarios pass.

     **State the MODE or the number means nothing.** `--single-cartridge` — the
     invocation this block used to name as routine — is *weaker* than the full
     one: it sets a `copy_count` allowance that hid a `permute` failure
     completely. That is the same shape as #198's finding that
     `retire-and-reuse` passes under `--erase long` and fails under
     `--erase short`. The measured-green invocation is:
     `TAPECTL_MHVTL=1 bash scripts/lifecycle-suite.sh --all --device /dev/nst1 --erase short`
     (no `--single-cartridge`; `compaction` needs four cartridges). Both
     harnesses take the build lock internally, so run them BARE.

     Two lessons from fixing those scenarios, worth carrying to any future
     harness work: **a red can be propping up a green** (fixing
     `tape-only-and-reclaim`'s placement bug turned two passing checks red —
     they had only passed because an earlier failure meant the unit never
     became tape-only, so `mark-reclaimable` got the 1x rule instead of the
     tape-only 2x); and **a randomised scenario's per-step assertions can be
     seed-dependent** (`permute`'s copy_count check passed or failed according
     to where the RNG placed `write-next-volume`, so a green run proved nothing
     about another seed — the assertion had to move to the end of the walk).

     **NOTHING IS PARKED as of 2026-09-16.** #197 and #199 were parked and both
     were **RULED the same day**; the rulings are in ADR-0012 as a dated
     amendment (`42ef30c`) and repeated as "RULED 2026-09-16" comments on each
     issue, which supersede the "PARKED" comments above them.
     - **#199 — Option 2, as recommended:** `is_write_target` stops answering
       "does this volume hold bytes?" with a status and consults attached
       write/slice rows. Option 1 (rebuild marks the row `sealed`) was
       explicitly rejected: on a label collision it seals a *blank* tape, which
       ADR-0003 then makes unwritable without a real erase. **The trap is
       `volume resume`**, which exists to continue a volume that already has
       `writes` rows — the resumable states must be distinguished from
       `completed` or resume breaks quietly.
     - **#197 — neither option I offered.** The CTO ruled **two columns**:
       `serial_number` written only from a MAM read, a new `operator_serial`
       for the operator's claim. Worth remembering as a pattern: it *dissolved*
       the fork I raised (promote-or-refuse) instead of deciding it, because
       two values that never share a slot never need a precedence rule. When a
       question is "which of these two things wins", check whether they can
       simply stop competing.
     **#147 remains the second CTO *review gate*** — branch `pm-147`, 9 commits
     at `88ed22a`. **It is 125 commits behind master and touches
     `operations.rs` (+990), `coverage.rs` (+368) and `write.rs` (+205)** — the
     three most-churned files of waves 1–4 — so it needs a rebase through the
     consent path *before* the diff and before/after fixture output can be put
     in front of the CTO. Budget that as a task of its own, not as a step.
  5. **Excluded by ruling:** #143 (`config set/add/remove`) and #144
     (`--policy-aware` packing) stay open, unlabelled, and are not this queue.
     #145 (`volume calibrate`) is closed as rejected. **#182** — the MAM
     MiB-vs-MB question — is `needs:cto`, not queue work: it is settled by the
     operator's real-drive rehearsal, and the code fix follows the measurement.
  6. **Docs are in scope**, and so are `scripts/` (first-run.sh and the
     harnesses). The coordinator edits those and the normative docs itself
     (`docs/design/`, `docs/adr/`, `CONTEXT.md`, `docs/design-errata.md`,
     `docs/operator-guide.md`, `README.md`, `CLAUDE.md`); workers still never do.
     When a clap definition changes, `cargo run --example gen_man` and commit
     `docs/man`. `--generation` is the only spelling (#170); no aliases.
  7. **After the queue is empty, two things remain before the first write, and
     neither is autopilot's to skip:** re-run the adversarial review on the full
     diff since `5d4cc43` (same shape as the 2026-09-13 one; record it under
     `docs/audits/`) and work what it finds under the same label; then STOP for
     the CTO's real-drive rehearsal on an expendable cartridge on home2, which
     also settles #182. Do not attempt the rehearsal yourself — the drive is not
     on this VM.
- **THE INVENTORY SURFACE — ratified 2026-09-15, ALL FOUR NOW CLOSED (verified
  2026-09-17).** Four angles onto the catalog, so the archive can be questioned
  from any direction. Ratified over four grilling rounds; the record is the
  artifact https://claude.ai/code/artifact/5f486244-2747-4d25-b5ba-0e5f0d78eca9
  and the trigger is recorded on #14.
  - **#195** (`volume list` / `volume info`) and **#196** (per-copy evidence age
    in `catalog locate`) — shipped and CLOSED. This block described them as
    "NEW, unlabelled" open work until 2026-09-17, when a pre-declaration audit
    of every open issue found them already closed. They were unlabelled by CTO
    ruling, which is why they never appeared in a label query and the staleness
    survived.
  - **#157** (location views show cartridges) and **#184** (`total_load_count`
    wired from MAM) — CLOSED; they carried the label and gated the first write.
  **The gating rule that produced this, kept because it will apply again:**
  `review-2026-09-13` is a DEFECT queue. A convenience feature must never be
  labelled into it, because letting one gate the first production write inverts
  the priority the queue exists to express. "Queue empty" means the LABEL is
  empty — an open unlabelled feature issue is not counted against it. The
  corollary learned here: an unlabelled issue is also invisible to every status
  check, so this block is the only record of it and goes stale silently. When
  declaring the queue empty, audit `gh issue list --state open` in full, not
  just the label.

- **QUEUE STATE 2026-09-17 (evening) — the review-of-the-review is what is left.**
  The 21 issues filed from the 2026-09-17 audit (#206-#226) are all closed except
  #226. What is open now came from the FOLLOW-UP passes over the gaps that audit did
  not reach, so the label is growing rather than shrinking. That is the process
  working; do not read it as regression.
  **Open: #226, #227, #228, #229 (PARKED), #230, #231, #232.**
  - **#229 is PARKED and is the only pending CTO decision.** `collection run --label
    L1 --label L2` cannot complete: `execute_batch`'s write-N-copies loop
    (`src/collection/batch.rs:148-152`) runs `volume_write` against ONE `device` with
    no prompt, eject, pause or changer anywhere in `src/`, so copy 2 always meets
    `claim_mismatch_label` with copy 1's cartridge still loaded. §11 of
    `v2-open-questions.md` settles the SHAPE ("stage once, write N times, cartridge A
    then cartridge B") but not how cartridge B reaches the drive. Options and a
    recommendation (refuse >1 label rather than prompt) are on the issue.
    **It blocks half of #226** — the multi-label on-media scenario cannot be written
    until the feature can complete, which is precisely WHY that test never existed.
  - **#227 and #228 are follow-up review findings**, not audit findings: #227 is
    critic gap 9 (migration 012's rebuild + four CLI modules read narrowly by two or
    three dimensions each and by none as a subsystem); #228 is `main.rs`'s startup
    path.
  - **#230 is a CLASS, not an instance** — `--dry-run` is global and documented,
    honoured by `volume retire`/`cartridge relabel`/`db`, silently ignored by
    `collection run`, `cartridge move`, `volume move` and all of `location`. Two
    reviewers hit it independently in unrelated subsystems, which is what identified
    it. The worst instance stages a whole batch and seals a real cartridge under a
    flag whose help says "without making changes".
  **Ordering note:** #230 touches `main.rs`, `collection.rs` and `location.rs`, so it
  conflicts with #228 (`main.rs`), #231 (`location.rs`) and #232 (`collection.rs`).
  Run it alone on the code side; review passes are read-only and always ride along.

- **THE 2026-09-17 REVIEW'S CRITIC GAPS ARE NOW FULLY RECONCILED (2026-09-17).**
  The completeness critic listed NINE gaps; the queue only ever carried issues
  for eight of them, and nothing recorded which. Reconciled at the #214 pickup:
  gap 1 `scripts/` → #223 (closed), gap 2 `destination_budget` → #224 (closed),
  gap 3 version-minting → #159/#206 (closed), gaps 4/5/6 Collection +
  `location.rs` + `main.rs` → **#225 (open)**, gap 7 on-media coverage →
  **#226 (open)**, gap 8 the seal-pointer data-loss finding → #208 (closed),
  **gap 9 → nothing.** Filed 2026-09-17 as **#227** (migration 012's table
  rebuild — the only rebuild in the range, indexes and `foreign_key_check`
  unverified — plus `catalog.rs`/`report.rs`/`archive_set.rs`/`snapshot.rs`
  read narrowly by two or three dimensions each and by none as a subsystem).
  **Had this not been checked, "queue empty" would have been declared false.**
  The general rule, now twice learned: when an audit produces a list, reconcile
  the list against filed issues item by item before trusting the queue count.
  Filing in a batch drops entries silently (#222 was caught the same way).
  **Rules if this work is ever extended:** catalog-only, never opens a drive (so
  no mhvtl gate is owed and it stays usable on a rebuilt machine with no
  `backend add`); every status visible by default, since ADR-0011 makes retired
  mean unfit-to-write and not unreadable; unplaced rows appear as
  `(not placed)`; counts route through `policy::coverage`, escrow through
  `policy::escrow`, evidence age through `policy::evidence` — never re-derived,
  and never `audit.rs`'s `verify_age` query, which would make never-verified
  volumes vanish. No new top-level command: `browse` is taken twice over and
  `CONTEXT.md` reserves `library` for the changer.
  **Declined and still declined:** TUI, FUSE mount, web view, daemon (#13, #14).
  The changer (`mtx`, slot addresses) is deferred until a real fleet exists.

  8. Standing constraints, unchanged: no new dependencies; never weaken a gate,
     test, clippy setting or `EXPECTED_FAIL`; `tests/on_tape_golden.rs` is never
     re-pinned (a byte change is a CTO decision); `git stash` is banned in every
     form; cargo synchronous, never backgrounded; no GitHub closing keywords in
     commit messages; workers never touch `/dev/nst*` or `/dev/sg*`; the real
     `~/.tapectl` is never touched from this VM; migrations are forward-only and
     the next free number is **017** (016 is
     `016_cartridge_operator_serial.sql` from #197). This line has now been
     stale THREE times — 014 until the #184 worker caught it on 2026-09-16, 015
     until the coordinator caught it later the same day, 016 until 2026-09-17.
     It will go stale again: **verify with `ls src/db/migrations/ | tail -1`
     before writing one, and do not trust this line.**

- **DEEPENING QUEUE 2026-09-11 (attended; CTO said "do all") — COMPLETE, all seven + C2b landed; real-drive pass #4 45/45 on 2026-09-12.**
  The CTO asked for an architecture review and then `/autopilot do all`. The
  queue is the seven candidates of the architecture review — not in the repo
  (report: `/tmp/architecture-review-20260911-180729.html`, artifact
  https://claude.ai/code/artifact/5625257c-2dcb-43ee-aaf5-6c679cb5c3f1).
  Take in this order — dependency and blast radius, not preference:
  1. ~~**C1**~~ — **LANDED `..7b26f57`** (sonnet worker, 7 commits + 1
     coordinator). `policy::escrow::stage_set_coverage(conn, Scope, escrow)`
     owns the SQL; `Scope::{Unit, AllUnits}` filter volumes by `in_service`
     and skip `encrypted = 0` (audit's `encryption` violation owns that);
     `Scope::StageSets` is the write pre-flight (no volume join, fail-closed
     on a missing id); `Scope::UnitAnyVolume` is `catalog locate`, which
     lists retired cartridges on purpose (#57) and so answers escrow for
     every row. The write pre-flight's private JSON parse and its two
     divergent reason strings are gone. **User-visible:** quarantined
     volumes no longer appear in audit/report escrow findings (they are not
     in_service; Copy is defined as unquarantined) — flag to the CTO as FYI,
     not a question. 760 lib / 878 total, gate GREEN.
  2. ~~**C3**~~ — **LANDED `bed8338..ed14c92`** (cherry-picked from a sonnet
     worker, 4 commits). `volume::manifest::Manifest` with `to_toml` (the
     moved hand format, byte-identical — `tests/on_tape_golden.rs` passed
     unchanged) and `from_toml` (serde, tolerant of pre-#134
     `layout_version`); `layout::ManifestUnit`/`envelope::*` are re-exports,
     `EnvelopeManifest` stays a nested view because `rebuild.rs` reads
     `.manifest.manifest.tenant`; `tape_position` is i64 on both sides. Two
     of the three awk fixtures now come from the writer; the #135 decoy
     fixture stays hand-typed because the writer cannot emit a `name` inside
     a slice block. 749 lib / 867 total, gate GREEN, CI read after push.
  3. ~~**C6**~~ — **LANDED `..6551b46`** (sonnet worker, 3 commits). Six
     `pub(crate)` consts in `volume::restore_script` (`AWK_PARSE_FILE_LIST`,
     `AWK_CHECK_FILE_LIST`, `AWK_FIND_ENVELOPE`, `AWK_MANIFEST_HAS_UNIT`,
     `AWK_UNIT_LIST`, `AWK_SELECT_VERSION`) spliced by `.replace` into the
     template; six plain field-extraction awks stay inline. The golden hash
     passed unchanged; three new tests pin fragment presence, no surviving
     placeholder, and no apostrophe in any fragment. Tests reference the
     #131/#133/#135 rules by name now. 763 lib / 881 total, gate GREEN.
  4. ~~**C7**~~ — **LANDED `..ae2c4ee`** (sonnet worker, 4 commits).
     `volume_identify`, `read_slices`, `compact_read` and `restore_raw` take
     `&mut dyn Store`; the CLI opens `TapeStore::open_read` once (the
     `Compact` arm scopes its read store so the fd closes before
     `compact_write` reopens the device); `restore_raw`'s private test twin is
     gone — it had silently dropped the mismatch `tracing::error!`, which is
     what a twin is for. `rebuild_from_volume` already sat over
     `rebuild_from_store` and was left alone. One line in `tests/mhvtl_e2e.rs`
     changed for the new signature (standalone commit). Write sessions'
     `TapeStore::open` (the capacity seam) untouched. 769 lib / 887 total.
  5. ~~**C5**~~ — **LANDED `..e6d800c`** (sonnet worker, 5 commits).
     `db::ontape_catalog` owns SCHEMA, `Generation::{Original,
     WithOwnershipAndReceipts}`, `detect_generation` (probes `tenants` +
     `key_fingerprints`; a file with one but not the other is refused as
     corrupt/hand-edited, naming which half is present), typed rows,
     `write`/`read`; `catalog_snapshot` is a compatibility surface;
     `rebuild::Supplement::load` is a caller. Shape unchanged. **Queued CTO
     question:** a generation STAMP inside catalog.db (bytes on tape) — not
     needed today; the probe is the reader. 766 lib / 884 total, gate GREEN.
  6. ~~**C2**~~ — **LANDED `..d13de21`** (sonnet worker, 4 commits). The
     JSON shape of every row listing was PINNED first (one test per struct,
     from the pre-refactor `json!` arm), then eight row structs got typed
     fields + `Serialize` with `#[serde(rename)]`/`serialize_with` so both
     views derive from one struct; `catalog.rs` no longer reverse-parses its
     own display strings. Three commands (`tenant list`, `key list`, `unit
     list`) keep serializing `db::models` directly — richer than the row,
     so wiring the row would change the contract either way; they got
     `Serialize` + a parity pin and a doc comment. **Queued CTO question:**
     twelve table-only columns have no `--json` counterpart (e.g.
     `FileRow.modified`, `SnapshotRow.{files,size,created}`); adding keys is
     a contract change. 777 lib / 895 total; no tape path, no gate owed.
  7. ~~**C4**~~ — **LANDED `..308b664`** (sonnet worker, 2 commits).
     `cli::audit::CHECKS` — 11 rows (8 per-unit with their statuses, 3
     archive-wide), one runner; `policy_unresolvable` is what the runner
     emits when `resolve` fails, not a row. Findings text and order
     identical; 45 tests unchanged + 4 (the #138 scope table is now a
     property test). One edge made real: `audit --unit <retired-unit>` now
     runs no checks — the #138 table always said `retired: no`; the old code
     only honoured that for the bulk path. 767 lib.
  **All seven landed.** CTO batch answered 2026-09-11 evening: (Q1) **no
  generation stamp** in catalog.db — the shape probe is the reader;
  (Q2) **add the twelve table-only columns to `--json`** — additive, pins
  updated deliberately (follow-up **C2b**, LANDED — twelve keys, raw values, names following each command family's existing `--json` conventions; pins extended in place); (Q3) **keep
  `in_service`** for escrow findings — quarantined volumes excluded, per
  Copy's definition.
  **#139 LANDED `..b10b089`** (2026-09-12, sonnet worker, 5 commits): `init
  --escrow-public-key <KEY_OR_FILE>` adopts the original escrow identity at
  init through `key::adopt_escrow_recipient`, shared with `key import
  --escrow` so the two cannot drift; the key is parsed BEFORE the first side
  effect, so a bad value leaves nothing on disk (verified: exit 2, empty
  home); conflicts with `--no-escrow`; `--json` keys unchanged; man pages
  regenerated, zero drift; five smoke tests. The DR recipe is now one
  command. 784 lib / 907 total. **The queue is empty; the software side is
  at a plateau — what remains is the CTO's: the Heir Kit ceremony and the
  first production write (`docs/handoff.md`).**
  **Lesson (C4 caught it in the act): `git stash` is shared across every
  worktree of one `.git`.** A worker popped another worker's entry. No loss —
  it noticed and restored — but the template now bans `stash`; baselines run
  in a detached worktree or from a patch file.
  Rules for this queue: one worktree sub-agent per item (sonnet), coordinator
  reviews and cherry-picks, full gate after each, mhvtl gate for anything under
  src/volume, src/tape, src/store.rs or generated RESTORE.sh, push, READ CI.
  Golden tests for C3/C6 are written by the coordinator FIRST and must pass
  before dispatch, so a worker cannot move bytes and re-pin.
- **RUN 2026-09-11 (unattended, then attended) — CLOSED 2026-09-11 evening.**
  Three rounds: #125/#126/#130/#132/#133 + symlink restore; #134/#135/#136
  (`catalog rebuild`); the design review's six items (#138, third escrow
  state, catalog.db as rebuild source, attestation, DR docs, lifecycle arm d).
  Real-drive passes #1–#3 all 45/45 with bytes read back off the cartridge.
  Full account in `docs/runs/2026-09-11-unattended.md`; rules that outlived
  the run are in `.claude/skills/unattended-run/SKILL.md`. Open follow-up:
  #139 (`init --escrow-public-key`).
- **DEVICES MOVED — the 2026-09-10 entry below is superseded on this point.**
  The VM reboot happened. The **mhvtl gate is GREEN 26/26** against an empty
  `EXPECTED_FAIL` manifest and is fully usable again; real-LTO-6 validation no
  longer substitutes for it. Resolve drives by serial through
  `/dev/tape/by-id/`: `scsi-HUJ808A5L4-nst` is the **real HP LTO-6**
  (now `/dev/nst0`, cartridge `EW7VWMVKF6`, expendable), `scsi-XYZZY_A1..A4`
  are **mhvtl** (now `/dev/nst1`-`nst4`, changer `/dev/sg4`, 43 slots loaded).
  The numbers swapped across the reboot and will swap again — a gate leg that
  defaulted to `/dev/nst0` read the real drive for three runs and passed
  against the wrong tape. Always set `TAPECTL_GATE_TAPE`.
- **RUN 2026-09-10 (real-LTO-6 follow-through) — CLOSED 2026-09-11.** Landed
  #115-#124, #127-#129 and #131; lifecycle suite 277/2/27; mhvtl gate GREEN
  26/26; CI green at `7150132`. Full account in
  `docs/lto6-session-journal-2026-09-10.md`. Its device and gate claims are
  superseded by the entry above; its **lessons stand**, chiefly: two of the
  three most serious hardware findings were in the *measuring instrument*, and
  every failure #128 blamed on single-cartridge media was a real bug. Any
  harness number arguing for a design change gets a controlled re-measurement
  first, and "the test environment can't do this" is a hypothesis, not a
  diagnosis — the cheap discriminator is re-running multi-cartridge on mhvtl.
  CTO batch answered 2026-09-10: (Q2) pre-escrow-tape copy →
  `--allow-missing-escrow` override, default refuse. (Q3) `init` creates the
  escrow identity. (Q4) backend error+example now, `backend add` is #126.
  Recorded in design-errata §2.16/§7.
- **CTO BATCH ANSWERED 2026-08-01 (five decisions, recorded in ADR-0009,
  commit 9ee0658). #69 IS UNBLOCKED AND IS NEXT — it is the only open
  `severity:high`.** The deferral covers the **ceremony** (printing,
  tamper-evident envelopes, distribution), NOT the command; #68 is closed
  and the escrow recipient is live in the write path. Ratified:
  1. Build `key escrow-kit` end-to-end, stop at the printed artifact.
  2. The bundle is the **full `tapectl.db`**, not #83's filtered
     `catalog.db` — the filtered schema has no `locations`/`cartridges`, so
     it tells an heir what exists but not which cartridge to fetch. Safe
     because the DB holds no secret material (only `tenants.public_key`;
     private halves are files under `keys/`).
  3. Output is **self-contained HTML (inline SVG QR, print styling) + a
     plain-text `COVER.txt` twin** carrying the same words and the key in
     retypable Bech32. The `.txt` is the decades-scale artifact. PDF was
     rejected (heavy dependency, less inspectable).
  4. **Kit staleness is recorded and warned about advisorily** — an
     `events` row on generation, and an `audit` check at exit 1, never 2,
     when volumes were sealed since. ADR-0005 names that failure and
     rejects *enforced* discipline; an advisory check is neither
     enforcement nor memory, so it sits in the gap. ADR-0004 holds.
  5. #113's fix was the deterministic park hook — **landed first on
     purpose**, so the gate is trustworthy before it is used to validate
     #69. Fix the measuring instrument before the measurement.
  Open risks named to the CTO and not yet decided: the **QR crate is a new
  dependency** in a tree that pins deliberately (pick one with no
  transitive image stack, pin it), and #69 grows `audit` to a **seventh**
  operator-facing check. #69 lands in `src/crypto/` ⇒ full mhvtl gate.
- **Mode: PHASE 3 (from 2026-07-31), CTO-scoped.** Phase 2 ran to completion
  — every phase-2 issue closed except **#69 (Heir Kit)**, deferred by the
  CTO for a physical step (printing, tamper-evident envelopes) no agent can
  perform. R&D exited 2026-07-28; the design docs are settled reference.
  Gate `EXPECTED_FAIL=()`, so zero slack — any failure is a hard stop.
  The three hard stops are untouched and remain CTO calls: the first
  production write, the LTO-6 hardware session, and #69's physical step.
- **Phase-3 queue, re-triaged 2026-07-31 against shipped code. Take in
  this order — the ordering is a dependency, not a preference:**
  1. ~~**#70**~~ — **LANDED 2f62aad, closed 2026-07-31, CI green.**
     `contrib/systemd/` (wrapper + service + timer) plus an operator-guide
     section; no Rust change. Three decisions worth not re-litigating:
     `report verify-status` KEEPS exit 0 (the verification-age check with
     real exit codes is already one of `audit`'s six §2.20 checks — do not
     give a report command an exit code to serve a wrapper); **audit exit 1
     is a unit SUCCESS** via `SuccessExitStatus=1`, only exit 2 fails,
     because alerting on advisory warnings makes the audit blocking by the
     back door (ADR-0004); and healthcheck pinging **fails open** — unset
     URL, missing curl, or a failed ping never changes the run's exit
     status (same rule as #97's dar probe). The service must set `User=`
     AND `Environment=HOME=` explicitly: `src/config.rs` resolves the
     config root purely from `$HOME` and a systemd service inherits none.
     Documented caveat, not a defect: an audit firing during a
     `volume write` logs a spurious "recovered orphaned write sessions"
     event, because `db::open`'s sweep marks `in_progress` sessions
     `interrupted` (resumable, revalidates on resume — nothing is lost).
     The original entry, for context: **#70** (systemd timer for `audit` +
     `report verify-status`). Its
     stated hard dependency #45 is CLOSED and verified in code
     (`fsck_exit_code`, `exit_if_nonzero`, `init_tracing` → stderr), so the
     scheduled jobs can signal failure. #13 permanently rejected the daemon
     shape; a **timer is the sanctioned alternative** — do not drift toward
     a daemon. Metadata-only: no tape, no restore path, no new deps.
     Scheduling changes *when* the audit runs, never *whether* it blocks.
  2. ~~**#73**~~ — **LANDED `654093a..4ac27aa` (11 commits), closed
     2026-07-31. Gate GREEN 26/26, CI green, 694 tests (was 656).**
     Migration 007 (`locations.kind`, `archive_sets.warehouse_copies`,
     `volume_deposits`), the `warehouse_copies` knob through all three
     policy layers, `location add --kind`, `volume deposit add|list|
     remove`, deposits first-class in every copy/location derivation, an
     `audit` check, warehouse evidence as its own class, operator-guide
     section. What to know before touching any of it:
     - **A warehouse copy is a DEPOSIT of an existing sealed volume**, in
       `volume_deposits` — never a new `volumes` row, never a change to
       `volumes.location_id` (single-valued on purpose: "where do I go to
       fetch this cartridge"). The migration header argues this at length
       so nobody "unifies" it into a `volume_locations` join table.
     - **No checksum / uri / credential columns**, ever. tapectl did not
       perform the upload, so a typed-in checksum is a claim about a
       claim; `locations.description` holds `s3://bucket/prefix`.
     - **`policy::coverage` now owns `copy_count_expr` /
       `location_count_expr`.** SIX sites hand-wrote the location count
       and would each have under-counted a warehouse copy — #96's failure
       mode in a new dimension. Twelve call sites route through them now.
       Never inline either expression again.
     - **`unit mark-tape-only` and `snapshot mark-reclaimable` now pass on
       one tape + one deposit.** Intended (ADR-0006: the catalog claims
       the copy so it can reason about it; ADR-0004 keeps it advisory),
       but both greenlight deleting local data, so the evidence line at
       those moments names the deposit and says it is never re-verified.
     - **A deposit stops counting when its source volume stops being
       `sealed`.** Conservative by choice: a deposit can never be the
       thing that keeps a unit looking covered after its tape went bad.
     - Residuals filed: **#100** (`volume move` still accepts a warehouse
       destination) and **#101** (`volumes.storage_class` did NOT "become
       meaningful with #73" as errata row 32 claims — decide populate/
       surface/drop and correct the errata row in the same commit).
  3. ~~**#72**~~ — **LANDED 7a92dce, closed 2026-08-01, CI green.** The
     documented rclone/aws-cli procedure (native S3 stays out; ADR-0006 is
     NOT amended and `WarehouseStore` stays open on the same seam). Docs
     only, composing `restore raw-volume` (#63) + `volume deposit add`
     (#73). **The retrieval path was verified end-to-end on a real dumped
     volume before it was written**, which caught two traps that
     transcribing the on-tape system guide would have got wrong:
     - **The front index and seal marker dump as FULL NUL-PADDED tape
       blocks** (524288 bytes, 520311 NUL) — they are the two files that
       cannot carry their own hash, so there is no length to truncate to.
       Content files ARE trimmed exact. Applying `tr -d '\0'` uniformly,
       or omitting it, is wrong for half the files.
     - **Slice number is NOT tape position, and slices do not start at 0**
       (position 8 on the verified volume, after the envelopes). The
       mapping is each `[[units.slices]]` block's `number` +
       `tape_position` in the envelope MANIFEST.toml. Filename order
       yields a broken archive.
     Proof: decrypted slice matched manifest `sha256_plain`; `dar -x` was
     `diff -r`-identical to source. The guide states which of its own
     lines were executed (tapectl/age/dar) and which were not (rclone/aws
     — not installable from apt here). Heir-kit obligation recorded on #69.

  **THE PHASE-3 QUEUE IS EMPTY (2026-08-01), and the three CTO calls it
  raised have all been executed:**
  - **#20 CLOSED as delivered.** All 15 children closed, gate GREEN 26/26
    with `EXPECTED_FAIL=()`. #26 (EOT) was *rejected* by ADR-0007, not
    built. The block-size question survives independently in
    `v2-open-questions.md` §5 — closing the epic did not lose it.
  - **#65/#66/#67 AUDITED, DECOMPOSED, CLOSED.** Full item-by-item verdict
    tables are recorded as closing comments on each. Roughly a third were
    already DONE or SUPERSEDED — the usual rate; **#66's `key export --qr`
    rider was correct**, and **#65's `--depth` item was invalid as
    written** (no such flag exists; it is a vestigial fn parameter).
  - **#101 DROPPED the column** (migration 008, `7b546f4`), errata row 32
    corrected in the same commit. Note **migration 008 is now taken** —
    #107 (`tape_alerts`) is 009.
  **PHASE 4 = the decomposed backlog, severity then value:**
  ~~**#102**~~ — **LANDED 57d731e, closed 2026-08-01. Gate GREEN 26/26, CI
  green, 704 tests (was 694).** `RestoreScratch` RAII guard removes
  `.tapectl-restore-tmp` on every path out of `restore_unit`. Keep three
  things: it is **not** `tempfile::tempdir()` on purpose (the scratch dir
  must sit under the destination — dar extracts from it, and a slice set
  can be hundreds of GB, so a tmpfs `/tmp` is the wrong home);
  removal failure **warns naming the path and does not escalate** (the
  operator must know plaintext remains, but a cleanup error must not mask
  the original failure, and `Drop` cannot return one); and the **wiring
  test** is the one that matters — deleting only the `let _scratch = ...`
  binding fails exactly that test and none of the three guard tests. The
  mhvtl gate does NOT cover this path: its restore legs use fresh
  destinations and succeed, so they never fail partway.
  ~~**#103**~~ — **LANDED b46ffe1, closed 2026-08-01. CI green, gate GREEN
  26/26 (see the flake note below), 726 tests.** New `src/naming.rs`:
  `validate_tenant_name` / `validate_unit_name` / `validate_volume_label`,
  called at the FOUR creation sites (`tenant add`, `unit init`, `unit
  rename`, `volume init`) and **never on the load path** — which is what
  makes it safe for existing installs and dissolves the
  grandfather-vs-migrate question entirely. Keep in mind:
  - **It was a real defect, demonstrated:** with validation removed,
    `add_tenant("../../escaped")` returns `Ok(1)` and writes BOTH PRIVATE
    KEYS two directory levels above `keys/`. That capture is the negative
    control.
  - **Unit names are hierarchical** (`tv/breaking-bad/s01`, and
    `collection sync` generates `{collection}/{relative path}`), so `/` is
    a validated SEPARATOR for units and a rejected character for tenants
    and labels. Never "simplify" the three functions into one.
  - **Volume labels were the same defect at a third site** the issue did
    not name (`{staging}/clone-{from_label}-{unit_name}`).
  - Allowlist, not blocklist; leading `-` and `.` also refused.
  ~~**#113 — THE GATE FLAKE IS FIXED.**~~ **LANDED f5e716e, closed
  2026-08-01. Five consecutive gate runs GREEN 26/26, CI green, 718 tests.**
  Fixed at the source, not tuned: `TAPECTL_TEST_PAUSE_AFTER_PLAN` names a
  marker path, `execute` parks at exactly the BOT state and creates that
  file, and the gate waits for **the file** — a fact, not a timing guess —
  before signalling. No sleeps anywhere in that arm. What to keep:
  - **The env var is read at ONE boundary (`park_marker_from_env`) and
    passed into `run_entries` as a parameter.** Not decoration: env vars
    are process-global, so an in-process test that set one would leak into
    every other test running in parallel in the same binary — the exact
    nondeterminism being removed. Never move the lookup back inside.
  - **A test hook on the production write path is justified, not hidden:**
    runtime check because `#[cfg(test)]` can't reach integration binaries
    (#87); unset ⇒ not one branch taken (pinned by a test); warns loudly so
    it can never park silently on a real tape; times out after 120s into a
    normal write so a gate that never signals fails on its assertion rather
    than hanging.
  - **One green run proves nothing about a 1-in-3 flake.** Five consecutive
    full runs was the bar (~13% under the old behavior). Use that standard
    for any future flake fix.
  - The two "retune the sleep" remedy strings were stale and are fixed; they
    now name the park hook and forbid widening the bound.
  **Historical note — the flake as originally recorded:**
  `resume_bot` failed 1 run in 3 with "expected between 0 and 0
  confirmed-written positions, got 1". It is structural, not tuning: the
  arm waits on `writes.status='in_progress'` (true at `plan()`, before any
  entry) and then signals, so the writer can confirm entry 0 inside the
  delivery window. **Do NOT widen the bound to `0 1`** — that converts arm
  1 into a duplicate of `resume_midwrite` and deletes the BOT case. Also
  note the assertion's own remedy text ("retune the sleep") is stale;
  there is no sleep any more. If the gate reds on `resume_bot`, check
  #113 before assuming your change caused it — but never just re-run
  until green.
  ~~**#104**~~ — **LANDED e1def28, closed 2026-08-01. CI green, 712 ungated
  tests (was 704). No gate — no tape-path file touched.** `db fsck` now
  collects every `PRAGMA integrity_check` row, shares one transaction across
  both `--repair` DELETEs, and logs a `system`/0/`db_fsck_repair` event
  **inside** that transaction. Three things to keep:
  - **The clean-case predicate is `len()==1 && [0]=="ok"`.** A healthy
    SQLite DB returns exactly one row reading `ok`, never zero — so
    `integrity_ok` must not be derived from emptiness, from
    `contains("ok")`, or from "first row is ok" (which *was* the bug).
    Fixing the loop and leaving that test naive only relocates it.
  - **`db::open*` sets `PRAGMA foreign_keys = ON`, so orphans of this shape
    can no longer be created through tapectl at all.** The orphan checks
    serve older, hand-edited, or partially-restored databases. The fixture
    drops the pragma only to insert, then restores it so the repair runs
    under production FK semantics — don't "simplify" that away.
  - **`FsckReport::repaired` counts deleted ROWS, not categories.** Nothing
    reads it (`fsck_exit_code` does not); it renders as `repaired=N`, where
    a category count is meaningless. The fixture is deliberately asymmetric
    (2 writes + 1 slice) so the two cannot be confused.
  Not covered, stated rather than implied: the genuinely-corrupt multi-row
  integrity path needs a real damaged file the ungated suite cannot
  synthesize portably.
  ~~**#105**~~ — **LANDED f41c339, closed 2026-08-01. CI green, 715 ungated
  tests (was 712). No gate — only `src/policy/mod.rs` touched.** Every layer
  of `policy::resolve` now propagates instead of falling through to the
  weaker system defaults. Four things to keep:
  - **THREE sites, not the two the issue names.** `required_locations` was
    `if let Ok(arr) = serde_json::from_str(..)`, so corrupt JSON yielded an
    EMPTY vec — "no locations required" — the same downgrade in a third
    place. Checked all three writers first: each stores a `serde_json`
    array or NULL, so a non-NULL unparseable value is only ever corruption.
    Had any writer been able to store `''`, propagating would have newly
    failed healthy units.
  - **The absent dotfile MUST stay silent.** `dotfile_path.exists()` is the
    whole absent-vs-present split (#92: absent = defer upward). A test pins
    it, because conflating the two makes every unit without a dotfile fail.
    TOCTOU (present at the check, gone at the read) deliberately errors.
  - **A dangling `archive_set_id` is MAPPED, not propagated raw** — bare
    "Query returned no rows" reaching an operator through `audit`'s message
    is unactionable, so the error names the unit and the id.
  - **A pre-existing test asserted the defect as intended behavior.** Its
    own comment showed the real intent was panic-safety, which `Err`
    satisfies as well as `Ok` — so the assertion was TIGHTENED, not the
    behavior loosened. Worth expecting more of these: a silent-fallback
    fix will often collide with a test that pinned the fallback.
  Residual filed as **#114** (`audit`'s `policy_unresolvable` action line
  always blames the dotfile; now often wrong). Deliberately NOT folded in —
  the honest fix needs a policy-source discriminant on the error, and the
  cheap alternative trades wrong advice for vague advice.
  **#106** (fire-risk global threshold), **#107**
  (`tape_alerts` — gate, migration 009), **#108** (staging-clean permanent
  orphan — gate), **#109** (`--home`/`--config` — looks gate-free, is NOT:
  the gate script depends on the hijack), **#110** (grouped one-liners),
  **#111** (harness hygiene), **#112** (`main.rs` extraction), **#100**
  (`volume move` to a warehouse). Every one was verified against shipped
  code on 2026-08-01, so for once the backlog is NOT stale — but re-check
  anything that sat unworked for a while.
- **Not in the phase-3 queue, but needing a CTO call soon:** **#20** is an
  open `severity:high` epic whose stated acceptance — *"verify-gate legs 1
  and 4 green with EXPECTED_FAIL empty"* — is literally the current state,
  with its Store-seam child #71 closed and the typestate session shipped in
  the v2 regear. It looks complete-but-unclosed (the #27 pattern) and is
  misrepresenting project state. Surface it; do not close an epic
  unilaterally. The LOW umbrellas (#65/#66/#67) are 2026-07-20 grab-bags
  that likely overlap work #61/#62/#97 already landed — decompose or close
  them, never implement as written.
- **Queue (re-triaged 2026-07-29; highs cleared 2026-07-30):**
  **CTO ruled ALL phase-2 issues gate exit**, so severity drives *ordering*,
  not scope. **Every phase-1/phase-2 high is now closed** — the four numbered
  highs (#45, #89, #48+#47, #49) plus #27 and #25. What remains is mediums,
  then lows. **#69 (Heir Kit) deferred by CTO** — it has a physical step
  (printing, tamper-evident envelopes) no agent can perform. Skip `epic`,
  `wontfix`, `needs-human`.
  **Next up (mediums, no hard order — pick by blast radius):** #87, #56, #55,
  #54, #53, #52, #44, #46, #93, #95, #51. Prefer ones outside the gate's path
  set when parallelising. (#94, #90, #96, #92, #87, #56, #54, #55, #52,
  #53, #93, #44, #46 closed 2026-07-30; #97/#98 filed as residuals.)
  **ALL phase-2 mediums are now closed.** Remaining queue is lows only:
  **THE PHASE-2 QUEUE IS EMPTY (2026-07-31).** Every phase-2 issue is
  closed except **#69 (Heir Kit)**, which the CTO deferred because it has a
  physical step — printing, tamper-evident envelopes — that no agent can
  perform. `EXPECTED_FAIL=()` is empty, so the mhvtl gate has zero slack.
  Closed 2026-07-30: #91. Closed 2026-07-31: #59, #50, #51, #98, #62, #43,
  #61, #97, #99, #95, #63, #83.
  **Before starting new work, re-triage.** The remaining backlog is phase-3
  (#70 scheduled advisory ops, #72 WarehouseStore, #73 warehouse locations)
  plus the LOW umbrellas (#65/#66/#67) and the epic (#20) / map (#1). Those
  were written pre-v2 and against older code — the single most valuable
  habit from this session was **auditing each issue against shipped code
  before implementing it**; roughly half of phase-2 turned out materially
  stale. Do that first, and expect phase-3 to need re-specing more, not
  less, since ADR-0006's store seam landed after those issues were filed.
  **`restore raw-volume` exists (#63)** — `src/volume/raw.rs`, DB-less by
  signature (`restore_raw(device, block_size, dest, expect_label)`),
  read-only (`TapeStore::open_read`), streaming. It dumps by position from
  the front index, naming files `{position:04}_{type_label}.bin` — the
  index carries no unit/tenant names by invariant, so friendlier naming is
  impossible without reintroducing the DB dependency the command exists to
  avoid. Verified on a real sealed tape: 23 files, 21 verified, 0
  mismatched, from a home whose DB had never seen that volume.
  **#83 changes what is written into the operator envelope — i.e. bytes on
  tape.** It is the one remaining item that touches the tape format, so it
  needs the envelope traps from #87 (hand-rolled `Header::new_gnu()`, never
  `append_file`/`append_path_with_name`, whose pax/ustar records pass every
  Rust test and break only the bash-`tar` heir legs) and a full mhvtl gate.
  **Session-dir GC is live (#95).** `staging clean` reclaims
  `{staging.directory}/sessions/*` matched to `writes.session_dir` by
  **exact equality** (never prefix/label — two attempts on one volume
  differ only by uuid). RETAIN while any row is `planned`/`in_progress`/
  `interrupted` (the resumable set); RECLAIM when all are `completed`/
  `failed`/`aborted`. §3.5 literally says "terminal-success", which would
  retain failed dirs forever; its own parenthetical names `interrupted` as
  the thing that must not be reaped, so the retention set is the resumable
  set. Reasoning is in the code comment — do not "correct" it back.
  **Orphan session dirs (zero referencing rows) are NOT deleted without
  `--force`** — a live `build()` that hasn't committed its `plan()` rows
  yet is indistinguishable from crash garbage. Leaking beats racing a
  writer. Same instinct as #98's sweep.
  **Lockfiles are reclaimed only for terminal stage sets** (`staged`/
  `failed`/`cleaned`, never `staging`) — deleting one while a process
  holds the flock lets a second process lock a different inode at the same
  path with both believing they hold it.
  **ADR-0004 Tier-1 evidence display is now COMPLETE (#99)** at all three
  destructive ops the ADR names — `volume retire`, `unit mark-tape-only`,
  `compact-finish`. `remaining_coverage_evidence` takes `Option<i64>`
  (`None` = "all eligible coverage", a real SQL branch, never a sentinel).
  If a fourth destructive op ever appears, wire it here too.
  **`compact_finish` returns a per-unit report** and BOTH its CLI call
  sites print it (`CompactFinish` and the combined `Compact`) — wiring one
  and not the other is the easy miss.
  **External-capability checks must FAIL OPEN (#97).** `dar -V` capability
  parsing (`dar::version::parse_capabilities`) starts with everything
  supported and only REMOVES on a positively-parsed `NO`; a missing block,
  absent line, odd verdict, garbage, or an unrunnable dar all leave the
  algorithm accepted. Hard-rejecting on unrecognised external output turns
  a helpful check into a tool that refuses to run on someone else's distro.
  Same rule for any future probe of an external binary.
  **This box reports YES for every dar compression algorithm**, so
  rejection cannot be tested against the real dar — use synthetic `-V`
  text for the parser, and a stub `dar` script for the end-to-end wiring.
  Parser tests alone would pass even if the caller propagated the error
  instead of failing open, which is the defect that actually matters.
  `lzma` rides the `xz compression (liblzma)` line; all eight allowlist
  entries are verified-valid dar names — do not "clean up" that list.
  **`meta.schema_version` is a STALE RELIC — never trust it (#61).**
  Migration 001 writes it as `'1'` and nothing bumps it;
  `rusqlite_migration` keeps the real applied level in `PRAGMA
  user_version` (currently 6). Any code needing the schema version reads
  `user_version`. The stale row is inert only because nothing else reads
  it — do not wire anything to it.
  **`db export` streams a full dump** (`src/db/export.rs`): tables
  enumerated from `sqlite_master` at runtime (never hardcode — that drift
  is what made the old version wrong), rows written straight to a
  `BufWriter<StdoutLock>` so peak RAM is one row, and FTS5 virtual tables
  plus their `<vtab>_*` shadows excluded as derived binary state. If you
  add a virtual table, the exclusion handles it automatically.
  **`dar` is a documented HARD test dependency (#43).** The ungated suite
  is NOT hermetic — 13 tests shell out to a real dar.
  `tests/test_dependencies.rs` asserts it once by name and **fails** rather
  than skipping: those tests are the staging-pipeline regression guards, so
  a silent skip yields a green suite that verified none of the pipeline.
  If you add a dar-dependent test, add it to that file's list. Fixtures
  resolve `"dar"` via PATH — never hardcode `/usr/bin/dar`.
  **"Honest skips" means VISIBLE, not merely present.** #43's tempting fix
  was to skip the 13; that would have converted 12 loud failures into
  invisible non-events one iteration after a months-long signal loss was
  found. When a test can't run, prefer a named failure over a quiet pass.
  **Staging is now flock-guarded (#98).** `stage_create` holds an exclusive
  flock on `<db_parent>/locks/stage-<id>.lock` for its lifetime; the
  `db::open` sweep probes that lock to tell a crash from a live run.
  Three rules any future change here must keep:
  (a) **the sweep MARKS ONLY, never touches files** — it runs on every
  `db::open` including read-only commands, so deleting there means a read
  can destroy staging data; deletion belongs to `clean_staging`;
  (b) **order is INSERT → flock → COMMIT** — under WAL the row is invisible
  until commit, which is what stops a concurrent sweeper seeing a live row
  with a free lock;
  (c) **the `writes` sweep stays NOT lock-aware** — `layout-session.md`
  (~line 157) makes "still `in_progress` at open ⇒ live writer" load-bearing
  for `rehydrate`, and `interrupted` degrades safely anyway.
  Use `nix::fcntl::Flock` (guard type); the free `flock` fn is deprecated
  since nix 0.28 and fails the clippy gate.
  **`tests/cli_smoke.rs` now costs ~37s** (was ~0.2s): #98's two tests need
  dar to run long enough to be caught mid-flight, on a deliberately
  incompressible fixture. Accepted — shrinking it would make a safety test
  racy. Budget for it when judging suite wall-clock.
  **Two CTO decisions ratified 2026-07-31 and recorded in
  `docs/design-errata.md`** — read the rows there before touching either:
  `preserve_acls` KEEPS its config field and column (dar has no
  independent ACL switch; ACLs ride EAs), with the no-op surfaced by
  `config check` via `policy::subsumed`; and restore uses **detect-and-fail**
  on overwrite collisions.
  **The restore-collision detector is counter-intuitive — do not
  "simplify" it.** dar under `-Q` silently skips a colliding file and
  EXITS 0. The skip is tallied under `ignored (excluded by filters)`,
  while `not restored (overwriting policy decision)` — the counter whose
  name fits — reads **0**. The only per-file signal is the stdout line
  `<path> not restored (user choice)` (stdout, not stderr). A test pins
  the real captured output and asserts a skip is found despite that zero,
  precisely to stop a future refactor from reading the summary block.
  **`#62` inherits two decorative keys from #59** (`backends.lto[].block_size`,
  `packing.min_free_for_append`) — parsed by serde, read by nothing.
  **A real-dar test pattern now exists** (`src/dar/restore.rs`'s
  `real_dar_collision_is_reported_as_a_failed_restore`): it shells out to
  `dar` and skips when absent, keeping the ungated suite free of
  external-binary requirements. Use that shape if wiring needs real-dar
  coverage — and note the mhvtl gate does NOT cover restore collisions,
  since its restore legs use fresh destinations.
  **`parse_size_to_bytes` now returns `Result` (#59) and `policy::resolve`
  with it.** Sizes are validated at `Config::load` (`validate_sizes`), so a
  bad `defaults.*` or backend value fails loudly at the boundary. A bare
  number is VALID and means bytes — do not "tighten" this into a suffix
  allowlist. `audit` catches `resolve`'s Err and reports
  `policy_unresolvable` as a VIOLATION that names the skipped checks;
  keep that shape, because a unit whose policy won't resolve has none of
  its other checks run and would otherwise read as clean.
  **`backends.lto[].block_size` and `packing.min_free_for_append` are
  decorative** — parsed by serde, read by nothing. #59 deliberately left
  them unvalidated (validating a field that does nothing gives false
  assurance). They belong to **#62**'s decorative-key honesty work.
  **`slice_arg_for_dar` (`staging/mod.rs` ~695–770) stays as-is.** Its doc
  justifies handing dar the operator's raw string BECAUSE the parser was
  untrustworthy. That premise is now gone, so "simplify it to use the
  parsed byte count" looks obvious and correct — it is not: it moves real
  on-tape slice boundaries for every unit. Leave it alone absent a ticket
  that decides that explicitly.
  **Lesson from #91 (2026-07-30): scope the issue against the ADR, not just
  against the issue's own Acceptance clause.** #91's Acceptance named only
  `volume retire`, but ADR-0004 names THREE destructive ops that must show
  the Tier-1 display (retire, `unit mark-tape-only`, `compact-finish`). The
  worker satisfied the issue exactly and still left two-thirds of the ADR
  unimplemented — and did not flag it, because nothing in its prompt asked
  it to read past the issue. #99 tracks the remainder. When an issue cites
  an ADR, read the ADR and diff its claims against the issue's scope before
  writing the worker prompt.
  **Second #91 lesson — a formatter is only as honest as its narrowest
  render context.** `describe` named the WEAKEST remaining copy using
  ADR-0004's single-copy wording ("coverage ... rests on L6-0009, never
  verified"), which asserts sole dependence. In `print_retire_impact` and
  the `--json` array the copy count sits alongside, so it reads correctly;
  but `cli::consent::confirm` prints each fact as a STANDALONE line, and
  ADR-0008 calls that prompt the one moment the operator is guaranteed to
  read. The misleading render landed exactly where it hurt most. Any string
  destined for `confirm`'s `facts` must be true read alone, with zero
  surrounding context — check every new fact line against that bar.
  **`src/policy/evidence.rs` is now the single place for evidence-age
  derivation** (`remaining_coverage_evidence` + pure `describe`). It
  deliberately does NOT match `audit.rs`'s `verify_age` query: per-volume
  rows not a unit-level MAX, `outcome='passed'` in a LEFT JOIN's ON clause
  (a WHERE turns it back into an inner join and never-verified volumes
  vanish), `coverage::eligible` on the volumes alias, and exclusion of the
  volume being consumed. Copy `audit.rs`'s query into a new evidence call
  site and the line will cite the very volume being retired as its own
  remaining coverage.
  **The gate now has leg 5: interrupt+resume (#93), 3 arms + verify/restore,
  and it RUNS LAST because it erases the tape legs 1-4 wrote.** Gate is now
  ~1m50s. Two facts any future gate work needs: **bash sets SIGINT to IGNORED
  for background jobs** in a non-interactive shell, so a signal sent before
  tapectl's `ctrlc::set_handler` runs is silently dropped; and `volume write`
  spends its first seconds in build/validate (full-hashing every staged
  slice) with the front zone + envelopes written before any slice — so
  "sleep N then signal" is unreliable at both ends. Wait on a DB precondition
  instead; the observed wait for one arm varied 4s..30s across runs.
  **There is now a process-level test layer (`tests/cli_smoke.rs`, #44).** It
  spawns the compiled binary via `env!("CARGO_BIN_EXE_tapectl")` — no
  `assert_cmd` dep. The config root resolves purely from
  `std::env::var("HOME")` (`src/config.rs:58`), so a `TempDir` as `HOME` fully
  satisfies the never-touch-real-`~/.tapectl` guardrail — but assert
  `<tmp>/.tapectl/tapectl.db` exists, because an exit-code-only assertion
  passes just as happily if the redirect silently failed. **Every `--json`
  assertion must parse the WHOLE stdout**, which is the only live guard
  against the #56 trailer defect; extend this file rather than inventing a
  second CLI harness. **All DB test fixtures now run the real ordered
  migration runner via `db::open_memory()`** — never hand-apply an
  `include_str!` migration again, especially not a non-contiguous subset
  (001+005 and 001+002+003 were the two real defects #44 removed).
  **Staging file naming is now `{uuid12}_v{version}_s{stage_set_id}.`** (#53)
  and lives in ONE place: `archive_base_name` / `archive_base_prefix` in
  `staging/mod.rs`. `stage_create` and `cleanup_failed_stage_set` both call
  them — if you change one, the other follows automatically. #54's
  end-to-end test is the lockstep guard (verified: reverting cleanup to the
  old per-snapshot shape fails it). `catalog_base` stays PER-SNAPSHOT on
  purpose — the `existing_catalogs == 0` guard extracts once and later stage
  sets reuse it; "making the naming consistent" is the wrong move.
  **Re-staging vs read-slices:** a re-stage yields DIFFERENT checksums (dar
  timestamps + randomized age) and that is correct. §2.21 read-slices is the
  identical-bytes path (tape→staging); re-stage is the fresh-bytes path
  (source→staging). Never treat them as substitutes.
  **`snapshot_create` now takes `config: &Config`** (replaced its
  `global_excludes` param, #52) and enforces design lines 184/185/203:
  nesting is an ERROR, empty units warn (gated on `file_count`, never
  `total_size`), large files warn. Warnings go through `tracing::warn!` —
  a `println!` on this path corrupts `--json` (the #56 defect).
  **Nesting predicates exclude by unit id, never by path.** A unit's own row
  makes `check_path.starts_with(existing)` trivially true, so a naive
  `check_nesting` call from `snapshot_create` fails EVERY snapshot. Two units
  can legitimately share a `current_path` after a bad `unit discover`, so
  path-comparison exclusion would hide a real conflict.
  `check_nesting{,_conflict}` now delegate to their `_excluding` forms.
  **`stage_slices.staging_path` rows are the ONLY handle on staged `.age`
  files** — `clean_staging` finds them exclusively by joining that table.
  Any code path that deletes those rows must unlink the files first, or the
  ciphertext is stranded forever, invisible to every cleanup path. #54 and
  #55 were the same defect at two entry points; assume more exist and check
  before deleting `stage_slices` anywhere. Order: collect paths, commit the
  DB change, THEN unlink — unlinking first strands a live snapshot pointing
  at nothing, which is worse than an orphaned file.
  **Verify the negative control mutated what you think it did.** Chasing
  #55, two attempts silently hit the wrong code (`snapshot_purge`'s
  transaction instead of `snapshot_delete`'s; then leaving `let tx = ...`
  alive so `conn.execute` still joined the open transaction). Both times the
  test "passed" and looked like weak coverage — the control was wrong, not
  the test. Print/grep the mutated region before believing a passing NC.
  **Staging-file prefixes must be dot-terminated (from #54).** dar names
  slices `{base}.{N}.dar` and `archive_base` is `{uuid12}_v{version}`, so a
  bare-base prefix makes `_v1` match `_v10.1.dar` and one stage's cleanup
  eats another's live plaintext. Matching rules, both load-bearing: plaintext
  `.dar`/`.sha512` by dot-terminated filesystem prefix (dar writes all slices
  up front, so most orphans have NO db rows); `.age` strictly by
  `stage_slices.stage_set_id` (`archive_base` is per-*snapshot*, so a prefix
  scan crosses stage sets).
  **Never wire cleanup to `stage_sets.status='failed'`** — `db/mod.rs`'s
  startup sweep marks every `'staging'` row failed and cannot see that
  another process is mid-stage, so cleanup keyed on that status deletes live
  files. Inert only because nothing targets `'failed'` today. Tracked in #98.
  **#53 has a hidden prerequisite:** `archive_base` is per-snapshot, so
  allowing a second stage set per snapshot makes two stage sets write
  identically-named `.age` files. Make `archive_base` per-stage-set as part
  of #53, or it silently corrupts.
  **Long-running work must not sit in one transaction.** `unchecked_transaction`
  is DEFERRED — it takes SQLite's single write lock at the first write. #54
  transactions only `stage_create`'s finalization block, not the dar run:
  a whole-function transaction would hold the lock for hours AND roll back
  the `stage_sets` row a crash needs to leave behind as its signal.
  **`audit` now implements all six §2.20 checks** (#56). Its dirty check
  reuses `report.rs::dirty_rows` (now `pub(crate)`) — one scan, one place.
  Note `audit` with no `--unit` now walks the filesystem per active unit;
  `tests/performance.rs::perf_many_units_audit` replicates the *queries*, not
  `run`, so it will not track that cost.
  **Watch refactors that move a `println!` across a `json_output` branch.**
  #56's `collect_findings` extraction hoisted the summary line out of the
  non-JSON arm, so `audit --json` emitted JSON plus a human trailer. Every
  test asserted on findings or exit codes, so nothing caught it. The fix
  shape is reusable: make rendering a pure `render() -> String` and assert
  the WHOLE output parses. Any command with a `--json` mode is exposed to
  this; there is no test that pins the others.
  **H9 (whole-object buffering) is fully closed with #87.** Envelopes now
  stream `File -> util::HashingWriter -> age StreamWriter -> tar::Builder`;
  the HashingWriter must stay on the *ciphertext* side or the front index
  records a plaintext hash. Two envelope traps are now pinned by tests, but
  keep them in mind for any future envelope work: (a) `OperatorEnvelopeBackup`
  is an `fs::copy` of the primary, never a second encrypt — age is randomized
  per call and the tar layer stamps `set_mtime(now)`, so re-encrypting yields
  an unrelated ciphertext that defeats the redundant copy; (b) envelope tars
  keep the hand-rolled `Header::new_gnu()` shape — never `append_file` /
  `append_path_with_name`, whose pax/ustar extension records pass every Rust
  test and break only the bash-`tar` heir legs.
  **`#[cfg(test)]` is not a usable negative control in this crate.** It does
  not propagate to integration-test binaries, which link the library
  normally; gating a `pub fn` they use breaks the build. #87 tried this and
  had to back it out.
  **Status-predicate discipline (from #96):** every `volumes.status` read
  filter now routes through `policy::coverage` — `eligible` ("is a finished
  copy", `sealed`), `in_service` ("holds bytes we account for", + `active`/
  legacy `full`), `in_service_or_provisioned` (+ `initialized`). Never inline
  a status list again; five inlined copies are how #96 happened.
  **Dotfile policy contract (from #92, landed 2026-07-30).** Dotfile
  `[policy]` fields are `Option`; `write_dotfile` omits the whole `[policy]`
  table when unset; absent = defer upward. `policy::resolve` was NOT changed —
  its layer 1 reads raw TOML, so absent keys always fell through correctly;
  the bug was purely in the writer. Never reintroduce a serde `default` on a
  policy field: a filled default is indistinguishable from an operator choice
  and silently outranks the archive set. `config check` flags pre-existing
  shadowing dotfiles via `policy::shadowing::scan` — it advises, never
  rewrites operator-owned files, never changes the exit code.
  **Lesson from #92 (worth generalising): a field nothing could reach hides
  the bugs behind it.** Making archive-set `compression` reachable
  immediately exposed that `dar/create.rs` had *never* worked for any
  non-`none` value — it passed `-z` and the algorithm as two argv tokens, but
  dar's `-z` takes an *optional* argument, so getopt only binds a glued
  `-zgzip`. Both defects were invisible for the same reason. When unblocking a
  dead config path, budget for the code downstream of it being untested too.
  Also: the worker weakened the acceptance test (archive_set `"none"`) to
  route around that dar defect — but `"none"` was exactly the old hardcoded
  dotfile value, so it passed for the wrong reason. **Check that a
  regression test still fails against the pre-fix code**, not just that it
  passes after.
  **Cross-issue sequencing (recorded on the issues too):** #50/#51 must also
  patch the generated RESTORE.sh in `layout.rs` (`-O` appears there too) or
  the heir path keeps the fixed-away behavior; #50's remedies are impossible
  as written (`dar --acl` does not exist, `--hash sha256` is invalid) —
  remove, don't implement. (#45-before-#44 is discharged: #45 is closed.)
  *Landed and closed:* #27, #35/#84/#85/#86 (H9 streaming class), #32,
  #34 (slice numbering), #33 (symlinks), #36 (dirty detection), #45, #89,
  #48, #47, #49, #38, and #25 (CLI resume — rehydrate-don't-regenerate;
  see the new cross-process bullet in `layout-session.md`).
  **Lesson from #25 (2026-07-30):** an issue can be *implemented but never
  closed* — #27 was fully landed on master while its issue sat open, and the
  Policy block said "closed" while `gh` said otherwise. Survey by grepping
  the code for the symbol, not by trusting either. `EXPECTED_FAIL=()` is now
  empty, so the mhvtl gate has **zero slack**: any failure is a hard stop.
- **Issues were re-triaged 2026-07-29 against shipped code** — verdicts and
  rescopes are in each issue's comments and supersede the original text.
  Four were partly/wholly stale, four over-severed, three escalated, one
  (#64) a RESPEC whose prescribed fix would make the docs *less* accurate.
  Read the comments before implementing. Where an issue still contradicts the
  shipped design, the design wins and the issue is wrong.
- **Normative set (authority order):** `docs/design/volume-format-v2.md`
  (on-tape bytes) → `docs/design/layout-session.md` (session state machine) →
  `docs/design/v2-open-questions.md` (resolved decisions) →
  `docs/design-errata.md` (superseded v4.0 sections). ADRs
  (`docs/adr/0001`–`0007`) govern all of them. `CONTEXT.md` is the vocabulary —
  note **Collection** (source roots) vs **Tape Library** (the changer): never
  write a bare "library".
- **The three sacred invariants** (playbook preamble — a violation is
  stop-the-line): the seal marker is written only inside the session
  lifecycle; `Layout::validate` full-hashes staged slices; no plaintext file
  carries tenant/unit names, filenames, `sha256_plain`, or key fingerprints.
- **Integration authority: PM review + cherry-pick onto master, then push.**
  Chosen deliberately over merge-on-green: during the v2 regear, CI-green code
  still carried a hollow-map gap, a ~1-in-900 seal-timestamp flake, and a
  resume path that would rewrite a SEALED tape — all three passed
  fmt/clippy/test and were caught only by reading a flagged residual and
  deciding it was unacceptable. So: no PRs, no merge-on-green. Push master
  after each land so CI runs as an independent man-page/build check (it cannot
  run the mhvtl gate — that stays with you). Gate after EVERY integration:
  `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings &&
  cargo test`. Never pipe clippy through `tail` (warnings print above
  "Finished").
- **Verify, don't trust.** Re-run the gate yourself; read the diff for scope;
  check wall-clock, not just pass counts. Sub-agent reports are leads, not
  evidence. Treat a flagged residual as a decision you must make, not an FYI
  to file.
- **After pushing, actually READ the CI result** (`gh run list` / `gh run
  watch --exit-status`). On 2026-07-31 CI was found red going back past #56
  — 12 tests, every commit — because the ungated suite shells out to `dar`
  and the workflow never installed it, while the workflow's own comment
  claimed "the ungated suite is hermetic: no external binaries". Fixed in
  `8485f73`. Five pushes in one session treated CI as an independent check
  while it was providing no signal at all. A job that is red on every
  commit is not a check; confirm green, don't assume it.
- **Issue key/file lists go stale — re-audit before implementing.** #62
  named 8 "parsed-but-ignored" config keys; 5 were wrong (already wired by
  #52/#59, or never existed). Verify each claim against shipped code and
  report the corrected table, exactly as with #50/#51's dar flags.
- **"Wire-or-delete" on operator-facing surface now resolves to SURFACE**
  (CTO precedent, #50/#92, in `docs/design-errata.md`): keep the knob and
  report the no-op via `config check`. `policy::subsumed`, `policy::
  decorative` and `policy::depth_check` are the established shape — a
  `scan()` plus a pure `describe()`, advisory, never touching the exit code.
  Never delete a key whose consumer is merely *deferred* (`block_size` is
  owned by #20 / the open 512K-vs-1M hardware question).
- **Restore-path gate:** any diff touching `src/volume/`, `src/tape/`,
  `src/staging/`, `src/crypto/`, or generated RESTORE.sh/RECOVERY.md content
  MUST pass `TAPECTL_MHVTL=1 ./scripts/mhvtl-verify-gate.sh` before the branch
  is offered to the CTO. Tests/docs-only diffs skip it. If a fix resolves a
  manifest entry, shrink `EXPECTED_FAIL` in the same commit — the gate fails
  on unexpected passes. **Note:** the e2e suite is expected RED between
  playbook T8 and T9 — that is the one sanctioned red window; land them
  consecutively and do not run the gate in between.
- **Single-drive rule:** the gate takes `/tmp/tapectl-tape.lock` (flock);
  never run two tape-touching processes. Sub-agents never touch `/dev/nst*` —
  the coordinator runs the gate (see `worktree-agent.md`).
- **Docs cite commands; commands drift.** Nothing gates a wrong flag in prose,
  so hand-verify every `tapectl …` line you write against real `--help` output
  — `--help` short-circuits clap, so `<cmd> --bogus --help` still exits 0 and
  proves nothing about the flags. Writing #46's runbook this way caught three
  errors in the first draft. Write docs from `--help` and shipped code, never
  from the ADRs: ADRs describe intent, and open issues (#91, #69, #70) are
  exactly where intent is not yet reality. `docs/operator-guide.md` now names
  those gaps explicitly, so closing one of those issues means editing the guide
  in the same commit.
- **Man pages:** any clap change regenerates `docs/man` in the same commit
  (`cargo run --example gen_man`). **Regenerate again AFTER a rebase, before
  integrating** — a branch's generated artifacts are stale by definition once
  it moves onto a newer base. #147 changed the GLOBAL `--yes` help, so all 123
  pages moved; but it had regenerated before being rebased onto a master that
  had since gained `cartridge edit` (#167) and `cartridge unretire` (#163), and
  those two pages kept the old wording. The local gate does not check man-page
  drift — CI's "Man pages in sync" job is the only thing that does, which is
  why the Policy says to READ the CI result rather than assume it.
- **Model tactics:** you keep judgment (task selection, review, integration,
  anything crypto/tape-semantics/state-machine). Sonnet workers for spec'd
  legwork via the `worktree-agent` template; haiku for fully-specified
  mechanical work. **Create each worktree yourself** from the feature branch —
  never `isolation: "worktree"` (it forks from stale master; see
  `worktree-agent.md`).

## Iteration — one task, run to done

1. **Survey.** Feature branch, clean tree. A dirty tree is a STOP: say what is
   dirty and commit it or `git checkout -- .` it deliberately — **never `git
   stash`** (`refs/stash` is shared across every worktree; banned 2026-09-11).
   Confirm the branch gate is green BEFORE dispatching — never build on a red
   base. Pick the next queue item whose stated dependencies have landed.
2. **Viability gate.** Read the task entry fully plus the design sections it
   cites. Confirm it is decidable without the CTO. A design fork with one
   clearly-defensible option is viable — take it and record the reasoning in
   the commit. A genuine judgment call, a normative-doc conflict, or anything
   that changes bytes-on-tape or a ratified decision is NOT — queue it (step 5).
3. **Dispatch.** Create the worktree, verify its base, build the sub-agent
   prompt from `worktree-agent.md` (mandatory reading, scope fence, traps,
   stop-and-ask). Correctness-critical tasks (byte formats, state machines,
   integrity checks) require the test FIRST, confirmed failing.
4. **Review and integrate.** Verify-don't-trust (Policy). Work every flagged
   residual to a decision: fix it, or record why it ships as-is. Cherry-pick,
   re-run the gate, then `git worktree remove`. Record what landed and what
   surprised you in the project memory checkpoint.
5. **Escalate instead** when blocked: write the question down with the
   options and your recommendation, and **keep working** — take the next
   viable task. Do not stall the loop on a pending decision.

## Talking to the CTO — batch, don't interrupt

Accumulate decisions in a queue. Surface them with `AskUserQuestion` when
either: **two or more** are pending, or **every** remaining task is blocked on
one. Present each as: the question, 2–4 concrete options, your recommendation
first with its reasoning, and what it costs to defer. One question per genuine
decision — never ask about mechanics you can settle from the docs, and never
ask the same thing twice (ratified decisions live in
`v2-open-questions.md` §§1, 7).

If a decision arrives, fold it into the design docs FIRST (so it cannot be
re-litigated), then implement.

## Stopping the loop

Stop (end the /loop, not just the iteration) when: the current queue (the
Policy block names it — since 2026-09-14 the GitHub label `review-2026-09-13`)
is empty with nothing parked AND the gate's EXPECTED_FAIL manifest is empty
AND the post-queue re-review the Policy block requires has been run and its
findings worked; or every remaining task is blocked on a CTO decision and the
batch has been surfaced; or two consecutive iterations ended in escalation
with nothing landed. **Stop before any first production write** — that, the
real-drive rehearsal on home2, and #69's physical Heir Kit step are CTO
calls, not autopilot's.

Before stopping: post a summary (landed with SHAs, decisions pending with
their options, residuals accepted and why), update the memory checkpoint, send
a push notification.

## Hard guardrails

Never weaken a gate to make progress — not the EXPECTED_FAIL manifest, not a
test, not clippy. A green achieved by lowering the bar is a regression. Never
edit `tapectl-design-v4_0.md`, `docs/adr/`, `CONTEXT.md`, or the normative
design notes to make an implementation fit — if the code cannot satisfy the
spec, the spec wins and the mismatch is a CTO escalation (the one exception:
the spec is *demonstrably* wrong, in which case fix the doc FIRST, in its own
commit, with the evidence in the message). Never run the tapectl binary
against the real `~/.tapectl` — temp homes only. (R&D mode has exited: pushing
master after a verified land, and filing issues for real findings, are now
expected — see the Integration authority and Mode lines above. This sentence
previously forbade both and contradicted them.)
