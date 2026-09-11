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
| 8 | #1 — wayfinder map refresh | **done** — status comment posted, closure recommended, not closed |

Discovered work is filed as an issue and listed under Discoveries, not worked —
unless it is a regression this run caused, or it blocks a queued item.

## Real-drive confirmation pass — DONE 2026-09-11 12:45 UTC

#133 and #130 changed frozen on-tape bytes (File 0, File 1, File 2 and the
envelope's RECOVERY.md) and owed a pass on real hardware. It was blocked for a
few hours on Claude Code's auto-mode classifier, which refuses
`--i-will-lose-the-cartridge` as irreversible deletion; the CTO granted the
permission and it ran.

    ./scripts/lifecycle-suite.sh --scenario first-year --device /dev/nst0 \
        --erase short --single-cartridge --i-will-lose-the-cartridge EW7VWMVKF6

**45 checks, 45 passed, 0 failed, 0 skipped** on the real HP LTO-6.

The suite passing is not the same claim as "the new bytes are on the tape", so
each changed zone was read back off the cartridge directly:

| Zone | Evidence |
|---|---|
| File 0, ID thunk | carries "Your drive may not be /dev/nst0 ... example, not a fact" and `ls -l /dev/tape/by-id/` |
| File 1, system guide | carries the `## Which device?` section and "recover the WRONG volume" |
| File 2, RESTORE.sh | all four #133 fixes present in the on-tape bytes |
| Envelope RECOVERY.md | carries the caveat, decrypted from the tape with alice's key |

And the script read *off the cartridge* was executed, not merely grepped:
an unknown mode exits **2**, a truncated `--key` prints `FATAL: --key needs a
value` instead of dying silently, and `--info` announces
`Tape device: /dev/nst0 (from TAPE_DEVICE)` / `Tape identifies as: VOL-A` /
`Verdict: SEALED`.

## Progress log

| When (UTC) | Item | Outcome |
|---|---|---|
| 09-11 06:18 | — | run opened; skill, decisions file and this file created |
| 09-11 08:52 | — | **RUN CLOSED: the queue is empty.** One of the three ratified stop conditions. Closeout below. |
| 09-11 08:50 | #1 | status refresh posted. The map's destination was met long ago; recommended closing it, did not close it (maps and epics are surfaced, never closed unilaterally — the #27 rule). |
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

## Closeout

**Queue: 8 items — 6 landed, 1 deferred, 1 documented. One obligation blocked.**

| Commit | What |
|---|---|
| `0f1257d`, `6e41700` | the run's own scaffolding: skill, run file, decisions file, Policy refresh |
| `89fa1f6`, `da18e61` | #133 — four heir-path defects + hermetic-test fix |
| `69e7998` | #130 — device caveat in all three heir documents |
| `7af0c64` | #136 deferral recorded |
| `a7ce547` | `restore file` preserves symlinks |
| `3277acb` | #132 — quick-archive pre-flight + actionable error |
| `108a9df` | #125 — escrow markers in locate and copies |
| `e1f9c6d` | #126 — `backend add` |

Closed: #125, #126, #130, #132, #133. Deferred to the CTO: #134, #136.
Filed and left: #135. Recommended for closure: #1.

702 lib tests at the start, **720** at the end. Gate GREEN 26/26 on every
tape-path commit. CI green at every push except the one real failure below.

### What went wrong, and what it taught

**Two tests passed for the wrong reason, both because the fixture did not
resemble the real artifact.**

1. #133's argv test spawned the generated RESTORE.sh, inheriting that script's
   requirement for `mt`/`age`/`dar`. This box has them; CI does not. Green here,
   red there.
2. #126's unit tests built configs with no `[backends]` table, so they never met
   the `lto = []` stub that made `backend add` fail against a real `init`-written
   config with "invalid table header".

Both were caught by running the thing for real — CI in the first case, an
end-to-end command against the real drive in the second. Neither was caught by
adding more unit tests. The pattern is worth naming: **a fixture that is simpler
than the artifact tests something that does not exist.**

**Two issues understated their own scope.** #130 named two documents carrying
`mt -f /dev/nst0`; there were three. #132 was filed as a message problem; the
check also ran too late, after a unit, a snapshot and a staged slice set had been
created and left behind. Reading past the issue's own scope paid both times —
the #91 lesson, still holding.
