# worktree-agent — sub-agent scaffold for tapectl code changes

Prompt template, not a runnable skill (adapted from homorg's worktree-agent).
When delegating code changes to a sub-agent, build its prompt from the
template below. The coordinator owns worktree creation and integration —
agents never merge, never push.

## Spawning: create the worktree yourself

**Do NOT use the Agent tool's `isolation: "worktree"` for tapectl work.** It
forks from the repo's default branch as the harness sees it, which is
`origin/master` — *not* your feature branch. That bit four consecutive agents
in the 2026-07 v2 regear: each silently started from a base missing every
commit the task depended on. Instead:

```bash
git worktree add .claude/worktrees/pm-<task> -b pm-<task> <target-branch>
# then verify the base carries what the task needs:
test -f <a file the task depends on> && grep -q "<a symbol the task depends on>" <file>
```

Pass the worktree path to the agent explicitly and require every command to
`cd` there. Clean up with `git worktree remove` after integrating.

## Template

```
**YOUR ASSIGNED WORKTREE: `<abs path>` (branch `<branch>`).** ALL work happens
there: begin EVERY shell command with `cd <abs path> && `, and read/edit files
only under that path. Commit to that branch. Never push, never merge, never
run `gh pr create`, never touch the main checkout or master — the coordinator
owns integration.

**BASE SANITY CHECK — before anything else.** In YOUR worktree, verify:
(a) `<file the task depends on>` exists, and
(b) `<file>` contains `<symbol the task depends on>`.
If either fails, STOP immediately and report "STALE WORKTREE BASE" plus what
is missing. Do no work in that case — do not rebase, merge, or "fix" the base
yourself; base repair is the coordinator's call.

**Build rule.** Debug/check only — NEVER `--release`. Every cargo invocation
sets CARGO_TARGET_DIR=/scratch/tapectl-target (the / partition is small).
When agents run in parallel, use /scratch/tapectl-target-<branch> instead, so
each worker keeps its own incremental cache.

**Build lock — mandatory whenever workers run in parallel.** Wrap EVERY cargo
invocation in `flock /scratch/tapectl-build.lock`:

```bash
CARGO_TARGET_DIR=/scratch/tapectl-target-<branch> \
  flock /scratch/tapectl-build.lock cargo test
```

This VM has 9 GB of RAM and 16 cores, so the bottleneck is memory during
linking, not CPU: two concurrent cargo builds OOM-kill each other and
whatever else is running (recorded undated in the project memory checkpoint,
from the v2 regear; not to be confused with 2026-07-30, which was the
/scratch disk-fill incident described above). A
per-worker target dir alone does not prevent that — it makes it *more* likely,
because nothing serializes the builds. A *shared* target dir would serialize
them via cargo's own lock but forces a cold rebuild on every switch between
worktrees, since cargo fingerprints on the workspace path. The flock gives you
both: incremental caches per worker, one build at a time across all of them.
Expect to wait on the lock; that wait is the feature. Reading, editing and
thinking all proceed in parallel — only linking is serialized.

**tapectl guardrails.**
1. Never touch /dev/nst* or /dev/sg*, and never run TAPECTL_MHVTL=1 suites —
   the tape drive is a single shared resource; the coordinator runs
   scripts/mhvtl-verify-gate.sh.
2. Never run the tapectl binary against the real ~/.tapectl — temp home only
   (point --config into a tempdir).
3. Never modify tapectl-design-v4_0.md, docs/adr/, CONTEXT.md, the design
   notes under docs/design/, or the gate's EXPECTED_FAIL manifest — those are
   coordinator/human-gated. The normative reference set is: CLAUDE.md,
   docs/design-errata.md (never implement against a superseded section),
   docs/design/volume-format-v2.md (on-tape bytes),
   docs/design/layout-session.md (session state machine),
   docs/design/v2-open-questions.md (resolved decisions), and
   docs/design/v2-implementation-plan.md (the task playbook).
4. Do not create files under docs/audits/. Do not file GitHub issues.
   **Never put a GitHub closing keyword in a commit message** — no
   `Fixes #N`, `Closes #N`, `Resolves #N`. Cite the issue as `(issue #N)` or
   `(#N)` instead. The coordinator pushes, so a closing trailer auto-closes
   the issue the instant it lands — before review, and regardless of whether
   the work was any good. On 2026-07-30 that closed #40 and #41 out from
   under the verification step; the work happened to hold up, which is luck,
   not process. Closing is the coordinator's decision, made after the gate.
5. No new dependencies without coordinator approval.
6. **Never `git stash` / `git stash pop` — in any form.** `refs/stash` is
   shared across every worktree of this repository, so a `pop` can hand you
   ANOTHER worker's uncommitted changes and lose your own (2026-09-11: the C4
   worker popped the C7 worker's entry; it noticed, nothing was lost, luck
   not process). To take a baseline measurement before editing, run it FIRST
   and do not touch files until it finishes; if you must set edits aside,
   `git diff > /tmp/<branch>.patch && git checkout -- .` and `git apply` it
   back, or make a temporary WIP commit and `git reset --soft HEAD~1` later.

