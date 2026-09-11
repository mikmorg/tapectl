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
| 2 | #130 — ID thunk and system guide hardcode `mt -f /dev/nst0` | **landed** (3 sites, not the 2 named) |
| 3 | `dl.scenario_b` — bare `import` rebuilds only a volumes row | **deferred** → [#136](https://github.com/mikmorg/tapectl/issues/136) |
| 4 | `rfc.restore_file_symlink` — `restore file` dereferences symlinks | **landed** |
| 5 | #132 — `quick-archive --volume` says only "volume not found" | **landed** (option 1, per the ratified #124b precedent) |
| 6 | #125 — the `escrow: no` markers in `report copies` / `catalog locate` | **landed** |
| 7 | #126 — `backend add` command | **landed** |
| 8 | #1 — wayfinder map refresh | queued |

Discovered work is filed as an issue and listed under Discoveries, not worked —
unless it is a regression this run caused, or it blocks a queued item.

## Progress log

| When (UTC) | Item | Outcome |
|---|---|---|
| 09-11 06:18 | — | run opened; skill, decisions file and this file created |
| 09-11 08:40 | #126 | landed. `tapectl backend add`, appended as text so comments survive. **The end-to-end run found a bug no unit test could**: `init` serialized `lto = []`, which TOML rejects alongside a later `[[backends.lto]]` table — the command could not append to the file `init` had just written. Fixed both ends. Verified against the real LTO-6 by-id path: add exit 0, `config check` exit 0, duplicate name rejected. 720 lib tests (+6). |
| 09-11 08:28 | #125 | landed, closing the issue. New `policy::escrow` holds the fail-closed classification; `audit`, `catalog locate` (new Escrow column) and `report copies` (per-unit note + `volumes_without_escrow` in --json) all route through it. 714 lib tests (+5). No gate: nothing under the restore-path path set. |
| 09-11 08:15 | #132 | landed. Pre-flight volume check + actionable error + clap help + man pages. Settled as option 1 on the CTO's own 2026-09-10 Q4 precedent (error+example now, ergonomic command later); auto-init left open on the issue. Found a second problem: the old check fired inside `volume_write`, so three steps' work was done and staged before failing. 709 lib tests (+1), quick-archive scenario 14/14 on mhvtl. |
| 09-11 08:06 | `rfc.restore_file_symlink` | landed. `restore file` now preserves symlinks, and a second bug in the same lines is fixed: `.exists()` followed the link, so a dangling symlink was reported "not found in restored unit" though dar had restored it correctly. 708 lib tests (+3), gate GREEN 26/26, and `restore-file-and-catalog` 10/10 **on mhvtl tape** — the check itself now passes, not just the unit tests. |
| 09-11 08:02 | `dl.scenario_b` | **deferred**, not fixed. `import`'s job is registration; whether disaster recovery should rebuild the catalog from tape is a genuine fork the normative set does not settle. Surfaced the constraint that decides it: the plaintext zones carry no unit names by invariant, so any rebuild is a *keyed* operation. → #136, `needs:cto`. |
| 09-11 07:55 | #133 | CI red on `89fa1f6` — the new argv test spawns the heir script, whose prereq loop needs mt/age/dar; CI has none. Fixed hermetically with PATH stubs (`da18e61`), verified against a reconstructed CI PATH. Issue reopened until CI is green. **Lesson: a test that spawns the generated script inherits its tool requirements.** |
| 09-11 07:48 | #130 | landed. Caveat added to all THREE heir documents — the issue named two; RECOVERY.md had the same defect. 705 lib tests (+1), gate GREEN 26/26. Settled, not deferred: correcting wrong instructions is not a design fork. Owes the same real-drive pass as #133. |
| 09-11 07:35 | #133 | landed. 704 lib tests (+2), clippy/fmt clean, gate GREEN 26/26 on `/dev/nst1`. All 3 negative controls confirmed failing pre-fix at distinct assertion lines. **Owes a real-drive confirmation pass** — it changes File 2's bytes; batched to end of queue. |

## Discoveries

| Finding | Issue |
|---|---|
| `layout_version = 1` in the envelope on a v2 tape — separate schema, or stale constant? **deferred** | [#134](https://github.com/mikmorg/tapectl/issues/134) `needs:cto` |
| `config check` reports `block_size`/`hardware_compression` as "parsed but not consumed" for a backend that never set them — the decorative scan reads the parsed config, so serde defaults look like operator choices | not filed (cosmetic; noticed working #126) |
| `/^name = /` unguarded by table context in three heir awks — safe today, silent slice loss if any table gains a `name` key | [#135](https://github.com/mikmorg/tapectl/issues/135) |
