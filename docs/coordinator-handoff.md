# Coordinator handoff: tapectl 2.0.0, Layout Version 3

Written 2026-11-17 by the coordinator session that ran the design, for the session that
takes the role over. The CTO's words: "handoff for opus to take over; make sure complex
decisions have been thoroughly determined".

You are the **coordinator**. The user is the **CTO**. The design is ruled; your work is to
carry it out through the plan, and to bring the CTO only what the plan says is theirs.
This page is the entry. Everything it names holds the detail, and is not repeated here.

## Where things stand

*This section was brought up to date on 2026-11-26. The rest of the page is as written on
2026-11-17, and its "On the go" order is how the build ran.*

- **The build is done through Stage 6.** `master` writes Layout 3 and is the 2.0.0 candidate.
  Stages 2 to 6 of `docs/design/v3-implementation-plan.md` are on it, with the golden pins,
  the recorded tape and every fix from the Stage 6 reads.
- **The proof of the candidate** is in `docs/runs/`:
  - the whole chain (`2026-11-22-2.0.0-t65-chain.md`, and
    `2026-11-25-2.0.0-chain-07144179.md` for the code as it stands);
  - the real drive (`2026-11-24-2.0.0-real-drive.md`);
  - the heir's script's speed (`2026-11-25-restore-sh-speed.md`).
- **The design is ruled**: ADR-0012 items 235 to 312, gathered for the format in ADR-0014.
- **What is left is Stage 7**, the CTO's and the operator's: the tag `v2.0.0`, the fresh
  start on the production host, and the drill (#548, #554, #559).
- **A review of the candidate** is `docs/audits/2026-11-26-pragmatic-design-and-defect-review.md`.
  Its decision list is for the CTO, before the tag.
- The production host still runs 1.9.2; its two cartridges and its catalog are test articles
  (items 235 to 238).

## The gate: the go

**Given on 2026-11-17 (ADR-0012 item 292; the CTO: "Go").** Stage 2's first wave (T2.1,
T2.2, T2.3) and T0.7 started that day. What follows is kept for how the gate was held.

The CTO has answered several lists with "ratify all". Each ratified what was recommended.
**None was the go**, each record says so, and the CTO has been told that the go needs the
word.

**Begin the plan's tasks when the CTO says "go", or names a stage to start.** Until then
the coordinator's work is to answer what the CTO asks (a megabyte-scale measurement on
this host is part of an answer) and to keep the records in step.

**One task does not wait: T0.3**, the made tree of two million files. Item 291 ratified a
recommendation to run it "now", and it builds nothing. It has not been started. It is a
worker's task with its decision rule in the plan; tell the broker first.

When the go comes, record it first: an ADR-0012 amendment item that quotes the CTO's
words and states the scope you read into them.

## On the go, in order

1. **Read** the plan's sections 1 to 6 and Appendix B, `.claude/commands/worktree-agent.md`
   and, in the Policy block of `.claude/commands/autopilot.md`, the two entries dated
   2026-11-17. You are ready
   when you can say, for T0.7, T2.1 and T1.1, who owns each and what each waits on.