**SCOPE FENCE.** You own exactly these files: {{LIST}}. Other agents are
concurrently editing {{LIST}} — do NOT edit those, even trivially. You may
read anything. If the task seems to require editing a fenced file, STOP and
report rather than reaching across.

**STOP-AND-ASK.** If the design docs genuinely conflict, or a step is
ambiguous in a way that changes bytes-on-tape, schema, or a public trait
shape: STOP and report the question. Do not invent, do not "pick the
reasonable one" silently. An honest stop beats an inventive workaround —
wrong bytes on tape are forever.

## Task: {{ONE-LINE TASK DESCRIPTION}}

{{2–3 SENTENCES OF CONTEXT — which playbook task (T-number), why, how it fits.
State the baseline test count.}}

### Mandatory reading, in order

{{The playbook preamble (Global rules + the three sacred invariants) + this
task's entry, then the specific design-doc sections that govern it. Name the
sections — an agent that reads the whole doc set burns its context before it
starts.}}

### The N changes (apply in order)

{{Per change: name + file path + what + acceptance criterion. Where the task
is correctness-critical (a byte format, a state machine, an integrity check),
require the test FIRST and require confirming it fails before the fix.}}

### Traps (do NOT do these)

{{The specific wrong turns for this task, stated as prohibitions. This section
is the highest-value part of the prompt for a lesser-reasoning model — it
converts "use judgment" into "don't do X".}}

### Process

1. Base sanity check (above). Then `git branch --show-current` — confirm it is
   your assigned branch, not master.
2. **Re-confirm the defect before writing anything.** Your task cites specific
   files, lines and symbols. Those citations were drafted against an earlier
   tree and were NOT independently verified — the verification pass that was
   meant to check them never ran. So grep each one yourself and confirm the
   defect is still there, in the form described. Three outcomes:
   - it is there as described → proceed;
   - it is there but the line numbers or details have drifted → note the
     correction in your final report and proceed;
   - **it is not there at all, or the code already does the right thing** →
     STOP and report "DEFECT NOT PRESENT" with what you actually found. Do not
     invent work to justify the task, and do not "fix" code that is already
     correct. A task built on a stale citation is the coordinator's problem to
     re-scope, not yours to paper over.
   Where the task says a step is "ratified", that means an ADR paragraph
   settles it and you may not re-open it. Where a step merely *applies* a
   ruling, it is ordinary engineering and your judgement is welcome — issues
   #166 and #178 carry comments drawing that line explicitly.
3. **Run every cargo command synchronously, in the foreground.** Never
   background one, never arm a Monitor or wait on a "completion
   notification" — the coordinator sends none, and three workers on
   2026-09-11 each stalled for minutes waiting on one. A baseline `cargo
   test` is run first, waited on, and its totals recorded; no file is
   touched until it returns.
4. Apply changes in order; `cargo check --all-targets` after each.
5. Full gate: `cargo fmt --all -- --check && cargo clippy --all-targets --
   -D warnings && cargo test` — green AND test count >= baseline. Never pipe
   clippy through `tail`; warnings print ABOVE the "Finished" line and a tail
   hides them.
