# Unattended run — 2026-09-11

Rules: `.claude/skills/unattended-run/SKILL.md`. Mechanics: `/autopilot`.

**Wall-clock bound: end of Sunday 2026-09-13.** Started Friday 2026-09-11 06:18 UTC.
Ends earlier if the queue empties, or two consecutive iterations neither land,
defer, nor file.

**Cold start:** read this file top to bottom, then `git log --oneline -15`. The
queue below is authoritative; the log says how far it got.

**Known failure mode — nothing restarts this run.** The loop keeps working while
the session lives, but no scheduler can revive it: session cron is in-memory and
dies with the session, and scheduled *cloud* agents run in a sandbox with no
`/dev/nst0`, no mhvtl and no passed-through drive. This work is bound to this VM.
So if the progress log stops early, the session dropped — that is the expected
shape of the failure, not a crash to investigate. Resume by reading this file and
picking the first queue row that is not `landed`.

## Devices (verified 2026-09-11 06:18 UTC, post-reboot)

| Device | Serial | Role |
|---|---|---|
| `/dev/nst0` | `scsi-HUJ808A5L4-nst` | real HP LTO-6 — **sanctioned cartridge `EW7VWMVKF6`** (FUJIFILM, 2017), declared expendable by the CTO 2026-09-11: standing consent for `--i-will-lose-the-cartridge EW7VWMVKF6`, that cartridge only |
| `/dev/nst1`–`nst4` | `scsi-XYZZY_A1..A4` | mhvtl, changer `/dev/sg4` (`mtx status` for slots) |

Resolve by serial, never by number — they move across reboots.

## Build discipline on this VM

**Another Claude session shares this machine** (seen 2026-09-11 07:40: a
`cargo test --all-targets` under `/scratch/homorg`, target dir
`/scratch/homorg-target`). Two Rust builds on 10 GB do not fit, and the loser is
killed. Consequences for this run:

- Run cargo in the **foreground** with `CARGO_BUILD_JOBS=1`. Backgrounded cargo
  is what gets OOM-killed — twice on 09-11 before this was diagnosed.
- An OOM kill is contention, **not** a broken build. Re-run it; do not start
  debugging a failure that never happened.
- Leave the other session's processes alone. They are someone else's work.

## Queue (ordered by heir-path risk)

| # | Item | State |
|---|---|---|
| 1 | #133 — RESTORE.sh heir findings (4 defects + the missing test) | **landed** |
| 2 | #130 — ID thunk and system guide hardcode `mt -f /dev/nst0` | queued |
| 3 | `dl.scenario_b` — bare `import` rebuilds only a volumes row | queued |
| 4 | `rfc.restore_file_symlink` — `restore file` dereferences symlinks | queued |
| 5 | #132 — `quick-archive --volume` says only "volume not found" | queued |
| 6 | #125 — the `escrow: no` markers in `report copies` / `catalog locate` | queued |
| 7 | #126 — `backend add` command | queued |
| 8 | #1 — wayfinder map refresh | queued |

Discovered work is filed as an issue and listed under Discoveries, not worked —
unless it is a regression this run caused, or it blocks a queued item.

## Progress log

| When (UTC) | Item | Outcome |
|---|---|---|
| 09-11 06:18 | — | run opened; skill, decisions file and this file created |
| 09-11 07:35 | #133 | landed. 704 lib tests (+2), clippy/fmt clean, gate GREEN 26/26 on `/dev/nst1`. All 3 negative controls confirmed failing pre-fix at distinct assertion lines. **Owes a real-drive confirmation pass** — it changes File 2's bytes; batched to end of queue. |

## Discoveries

| Finding | Issue |
|---|---|
| `layout_version = 1` in the envelope on a v2 tape — separate schema, or stale constant? **deferred** | [#134](https://github.com/mikmorg/tapectl/issues/134) `needs:cto` |
| `/^name = /` unguarded by table context in three heir awks — safe today, silent slice loss if any table gains a `name` key | [#135](https://github.com/mikmorg/tapectl/issues/135) |
