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

## Queue — REOPENED 2026-09-11 (CTO grilling on open decisions, ratified)

The CTO was grilled on the four items round 1 left open and ratified every
answer. #134 and #135 are no longer deferred; #136 is authorized to build.
Same rules as round 1 (`.claude/skills/unattended-run/SKILL.md`).

| # | Item | State |
|---|---|---|
| 9 | #134 — drop the stale `layout_version = 1` from the envelope manifest | **landed** `64d3f91` |
| 10 | #135 — scope the heir version selector to the `[[units]]` head | **landed** `64d3f91` |
| 11 | #136 — `catalog rebuild --from-volume --key K` | **landed** `edef129` |

Ratified #136 design: operator/escrow key only (a tenant key is refused and
pointed at `RESTORE.sh`); an `events` row for provenance, **no new column**;
insert-missing-only and idempotent across cartridges; slice positions read from
the age-authenticated envelope manifest, never the plaintext front index; does
not bundle verification — `volume verify` already exists.

A real-drive confirmation pass is owed at the end of this round for the
#134/#135 heir-path byte changes; it is batched, not per-commit.

## Real-drive confirmation pass #3 — DONE 2026-09-11 18:15 UTC (catalog.db shape)

`4292cf9` changed operator-envelope bytes on every future tape: `catalog.db`
gained `tenants`, `stage_sets.key_fingerprints` and `stage_slices.sha256_plain`.
Same command, same guard (`sg_read_attr` must report `EW7VWMVKF6` before the
erase):

    ./scripts/lifecycle-suite.sh --scenario first-year --device /dev/nst0 \
        --erase short --single-cartridge --i-will-lose-the-cartridge EW7VWMVKF6

**45 checks, 45 passed, 0 failed, 0 skipped** on the real HP LTO-6.

Then the operator envelope the matrix had dumped raw off the cartridge
(`matrix-fy-big/raw/0006_operator_envelope.bin`, 52,227 bytes) was decrypted
with the operator key and its `catalog.db` queried:

| Evidence off `EW7VWMVKF6` | Result |
|---|---|
| tables | `tenants, units, snapshots, stage_sets, stage_slices, files` |
| `stage_sets` columns | `…, total_encrypted_size, key_fingerprints` |
| `stage_slices` columns | `…, encrypted_bytes, sha256_plain, sha256_encrypted` |
| `tenants` rows | `alice, bob` |
| receipts | 3 of 3 stage sets carry `key_fingerprints` |
| plaintext hashes | present on all 14 slices |

That is the whole of review finding 2 (a), on tape.

## Real-drive confirmation pass #2 — DONE 2026-09-11 14:50 UTC (#134/#135)

`64d3f91` changed frozen on-tape bytes again: the envelope `MANIFEST.toml`
(#134 dropped `layout_version = 1`) and File 2's RESTORE.sh (#135 scoped the
version selector to the `[[units]]` head). The CTO granted the permission and
it ran, with a guard that aborts unless `sg_read_attr /dev/sg0` reports the
sanctioned expendable cartridge:

    ./scripts/lifecycle-suite.sh --scenario first-year --device /dev/nst0 \
        --erase short --single-cartridge --i-will-lose-the-cartridge EW7VWMVKF6

**45 checks, 45 passed, 0 failed, 0 skipped** on the real HP LTO-6.

As in pass #1, the suite passing is not the same claim as "the new bytes are
on the tape", so both changed zones were read back off the cartridge:

| Change | Evidence off `EW7VWMVKF6` |
|---|---|
| #135, File 2 | `dd` from tape file 2 carries the `in_head` guard at lines 675-691 (`/^\[\[units\]\]/ ... in_head = 1`, `/^\[/ { in_head = 0 }`, `in_u && in_head && /^snapshot_version = /`), and the extracted script is `bash -n` clean |
| #134, envelope manifest | the envelope at tape file 4, decrypted with alice's key, has a `[manifest]` header of `volume` / `tenant` / `created_at` and **zero** occurrences of `layout_version` anywhere |

