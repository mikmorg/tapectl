# Unattended run — 2026-09-11

Rules: `.claude/skills/unattended-run/SKILL.md`. Mechanics: `/autopilot`.

**Wall-clock bound: end of Sunday 2026-09-13.** Started Friday 2026-09-11 06:18 UTC.
Ends earlier if the queue empties or two consecutive iterations land no commit.

**Cold start:** read this file top to bottom, then `git log --oneline -15`. The
queue below is authoritative; the log says how far it got.

## Devices (verified 2026-09-11 06:18 UTC, post-reboot)

| Device | Serial | Role |
|---|---|---|
| `/dev/nst0` | `scsi-HUJ808A5L4-nst` | real HP LTO-6, cartridge `EW7VWMVKF6` (expendable) |
| `/dev/nst1`–`nst4` | `scsi-XYZZY_A1..A4` | mhvtl, changer `/dev/sg4`, 43 slots loaded |

Resolve by serial, never by number — they move across reboots.

## Queue (ordered by heir-path risk)

| # | Item | State |
|---|---|---|
| 1 | #133 — RESTORE.sh heir findings (4 defects + the missing test) | queued |
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

## Discoveries

| Finding | Issue |
|---|---|
| `layout_version = 1` in the envelope on a v2 tape — separate schema, or stale constant? **deferred** | [#134](https://github.com/mikmorg/tapectl/issues/134) `needs:cto` |
| `/^name = /` unguarded by table context in three heir awks — safe today, silent slice loss if any table gains a `name` key | [#135](https://github.com/mikmorg/tapectl/issues/135) |
