# Autopilot — land the tapectl backlog sequentially, without the user

You are running the tapectl renovation backlog on autopilot: land ONE issue
end-to-end per iteration, or escalate it, then continue. Invoked as
`/loop /autopilot` (self-paced) or `/loop <interval> /autopilot`. The loop —
not this iteration — decides when work stops; your job each firing is one
issue, run to *landed* or *escalated*, never to half-done.

Read before the first pick: `CLAUDE.md` (build rules, reference-doc set),
`docs/design-errata.md` (v4.0 sections that are superseded — never implement
against a superseded section), and for epic #20 children,
`docs/design/layout-session.md` (normative).

## Policy (edit this block as reality changes — nowhere else)

- **Queue:** open issues in mikmorg/tapectl, consumed `phase:1` → `phase:2` →
  `phase:3`, then LOW umbrellas (#65–#67); within a phase, `severity:high`
  first; within epic #20, child order (#21…#28, #71 — ordering noted in each
  body). Skip: `epic`-labeled issues, `wontfix`, `needs-human`, anything
  already assigned.
- **Merge authority:** merge on green. Branch per issue → PR (`Closes #N`) →
  CI green → merge → delete branch. No `--admin`, ever; a red check is fixed
  forward on the branch or the issue is escalated. The audit job is
  `continue-on-error` until #42 lands — its red does not block, but never
  introduce a NEW advisory.
- **Restore-path gate (the blind-restore analog):** any diff touching
  `src/volume/`, `src/tape/`, `src/staging/`, `src/crypto/`, or generated
  RESTORE.sh/RECOVERY.md content MUST pass
  `TAPECTL_MHVTL=1 ./scripts/mhvtl-verify-gate.sh` locally before merge.
  Tests/docs-only diffs skip it. If the fix resolves a manifest entry, shrink
  `EXPECTED_FAIL` in the same commit — the gate fails on unexpected passes.
- **Single-drive rule:** the gate takes `/tmp/tapectl-tape.lock` (flock);
  never run two tape-touching processes. Worktree sub-agents never touch
  `/dev/nst*` — the coordinator runs the gate (see worktree-agent.md).
- **Man pages:** any clap change regenerates `docs/man` in the same commit
  (`cargo run --example gen_man`); CI's man-drift job enforces it.
- **Non-viable (escalate on sight, never attempt):** needs real tape
  hardware (phase 4 is trigger-gated per #16); needs an ADR-level decision or
  contradicts an ADR/design-errata entry — escalate WITH a drafted ADR;
  needs credentials, accounts, or new spend; labeled `wontfix`/`needs-human`
  or already assigned.
- **Model tactics:** the main loop keeps judgment (issue selection, merge
  decisions, anything crypto/tape-semantics). Sonnet workers for tightly
  spec'd legwork via the worktree-agent template; workers that could touch
  the same files run in worktree isolation or are sequenced.

## Iteration — one issue, run to done

1. **Survey.** From a clean, synced master (`git fetch --all --prune`,
   ff-only pull; dirty tree → stash and say so in the summary). List open
   issues; pick per Policy.
2. **Viability gate.** Read the issue fully, including its audit link and any
   design-input comments. Confirm it is solvable on this box and decidable
   without the user. A design fork with one clearly-defensible option is
   viable — take it and record the reasoning in the PR. A genuine judgment
   call the user would want is not — escalate.
3. **Land it.** Branch → fix + the issue's named regression test (many
   phase-1/2 issues ship with a red-today test — write it first, watch it
   fail, then fix) → run the local gate set (`cargo fmt --check`, `clippy
   --all-targets -- -D warnings`, `cargo test`; + the mhvtl gate per Policy)
   → PR (`Closes #N`) → CI green → merge → confirm the issue closed →
   delete branch.
4. **Record.** Update the project memory checkpoint (what landed, what
   surprised you); file follow-up issues for anything real you uncovered but
   didn't fix, labeled per the #17 rubric.
5. **Escalate instead** when the gate fails or landing stalls: comment the
   diagnosis + exactly what decision/resource is needed, label
   `needs-human`, and pick the next viable issue within this same iteration
   (one substitution max — then end the iteration).

## Stopping the loop

Stop (end the /loop, not just the iteration) when no open issue passes the
viability gate, or two consecutive iterations ended in escalation with
nothing landed. Before stopping: post a summary (landed with PR links,
escalated with the decision each needs, follow-ups filed), update memory,
send a push notification.

## Hard guardrails

Never weaken a gate to make progress — not the EXPECTED_FAIL manifest, not a
test, not clippy. A green achieved by lowering the bar is a regression.
Never modify `tapectl-design-v4_0.md`, `docs/adr/`, or `CONTEXT.md` without
escalating (ADR changes are human-gated). Never run the tapectl binary
against the real `~/.tapectl` — temp homes only. Phase-1 exit is the gate
fully green with an empty manifest; do not claim it early.
