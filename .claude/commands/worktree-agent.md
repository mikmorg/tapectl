# worktree-agent — sub-agent scaffold for tapectl code changes

Prompt template, not a runnable skill (adapted from homorg's worktree-agent).
When delegating code changes to a sub-agent, build its prompt from the
template below. Spawn with the Agent tool and `isolation: "worktree"`; the
coordinator merges — agents never do.

## Template

```
**WORKTREE RULE — read this FIRST.** All your work happens in the isolated
worktree the harness gave you, on whatever branch is checked out. Do NOT
switch to master, push, merge, or run `gh pr create` — the coordinator owns
integration. If `git branch --show-current` says `master`, STOP and report.

**Build rule.** Debug/check only — NEVER `--release`. Every cargo invocation
sets CARGO_TARGET_DIR=/scratch/tapectl-target (the / partition is small).
When agents run in parallel, use /scratch/tapectl-target-<branch> instead — a
shared target dir serializes builds on cargo's lock.

**tapectl guardrails.**
1. Never touch /dev/nst* or /dev/sg*, and never run TAPECTL_MHVTL=1 suites —
   the tape drive is a single shared resource; the coordinator runs
   scripts/mhvtl-verify-gate.sh after merge.
2. Never run the tapectl binary against the real ~/.tapectl — temp home only
   (point --config into a tempdir).
3. Never modify tapectl-design-v4_0.md, docs/adr/, CONTEXT.md, or the gate's
   EXPECTED_FAIL manifest — those changes are coordinator/human-gated. Check
   docs/design-errata.md before implementing anything from the design doc;
   for epic #20 children, docs/design/layout-session.md is normative.
4. Do not create files under docs/audits/.

## Task: {{ONE-LINE TASK DESCRIPTION}}

{{2–3 SENTENCES OF CONTEXT — the issue number, why, how this fits. State the
baseline test count.}}

### The N changes (apply in order)

{{Per change: name + file path + what + acceptance criterion. If the issue
names a red-today regression test, write it FIRST and confirm it fails.}}

### Process

1. `git branch --show-current` — confirm NOT master.
2. Apply changes in order; `cargo check` after each.
3. Full gate: `cargo fmt --all -- --check && cargo clippy --all-targets --
   -D warnings && cargo test` — green AND test count >= baseline.
4. If you changed any clap definition: `cargo run --example gen_man` and
   commit docs/man.
5. Commit per change, conventional style (see `git log --oneline -5`).
6. Do NOT push. Do NOT merge.

### Final report (return verbatim)
- Branch name and worktree path
- Commit SHAs in order
- Test count vs baseline ({{N}})
- LOC delta (`git diff --shortstat master...HEAD`)
- Gate green: yes/no
- Deviations from spec or surprises

If anything fails or is unclear, STOP and report — don't paper over it.
```

## Coordinator's merge order

1. Confirm master's baseline still holds (`cargo test`).
2. Merge agent branches sequentially; after EACH merge run the full gate.
3. If the merged change touched restore-path files (src/volume, src/tape,
   src/staging, src/crypto, generated RESTORE.sh/RECOVERY.md), run
   `TAPECTL_MHVTL=1 ./scripts/mhvtl-verify-gate.sh` before pushing.
4. Push after all merges. Direct-to-master today; if branch protection lands,
   switch to PR flow (autopilot already uses PRs).

## Model default

Spawn workers on sonnet; the coordinator stays on the session model and owns
verify-before-merge. Bump a single agent's model only for genuinely subtle
logic — tape semantics, crypto, the Layout state machine.

## Pitfalls (inherited from homorg + tapectl-specific)

- Agent silently writes to master — the worktree rule + step 1 catch it.
- Two agents, one file — scope agents to disjoint file sets or sequence them.
- Post-merge test-count drop with per-agent green — the after-each-merge gate
  catches cross-agent interactions.
- An agent "fixing" a red gate check that is in EXPECTED_FAIL — forbidden;
  manifest edits ride the fixing coordinator commit only.
