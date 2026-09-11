---
name: unattended-run
description: Rules for working the tapectl queue while the CTO is away.
disable-model-invocation: true
---

# Unattended run

`/autopilot` owns the per-iteration mechanics: pick one task, land it or escalate
it, continue. This file owns what changes when nobody is there to ask. Ratified
by the CTO 2026-09-11.

Every run has a **run file** — `docs/runs/<date>-<name>.md` — holding the ordered
queue, the wall-clock bound, and the progress log. It is the cold-start entry
point: a session that dies mid-run is resumed by reading it, never by
reconstructing from memory.

## The queue is closed

Work only what the run file lists. Discovery during a run is normal and does not
expand it: file the finding as an issue, note it in the run file, and move on.

Two things earn immediate work instead — a regression this run caused, and a
defect blocking a queued item. Everything else waits for the CTO, or the run
authors its own backlog faster than it burns it down.

## Three tiers for a decision

Sort every fork by how hard it is to undo.

- **Settle** — reversible and inside a queued item's scope. Decide it and justify
  the choice in the commit message.
- **Defer** — hard to undo: bytes frozen onto tape, DB schema, CLI surface.
  Implement nothing. Append the question to `docs/decisions-pending.md`, open an
  issue labelled `needs:cto`, and move to the next item. Both surfaces on
  purpose: the CTO can answer either one from a phone.
- **Halt** — safety, or anything irreversible. Stop the run and wait.

A fork that feels like *settle* but touches a **red line** below is a *halt*.

## Tape work runs on mhvtl

mhvtl has 4 drives and 43 loaded slots, so `mtx` gives multi-cartridge scenarios
unattended. That is the default for every tape operation.

Resolve every device by serial through `/dev/tape/by-id/` — `scsi-HUJ808A5L4-nst`
is the real HP LTO-6, `scsi-XYZZY_A*` are mhvtl. The `nstN` numbers move across
reboots, and the drives have swapped places at least once.

The real drive earns a **confirmation pass** only for changes to tape bytes,
batched to the end of the queue rather than run per-item. It holds cartridge
`EW7VWMVKF6`, which the CTO has declared expendable — standing consent for
`--i-will-lose-the-cartridge EW7VWMVKF6`. There is no autoloader, so that one
cartridge is the whole real-hardware budget until the CTO returns.

## Landing work

One commit per finding, straight to master, pushed once `clippy`, `fmt --check`
and `cargo test` are clean locally. Rollback is `git revert`; a pile of unreviewed
PRs is worse for the CTO than a pile of revertable commits.

What "done" requires scales with what the change touches:

| Touches | Must be green before the next item |
|---|---|
| anything | unit + integration tests, clippy, fmt, CI |
| the heir or tape path | `scripts/mhvtl-verify-gate.sh` — 26/26 |
| the suite or lifecycle | `scripts/lifecycle-suite.sh` multi-cartridge on mhvtl |
| bytes written to tape | a real-drive confirmation pass, batched to the end |

## Stopping

The run ends on whichever fires first: the queue empties, **two consecutive
iterations land no commit** (the loop is spinning), or the wall-clock bound in
the run file passes.

Write the closeout into the run file before stopping — what landed, what
deferred, what is still open.

## Red lines

Hold these unless the CTO lifts one explicitly. Each is a halt, not a judgement
call.

- Run the binary against temp homes only; `~/.tapectl` is the CTO's real archive.
- Write to `/dev/nst0` only as the sanctioned confirmation pass on `EW7VWMVKF6`.
- Leave the gate's `EXPECTED_FAIL` manifest, the tests, and clippy exactly as
  strict as they are. A failing check is a finding, and it may only shrink.
- Leave published history alone: no force-push, no rewriting, no reboots.
- Close only issues this run actually resolved.
- Build debug; a release artifact needs the CTO to ask for one.