6. If you changed any clap definition: `cargo run --example gen_man` and
   commit docs/man.
7. Commit per change, conventional style (see `git log --oneline -5`), body
   citing the design section that mandates it.
8. Do NOT push. Do NOT merge.

### Final report (return verbatim — this is data for the coordinator, not
prose for a user)
- Branch name and worktree path
- Commit SHAs in order
- Test count vs baseline ({{N}}), and any test whose runtime is notable
- Gate green: yes/no — state the exact command you ran
- Deviations from spec, and why
- Anything you flagged but did NOT fix (residuals), stated plainly

If anything fails or is unclear, STOP and report — don't paper over it.
```

## Coordinator's integration order

1. **Verify, don't trust the report.** Re-run the full gate yourself after
   integrating. Agents have reported "all green" on a suite that was in fact
   pathologically slow, and have correctly reported warnings the coordinator's
   own (tail-truncated) gate had hidden. The report is a lead, not evidence.
2. **Read the diff for scope**, not just the summary. Check the agent touched
   only its fenced files: `git diff --stat <base>..<head>`. A large deletion
   count inside a big insertion is usually diff noise (moved lines) — confirm
   with a sorted comm of added-vs-deleted lines before treating it as a
   violation.
3. **Integrate by cherry-pick**, not merge, when the agent's worktree base is
   anything other than the current branch head: `git cherry-pick <sha>...`.
   Keeps the feature branch linear and side-steps a stale base dragging old
   commits along.
4. **Treat flagged residuals as decisions, not FYIs.** The agent stops at "I
   noticed X"; deciding whether X ships is the coordinator's job. Several of
   the v2 regear's real defects were flagged honestly and would have shipped
   if the flag had been filed instead of worked.
5. If the change touched restore-path files (src/volume, src/tape, src/staging,
   src/crypto, generated RESTORE.sh/RECOVERY.md), run
   `TAPECTL_MHVTL=1 ./scripts/mhvtl-verify-gate.sh` before the branch leaves
   your hands.
6. `git worktree remove .claude/worktrees/pm-<task>` once integrated — **and delete
   its `CARGO_TARGET_DIR` in the same breath**:
   `sudo rm -rf /scratch/tapectl-target-pm<task>`.
   Removing only the worktree leaks a ~7 GB target dir per agent. On
   2026-07-30 that filled `/scratch` to 100% (6 MB free) across 21 orphans,
   ~159 GB, and blocked an agent mid-task until it cleaned up after the
   coordinator. Cheap to prevent, disruptive to hit. Sanity check when idle:
   `ls -d /scratch/tapectl-target-* 2>/dev/null` should list only dirs whose
   worktree still exists under `.claude/worktrees/`.

## Model default

Workers on **sonnet**; the coordinator stays on the session model and owns
verify-before-integrate. **haiku** is fine for fully-specified mechanical work
(fixture generators, scaffolding) — but expect to review its performance
characteristics, not just its correctness. Bump to the session model only for
genuinely subtle logic: tape semantics, crypto, the Layout state machine.

## Pitfalls

- **Stale worktree base** — the #1 failure mode; see "Spawning" above. The
  base sanity check turns a silent wrong-base build into an 8-second stop.
- Agent silently writes to master — the worktree rule + process step 1 catch it.
- Two agents, one file — the scope fence; sequence rather than fence when the
  overlap is real (e.g. two tasks both editing the write path).
- Post-merge test-count drop with per-agent green — the after-each-integration
  gate catches cross-agent interactions.
- An agent "fixing" a red gate check that is in EXPECTED_FAIL — forbidden;
  manifest edits ride the fixing coordinator commit only.
- **A fast agent report hiding a slow suite** — always look at wall-clock, not
  just pass counts. A generator that hashes per 32 bytes passes every
  assertion and makes the suite unusable.
- **Coordinator gate drift** — if the coordinator's own gate command is weaker
  than the template's (piping clippy through `tail`, skipping `-D warnings`),
  agents will surface defects the coordinator cannot see. Run the same gate
  you require.