And the script read *off the cartridge* was executed, not merely grepped:
`--find-envelope --key alice-primary.age.key` walked the ID thunk, found the
tenant envelope at file 4, decrypted it, and printed the slice map
(`tape_position = 8..14`) — which is the #135 selector doing its job on real
tape.

## Real-drive confirmation pass #1 — DONE 2026-09-11 12:45 UTC

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
| 09-11 18:05 | review | **db-loss 6/6 on mhvtl with arm (d) rewritten as the DR procedure** (finding 4, T1): `init --no-escrow` → `key import --escrow` the ORIGINAL → rebuilt rows read `escrow: yes` (the receipts rode the tape in the new `catalog.db`) and `audit` has no escrow findings. Then the mistake, in a second home: plain `init` → rebuild → `escrow: NO` and `audit` names `escrow_identity_mismatch` exactly once with the `key import --escrow age1…` command. The fixture no longer avoids escrow; it measures it. |
| 09-11 18:00 | review | `e0cfbff` — `catalog locate --json` never carried the Escrow marker the table showed since #125; it does now. Found because arm (d) needed to read it. |
| 09-11 17:58 | review | `3249ac6` — **attestation** (finding 2/#137, Q2/Q10): `catalog rebuild --key <escrow>` decrypts one slice HEADER per rebuilt stage set and records the coverage it proved. New `Store::read_file_head` (TapeStore stops after the first block). Applies only when the supplied key is the REGISTERED escrow recipient — finding 4's import step is now load-bearing. A unit test pins the age header-prefix assumption. Gate GREEN 26/26. Suite green on the tree (745 lib). The commit chain was OOM-killed once mid-clippy by the other session's build; nothing was lost, re-run. |
| 09-11 17:50 | review | Filed [#139](https://github.com/mikmorg/tapectl/issues/139) (`init --escrow-public-key`, Q6 option ii). Sharpened by a fact found writing the DR text: **no command replaces a registered escrow identity**, so after a plain post-disaster `init` the operator cannot import the original — the only recovery is to delete the still-empty home and re-init with `--no-escrow`. The guide says so. |
| 09-11 17:45 | review | `4292cf9` — **`catalog.db` is a complete rebuild source** (finding 2, Q1/Q9): `tenants`, `key_fingerprints`, `sha256_plain` ride the operator envelope; rebuild probes the shape and falls back to envelopes for older tapes. Old-shape test fixture is derived from the real generator by dropping the three additions. Gate GREEN 26/26 (envelope bytes changed; real-drive pass owed at the end). 870 tests. |
| 09-11 17:36 | review | `d5c9638` + `08b1dd1` — **third escrow state** (finding 2/#137, Q3/Q7/Q8/Q12): migration 010 `stage_sets.origin`; `policy::escrow` classifies covered / unknown / gap through one function; `?` in locate; write gate still refuses; `audit` gains the one-line `escrow_identity_mismatch` diagnosis naming the former key. CI red once on fmt (test added after the last fmt pass), fixed. |
| 09-11 17:10 | #138 | `ba8a4a1` — **audit scopes per check**: coverage/encryption/verify-age/escrow over active + tape_only + missing; dirty scan stays active-only by construction. Re-measured with the real binary across all four statuses: 3/3/3/0 violations. CI green. |
| 09-11 16:40 | review | CTO grilled on the review's open items across two rounds and ratified all thirteen answers; queue reopened with the six-item plan (#138 → third state → catalog.db → attestation → docs → fixture). `/scratch` filled (282/295 GB — 148 GB of it the other session's build); freed 24 GB of tapectl's own superseded output, nothing else touched. |
| 09-11 14:50 | #134/#135 | **real-drive pass #2 DONE — 45/45 on the HP LTO-6.** The CTO granted the permission. Both changed zones read back off `EW7VWMVKF6`: File 2 carries the `in_head` guard and is `bash -n` clean; the envelope at file 4, decrypted with alice's key, has no `layout_version`. The script taken off the cartridge was executed, not grepped — `--find-envelope` found and decrypted the tenant envelope and printed `tape_position = 8..14`. |
| 09-11 14:45 | #136 | **follow-up: a rebuilt catalog was QUIETER than the truth.** Rebuilt units were inserted `tape_only`; `audit` scopes every per-unit check to `status = 'active'`, so the rebuilt catalog reported 0 violations where the one it replaced reported 3 `copy_count` violations for the same units on the same tape. Fixed to `active` — which is also the honest value, since `tape_only` is a policy state `mark-tape-only` sets after checking preconditions. Negative control confirmed red. Also: `volumes.backend_name` was the invented literal `"rebuilt"`; now resolved from config like `volume_import`. And `volume verify --full`, which the command's own output tells the operator to run next, was never exercised — it is now arm (d)'s last step, 23/23. 839 tests (+3), gate GREEN 26/26, db-loss 6/6. |
| 09-11 14:20 | #134/#135 | real-drive pass **BLOCKED AGAIN**. The auto-mode classifier refused `--i-will-lose-the-cartridge` as irreversible deletion; the grant the CTO made at 12:40 was session-scoped and did not persist as a rule in `.claude/settings.local.json`. Not retried in variant forms. Everything the pass would confirm is already green on mhvtl (gate 26/26, including `heir_restore` and `heir_find_envelope` against the new File 2); what is outstanding is narrowly "the same bytes land on real LTO-6 hardware". |
| 09-11 14:12 | #136 | landed `edef129`, closing the issue. Gate GREEN 26/26. **db-loss 6/6 — green for the first time**: arm (b) was this suite's last expected failure, and arm (d) rebuilt 3 units / 2 tenants / 14 slices / 22 file rows off mhvtl tape, restored `photos` byte-identical through the rebuilt rows, then rebuilt again to all zeros. 730 lib tests (+7), 836 total. |
| 09-11 14:05 | #136 | implemented. New `volume::envelope` (the first Rust code that reads an envelope BACK — the write path packed them and only bash ever unpacked them) and `volume::rebuild`. 9 integration tests drive the **real** write session into a `MemStore` and rebuild from those exact bytes; the load-bearing assertion runs `restore_unit`'s verbatim resolution join. Two negative controls confirmed red at distinct assertions. |
| 09-11 13:40 | #136 | **two of the three design questions collapsed on evidence, one deferred.** `tenants` has no `public_key` (it is on `encryption_keys`) and `restore` loads identities from `keys/` on disk — so a rebuilt tenant row is structurally complete, not a stub. Manifest-authoritative is forced, not chosen: `catalog.db` carries neither `sha256_plain` nor `tape_position`. The third — rebuilt sets can never show escrow coverage — went to the CTO as #137. |
| 09-11 13:20 | — | queue reopened after the CTO ratified all three grilling rounds. #134/#135 landed `64d3f91`; #136 authorized to build. |
| 09-11 13:15 | #135 | closing comment posted correcting the issue's own miscount: ONE unguarded matcher, not three. |
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

**Round 1 — queue: 8 items — 6 landed, 1 deferred, 1 documented. One
obligation blocked.** Round 2 reopened the queue above with #134/#135/#136.

**Round 2 — queue: 3 items, all landed.** #134 and #135 in `64d3f91`; #136 in
`edef129` + `60dca6f`. 723 lib tests at the start of round 2, **730** at the
end; 839 total (the round's last three are integration tests). Gate GREEN 26/26 on every tape-path commit, CI green on every
push, and `db-loss` reached 6/6 — the first time that scenario has been fully
green, because its arm (b) was the suite's last standing expected failure.

Deferred to the CTO: **#137** (a rebuilt catalog can never prove escrow
coverage) — the only thing left open. The **second real-drive confirmation
pass is DONE**, 45/45, with both changed zones read back off the cartridge.

Round 2's lessons, both of which cost a real correction:

- **Measure the thing you are about to tell someone to rely on.** Two of the
  three defects found after the first commit — the silent `audit` downgrade
  and the never-run `volume verify` — were invisible to 9 passing tests and
  a green gate, and fell out of running `audit` once against the catalog the
  command had just built. This is the round-1 fixture lesson at one more
  remove: not only must the fixture match the artifact, the artifact must be
  used the way the docs say to use it.
- **A status is a claim.** `tape_only` looked like a neutral description of a
  unit whose data is on tape. It is a policy state with enforced
  preconditions and an audit consequence, and inferring it from a data fact
  silenced every check that mattered. The same shape as #105: the downgrade
  was silent, so the tool could not tell.

**Round 3 — the CTO's design review, all six items landed, attended.**
"review all of this, I think it points to design gaps that need addressing"
produced `docs/audits/2026-09-11-rebuild-findings-review.md`: one live defect
and four design gaps, grilled across two rounds, thirteen answers ratified.
Landed in order, one commit each, master `103ba99`:

| commit | item |
|---|---|
| `ba8a4a1` | #138 — `audit` scopes per check; tape-only units are audited again (3/3/3/0 across the four statuses, re-measured with the real binary) |
| `d5c9638` `08b1dd1` | third escrow state (`stage_sets.origin`, `?`), `escrow_identity_mismatch` diagnosis |
| `4292cf9` | `catalog.db` is a complete rebuild source — tenants, receipts, plain hashes on the tape |
| `3249ac6` | attestation: `catalog rebuild --key <escrow>` decrypts one slice header per stage set; `Store::read_file_head` |
| `e0cfbff` | `catalog locate --json` carries the escrow marker |
| `a7f2921` | db-loss arm (d) follows the DR procedure and measures both escrow states on tape |
| `103ba99` | DR procedure as a composition; "self-describing" defined; review corrections |

745 lib tests at the end of round 3 (730 at its start). Gate GREEN 26/26 on
every tape-path commit, CI green at every push but one (fmt, fixed), db-loss
6/6 with escrow measured, real-drive pass #3 45/45 with the new envelope
bytes read back off the cartridge. #137 closed; #139 filed as the follow-up.

Round 3's lessons:

- **Measure the thing before describing it.** Every prediction about how a
  rebuilt catalog would misbehave was wrong in direction or degree until it
  was run: "noisy" was actually *quiet* (#138), "violation" was a *warning*,
  "niche" was *default-on*. The review's value came from four commands run
  against a real artifact, not from reading.
- **A fact found while writing the docs changed the design.** "No command
  replaces a registered escrow identity" surfaced while writing the DR text,
  turned finding 4 from advice into a hard ordering constraint, made
  attestation's registered-key guard load-bearing, forced arm (d) into two
  homes, and produced #139. Docs are not the last step; they are a test.
- **The other session's build is a real constraint on this VM.** Two commit
  chains were OOM-killed mid-`clippy`; nothing was lost because git steps
  were ordered before cargo steps. Keep it that way.

**Round 4 — the architecture review's seven deepenings, attended, via
`/autopilot do all`.** Report: `/tmp/architecture-review-20260911-180729.html`
(and the 📼 artifact). Each candidate went to a sonnet worker in its own
worktree; the coordinator reviewed, cherry-picked, gated and pushed. Byte pins
for MANIFEST.toml and RESTORE.sh (`fcad514`) landed before anything could
move them. Order landed: C3 `..ed14c92` → C1 `..7b26f57` → C6 `..6551b46` →
C5 `..e6d800c` → C2 `..d13de21` → C4+C7 `..ae2c4ee`. 745 lib tests at the
start, **784** at the end (902 total). Gate GREEN 26/26 after every tape-path
landing; CI read after every push. CTO batch answered: no catalog.db stamp;
add the twelve table-only `--json` columns (C2b, in flight); keep
`in_service` for escrow findings. Two lessons: `git stash` is shared across
worktrees (a worker popped another's entry, noticed, restored — now banned in
the template); and a review that measures four things beats one that reads
forty files. Details per item in `.claude/commands/autopilot.md`.

One process note, twice over: `Closes #NNN` in a commit message auto-closes
the issue **on push, before CI finishes**. It went green both times, but the
#133 rule stands — do not treat an issue as closed until CI is read.

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
