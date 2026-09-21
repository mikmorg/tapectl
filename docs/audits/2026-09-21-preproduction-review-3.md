# Third pre-production adversarial review — 2026-09-21

**Range:** `057a6591..HEAD` (45 commits, 36 non-doc files, ~5,385 insertions) — everything
landed since the 2026-09-18 review was recorded, which that review therefore never saw.
Run when the `review-2026-09-13` label emptied for the third time, per Policy rule 7, before
the CTO's real-drive rehearsal on home2.

**Method:** a 31-agent workflow — ten dimension finders, one adversarial verifier *per
finding* (told to REFUTE and to default to "not a defect" when unsure), and a completeness
critic. Agents were read-only: no builds, no tape, no execution of anything in `scripts/`.

**Result: 20 raw findings, 8 confirmed, 12 refuted.** A 60% refutation rate, in line with
the previous two rounds and the ratio to want. Filed as **#276-#287**: 2 high, 8 medium,
2 low.

## Confirmed

| # | Sev | Area | Finding |
|---|-----|------|---------|
| [#276](https://github.com/mikmorg/tapectl/issues/276) | high | retire family | `retire_impacts` filters `writes.status = 'completed'`, so a confirm-failed volume yields zero impacts and the ADR-0008 Tier-3 zero-copy floor never fires — the only copy can then be erased |
| [#277](https://github.com/mikmorg/tapectl/issues/277) | high | write session | An `Inconclusive` confirm whose cause is an unreadable seal makes `check_tape_contact` return `Matches`, so `volume resume` falls through the empty arm to `reposition_for_resume` and `seal()` — a write to a physically sealed cartridge with ADR-0003's refusal bypassed |
| [#278](https://github.com/mikmorg/tapectl/issues/278) | medium | audit remedy | `audit --action-plan` auto-emits `staging clean --unit X --force`, and the force branch has no `EXISTS writes` guard, so the recipe also discards unit X's never-written staged sets |
| [#279](https://github.com/mikmorg/tapectl/issues/279) | medium | operator text | `stage create --version`'s refusal names a `staging clean --unit` that releases nothing when the live set has no `writes` row, and blames min_copies for a retention with a different cause |
| [#280](https://github.com/mikmorg/tapectl/issues/280) | medium | quarantine | `volume verify`'s clean-clear prints "it counts as a copy again" without consulting `volumes.status` — false for `retired` and for the sealed-but-`initialized` state |
| [#281](https://github.com/mikmorg/tapectl/issues/281) | medium | operator text | `volume resume`'s help and man page still promise quarantine-on-already-sealed, contradicting the recovery command the same diff prints |
| [#282](https://github.com/mikmorg/tapectl/issues/282) | medium | harness | `permute`'s newly-enabled restore matrix never loads the cartridge it asserts against — red or green by RNG |
| [#283](https://github.com/mikmorg/tapectl/issues/283) | medium | harness | `first-run.sh`'s subset-recovery prose documents a path that aborts the script on re-entry |
| [#284](https://github.com/mikmorg/tapectl/issues/284) | medium | tests | `execute_batch`'s `CleanScope::Units` scoping — #248's actual fix — is revert-silent; the test hand-copies the tail instead of calling `execute_batch` |

## From the completeness critic

| # | Sev | Finding |
|---|-----|---------|
| [#276](https://github.com/mikmorg/tapectl/issues/276) | high | (above) the retire floor — **no dimension was asked what the retire family does with `interrupted` writes**, and ADR-0012 names the defect verbatim |
| [#285](https://github.com/mikmorg/tapectl/issues/285) | medium | #263's dotfile-strictness ripple: one typo in one unit's dotfile fails `collection plan/status/sync/run` entirely, where ADR-0012 ruled the analogous `staging clean` case per-unit one day earlier. Plus: dotfile parse errors carry no filename |
| [#286](https://github.com/mikmorg/tapectl/issues/286) | low | Three new no-flag-reaches-it refusals were never classified against ADR-0008's tiers |
| [#287](https://github.com/mikmorg/tapectl/issues/287) | low | `LEGAL_VOLUME_STATUSES` hand-copies migration 017's CHECK set with nothing pinning them together (explicitly *not* a #96 violation) |

**Checked and clean, stated positively** (the critic's §2e): no pre-017 `'quarantined'`
reaches `volumes.status` by any route other than the #250 fix — `db import` is a byte-level
`rusqlite::backup` so it re-migrates, `ontape_catalog.rs` has no `volumes` table, and
`volume/rebuild.rs` and `cli/catalog.rs` already translate onto `observed_condition`. The
heir/on-tape byte surface is genuinely untouched: `volume/{format,manifest,restore_script,
envelope,rebuild}.rs` and `crypto/` are not in the diff at all. No dependency was added,
removed or bumped. `docs/man` was correctly regenerated except the one page downstream of
#281's stale source help.

## Critic's verdict

> **No — not trustworthy enough to precede a first production tape write.**

The ten dimensions covered the diff's centre well. The double-`seal()` path (#277) is by
itself disqualifying: its failure mode is rewriting a sealed cartridge, the exact path this
project got wrong once before (#208). Beyond fixing and re-reading that, the critic requires
#276 and #285 examined before the rehearsal.

## Notes on the review itself

- **Three of the eight confirmed findings are in code landed the same day** (#278 and #279
  in #274's fix, #282 in #252's). Reviewing a diff that includes the morning's own work is
  uncomfortable and is exactly why the rule says to review the whole range rather than only
  what feels new.
- **#278 corrects a claim the coordinator made to the CTO** while presenting #274's options:
  that scoping `--force` to one unit made it "no longer wider than the gate it overrides."
  True across units, false within the named one — the force branch drops the `EXISTS writes`
  guard entirely.
- The dimensions were drawn from this session's own misses rather than generic categories,
  per the 2026-09-17 lesson. The highest-yield brief was **vacuous assertions**, written
  after #275 was found by the residual sweep; it produced three findings, all three of which
  were refuted — and the refutations were correct. The lesson does not transfer as neatly as
  hoped: the recurring shape is real, but a brief aimed at it over-fires.
- Filed-against-found was reconciled: 8 confirmed + 4 critic items = 12 issues, no drops.