2. **Start Stage 2's first wave, and the measurements beside it one at a time.** Stage
   2's first wave is three workers on `master`: T2.1, T2.2 and T2.3. The measurements pull
   images or fill disk, so one runs at a time, after the broker is told, in this order:
   T0.7 and T0.1 (they gate Stage 1), T0.5 and T0.6 (they gate the format's section on
   compression, and T4.5), T0.2 (it gates the host's step), and T0.3 if it has not run.
   Each has its decision rule written in its task, fixed before it runs. **A measurement
   that fails its rule stops only what it gates** (the plan's Stage 0 table says what);
   everything else goes on while the CTO answers. T0.4, the escrow paper, is the
   operator's and gates Stage 7 alone.
3. **Integrate each wave** as the plan says: the full suite with `--no-fail-fast`, the
   lints, a push, and master's CI read to its end. A wave is done when CI is green on the
   commit that holds it.
4. **Write Stage 1 yourself** (T1.1 to T1.7), once T0.1 and T0.7 are in. Stage 1 writes
   the ruled items and D1 to D22 into the format document. It settles nothing new. Its
   section on compression is written when T0.5 and T0.6 are in.
5. **Then the plan**, stage by stage, each stage on its own "Starts when" line.

## Who settles what

| Kind | What it is | Where it is | May the coordinator change it? |
|---|---|---|---|
| **Ruling** | the CTO's words | an ADR-0012 item | Never. Only a later item amends it. |
| **Ratified choice** | D1 to D22 | ADR-0012 item 290, restated in the plan's Stage 1 | Never. |
| **Decision rule** | what a measurement's result means, fixed before it runs | the plan's T0.x tasks | No. Run it; the result is the answer. A failed rule goes to the CTO. |
| **Determination** | the coordinator's working-out under a ruling | the plan, wherever a heading or a sentence says "determined": the format document's outline, D9's tables, D15's frames and levels, T4.5, T4.6, T4.8 and T4.9's points, the plan order and the rewrite's hold (T5.1), `stage for-location`'s read-back (T5.2), a source that holds a range (T5.3) | Yes, when the code shows it wrong: correct it in the plan and say so in the report. If the correction would change bytes on tape, the schema or a command's spelling, it goes to the CTO first. |
| **Reading** | the coordinator's sense of the CTO's words | an amendment's "Readings" list | A reading never withdraws a ruled default. In doubt, ask. |

Anything else that would change bytes on tape, the schema or a spelling: **stop and ask.**

## Asking this CTO

- The CTO answers a list with "ratify all". So number each question, give it one
  recommendation with its price, and say in the ask what a "ratify all" will be read to
  cover.
- Keep every open question consistent with every other. Two that contradict would both be
  ratified.
- **Ask for the go as a question of its own, outside any numbered list**, with the exact
  work it starts, and say that it needs the word.
- Say of each claim whether it is **measured** or **reasoned**, and with which tools.
- Findings live in GitHub issues and in the documents. A terminal message is not a record.

## What this design cost to learn

Each of these was a correction by the CTO or by a measurement. Apply them before you
recommend anything.

1. **The design is not bent to the production host's old tools** (item 273). The host is
   brought up to the design.
2. **Nothing is argued from what the present collection holds** (item 281).
3. **Before recommending a layer that "protects the rules", check that the simpler shape
   breaks one.** Compression's second shape was recommended on a claim that was not
   checked, and withdrawn.
4. **Judge a format choice by what one slice gives the stock tools alone**, with no file
   table, and state the result by what does not depend on which tar reads it: BusyBox tar
   stops where GNU tar skips on.
5. **A record claims no more than was measured.** When one did, it was corrected in the
   open: a dated note in the issue, or a later amendment.
6. **An amendment is right before it is pushed.** After the push it is corrected only by
   a later amendment.

## What is in force where an older text says otherwise

ADR-0012 is read in order and later items amend earlier ones. These are the places where
an older item, an issue body or a paragraph of `CLAUDE.md` misleads when read alone.

| The older text | What is in force |
|---|---|
| Items 240, 241: slices as "a plain cut of a tar stream" | Slices stay (282). A slice ends at a member's start (283). |
| Item 264: readers feed tar whole members by byte range | Its two prohibitions stand. A reader leaves a bad slice out and gives tar the good ones. Byte ranges stay inside tapectl's own reader (283, D10). |
| Items 9, 99, 112: units are never split; the `cut` unit | Every unit continues on the next cartridge at the early warning (288). |
| Items 82 to 84, 94, 95, 123 to 125, 147, and the second half of 122's order: spanning by opt-in | No opt-in, one class in the plan, one bound, the queue filled to the warning (288, 289, D19, D20). Item 122's first rule (owed slices first) stands. |
| Item 133: `volume rewrite` and compaction refuse a range | They carry it (288, D21). |
| Item 258(a): `spanning` is `requires`' one entry | `requires` is empty on every Layout 3 tape (D1). |
| Items 88, 132: the heir joins a span in scratch space | Withdrawn by item 267: one stream across the swap. Items 131, 266 and 267 stand. |
| Item 143: a range is never a source | Amended by item 260: a range is a source for the files it holds whole. |
| Item 243: the on-tape authorization ends at the install | It ends when the drill passes on the production run (268). |
| Item 253: real scale "before any code" | The made tree before the code; the largest real unit at the real-drive acceptance (291). |
| `CLAUDE.md`'s paragraph of 2026-11-16 (night) and #565's body, which call that night's amendment "too narrow" and a second compression shape "recommended" | The amendment stands as written. Each slice is compressed alone (284). The second shape is history. |
| The first default of 2026-11-14: a reader zero-fills a lost slice | Withdrawn by item 264. |
| `CLAUDE.md`'s dated paragraphs that call a question open | The latest paragraph on that question wins. |

## Where a word is defined

- `CONTEXT.md` is the glossary: Slice, Range, Spanning, Stage set, Volume, Cartridge.
- **D1 to D22**: the plan's Stage 1, "What Stage 1 writes down".
- **W1 to W9**, the writer at the early warning, and the **fill ceiling**:
  `docs/design/volume-format-v3.md` §5.2, carried from Layout 2's §4.3 word for word.
- **An owed slice, an owed run, the bound**: the plan's T5.1, "The plan, exactly".
- **The cut** is where the stream is divided into slices (D3). **`cut`** in code font is
  a write status of Layout 2 that item 288 removes. They are two things.
- **The tape chain**: the plan's T6.5. **The drill**: #548. **A golden pin**:
  `tests/on_tape_golden.rs`, and `CLAUDE.md` "Byte pins".
- **The broker, the Policy block**: `~/.claude/CLAUDE.md`, and
  `.claude/commands/autopilot.md`.

## Rules the plan does not carry

They are in `CLAUDE.md` (project and user), and in the memory index, and are not restated
here. The ones that have cost time before:

- **Tell the broker** (`BROKER NOTICE:` to the session `broker`) before a push that starts
  CI and before any heavy job. Run `~/git/broker/status -q` first.
- **Debug and check builds only**, under `flock /scratch/tapectl-build.lock`, with the
  target directory under `/scratch`.
- **Every `docker run` has `--log-driver none`**, carries no data on its standard output,
  and its image is removed when the job is done.
- **The real drive and the production host are the operator's.** Workers touch no tape
  device.
- **`git stash` and `git commit --amend` are not used**; a commit message names no closing
  keyword; master's CI is read after every push.
- **A golden pin moves only in a commit of its own**, by the coordinator, under item 243.

## When to stop and ask

The plan's Appendix B is the whole list: the go, a measurement that fails its rule, a
question a task stops on, each real-drive run and the tag, and a serious finding of a
read.
