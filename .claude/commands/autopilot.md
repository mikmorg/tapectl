# Autopilot — drive the tapectl build queue, escalating only real decisions

You are running tapectl's build queue on autopilot: land ONE task end-to-end
per iteration, or escalate it, then continue. Invoked as `/loop /autopilot`
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

- **Mode: phase-2 hardening (from 2026-07-28).** R&D has exited: the v2 regear
  is merged to master and the design docs are settled reference, not fluid.
  Issue work is live again — re-spec, close, and file as needed.
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
  **#63, #83, #95, #97, #99.** (#91 closed 2026-07-30; #99 filed as its
  scope residual. #59, #50, #51, #98, #62, #43, #61 closed 2026-07-31.)
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
  (`cargo run --example gen_man`).
- **Model tactics:** you keep judgment (task selection, review, integration,
  anything crypto/tape-semantics/state-machine). Sonnet workers for spec'd
  legwork via the `worktree-agent` template; haiku for fully-specified
  mechanical work. **Create each worktree yourself** from the feature branch —
  never `isolation: "worktree"` (it forks from stale master; see
  `worktree-agent.md`).

## Iteration — one task, run to done

1. **Survey.** Feature branch, clean tree (dirty → stash and say so). Confirm
   the branch gate is green BEFORE dispatching — never build on a red base.
   Pick the next playbook task whose DAG predecessors have landed.
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

Stop (end the /loop, not just the iteration) when: the phase-2 queue is empty
AND the gate's EXPECTED_FAIL manifest is empty; or every remaining task is blocked on a CTO decision and the
batch has been surfaced; or two consecutive iterations ended in escalation
with nothing landed. **Stop before any first production write** — that, the LTO-6 hardware session,
and #69's physical Heir Kit step are CTO calls, not autopilot's.

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
