# Review: what building `catalog rebuild` exposed

**Date:** 2026-09-11
**Trigger:** the CTO, on reading #136/#137: "I think it points to design gaps that need addressing."
**Method:** single reviewer, empirical. Every claim below was measured on the dev VM against the `db-loss` and `first-year` lifecycle fixtures (mhvtl `/dev/nst1`, real HP LTO-6 `/dev/nst0`), and read against the normative set (`volume-format-v2.md`, `tapectl-design-v4_0.md` §2.20–§2.23, ADR-0004, ADR-0005, ADR-0009). Where a cause is a hypothesis rather than a fact it is labelled one.
**Scope:** the findings that fell out of #136 (`catalog rebuild --from-volume`) and #137 (a rebuilt catalog cannot prove escrow coverage). Not a re-audit of the restore path; the prior audits stand.

---

## Executive summary

Building the first command that reconstructs the *operator's* view of the archive from tape — rather than the heir's — put the tape's self-description under a load it had never carried. It held for data restore, which is what it was designed for. It did not hold for the catalog, and looking at why turned up one live defect that has nothing to do with rebuild at all.

> **The units with the least redundancy are the ones `audit` never looks at.** A unit whose source has been deleted (`tape_only`) drops out of every per-unit audit check the moment it is marked. The knob named `min_copies_for_tape_only` is applied to every unit except tape-only ones. Live since Milestone 6.

Around that, four design gaps, none of them bugs in the usual sense — places where two parts of the design were each right on their own terms and nobody had put them in the same room:

1. `catalog.db` (#83) has no agreed purpose; it is insufficient for both readers that exist.
2. The disaster-recovery procedure exists only as a composition of parts nobody has composed.
3. A post-disaster `init` mints a *new* escrow identity, and nothing tells the operator to import the original.
4. "Self-describing" is used as a principle and defined nowhere.

Totals: **1 defect**, **4 design gaps**, **2 test-design notes**. Tags: **settle** = correcting something unambiguously wrong, do without a decision; **defer** = a fork a reasonable CTO could take either way.

---

## Finding 1 — `audit` never checks tape-only units — **defect, settle** → [#138](https://github.com/mikmorg/tapectl/issues/138)

**Measured.** Same three units, same single tape, `min_copies_for_tape_only = 2`:

```
units.status = 'active'     audit: 3 violations (copy_count ×3), exit 2
units.status = 'tape_only'  audit: 0 violations,                 exit 1
```

The status flip is the only change. `report tape-only` lists them (`big: 1 copies, 1 locations`) with no policy comparison and no exit code, and the #70 timer runs `audit` + `report verify-status`, not `report tape-only`.

**Normative text.** §2.20: `audit` "compares reality against policy… checks: copy count, location presence, verification age, encryption compliance, dirty status." No scoping to `active` is stated. §2.22: `mark-tape-only` enforces `min_copies_for_tape_only` — **at marking time**. Nothing in the design re-checks it.

**Cause.** `cli::audit` selects `list_units(conn, None, Some("active"))` and runs every check over that set (since `b544705`). *Hypothesis:* the dirty scan walks each unit's source directory, which a tape-only unit no longer has, and one scope was applied to all checks instead of that one.

**Why it matters.** `tape_only` asserts "the source is deleted; the tape is all there is" — the moment copy count matters most. A volume retired, quarantined, lost or `missing` afterwards takes the unit below policy and the scheduled audit never says so. The existing guards are real but not continuous: `volume retire` shows dropped counts (§2.23), `compact-finish` refuses when live slices lack copies.

**How it was found.** `catalog rebuild` first inserted units as `tape_only`, and the rebuilt catalog reported 0 violations where the original reported 3. That was fixed in `60dca6f` (rebuilt units are `active`). The fix was correct and the measurement was the important part: it was the first time anyone compared `audit` on a tape-only unit against `audit` on the same unit active.

**Recommendation.** Scope **per check**, not per command:

| check | `active` | `tape_only` | `missing` | `retired` |
|---|---|---|---|---|
| dirty scan (walks the source) | yes | no | no | no |
| copy count / locations / warehouse | yes | **yes** | yes | no |
| encryption compliance | yes | **yes** | yes | no |
| verification age | yes | **yes** | yes | no |

Regression test is the measurement above, verbatim. The knob's name is a separate minor smell — #129 already settled that it *is* the general copy requirement — and is not worth a rename on its own.

---

## Finding 2 — `catalog.db` has no agreed purpose — **design gap, defer**

**What each source says it is.** `src/db/catalog_snapshot.rs` (#83): a "portable `catalog.db` subset for the operator envelope", scoped to "this volume's write only". `volume-format-v2.md` §L89: the catalog "rides each volume encrypted (the operator envelope's catalog snapshot, #83) and survives the machine via the Heir Kit (#69)". ADR-0009: the heir kit bundles the **full** `tapectl.db` because the filtered one "carries no `locations` and no `cartridges` — an heir holding it could enumerate what the archive contains but could not learn which cartridge to fetch."

**What #136 found.** It carries `units`, `snapshots`, `stage_sets`, `stage_slices`, `files` — and no `tenants` (a FK target), no `sha256_plain`, no `key_fingerprints`. So a rebuild takes the slice map from the manifest instead, takes tenant ownership by decrypting every tenant envelope, and cannot take the escrow receipt from anywhere.

**The gap.** Two readers exist and it is insufficient for both. ADR-0009 already declined it for the heir. #136 has now declined it for the rebuild, keeping only `files` and `source_path` from it. It is the second-largest member of the operator envelope and its job is "the per-file index".

**Options.**

- **(a) Make it a complete rebuild source.** Add `tenants` (name only — the row carries no key) and `stage_sets.key_fingerprints`. Both are inside an age envelope encrypted to operator + escrow; the isolation invariant governs plaintext files only. `catalog rebuild` then reads one file instead of three kinds, and **#137 is closed for every tape written afterwards** — the receipt is on the tape.
- **(b) Declare it a convenience** — "an heir with the operator key can run SQL against the file index" — and stop expecting more of it. Cheaper, honest, leaves #137 as is.

**Recommendation.** (a). It is the cheapest coherent answer to #137 and it turns a file with an ambiguous job into one with a clear one. It changes envelope bytes on future tapes, which is why it is a defer.

---

## Finding 3 — The DR procedure is a composition nobody has composed — **design gap, settle (docs) + one check**

**The parts.** ADR-0009: the kit's `catalog.db.age` is the **full `tapectl.db` at generation time**, and `escrow_kit_stale` warns when volumes have been sealed since. #136: `catalog rebuild` reconstructs any sealed tape. #83: the operator envelope carries `catalog.db`.

**The composition that follows, and that no document states:** *restore the kit's database, then `catalog rebuild` each tape sealed since the kit.* The kit gives every row up to kit time — with `key_fingerprints` intact. Rebuild covers exactly the set `escrow_kit_stale` already names.

**What that does to #137.** With a kit restored, `escrow: NO` appears only on post-kit tapes — the ones the advisory check was already nagging about — and the mitigation is the one ADR-0009 already prescribes: regenerate the kit. That narrows #137 from "every rebuilt unit, forever" to "tapes newer than your last kit, until you regenerate it". It does not close it: with no kit ever generated, rebuild is the only path and every tape shows `NO`.

**Recommendation.** Write the composition into the operator guide's Disaster Recovery section as the primary operator path, with `catalog rebuild` alone as the no-kit fallback. Docs only — settle. And then Finding 4.

---

## Finding 4 — A post-disaster `init` mints a *new* escrow identity — **design gap, settle (docs + fixture), one verification owed**

**Fact.** The 2026-09-10 batch (Q3) has `init` create the escrow identity. `escrow::gap` tests membership of the **current** escrow public key in `key_fingerprints`:

```rust
Ok(keys) if keys.iter().any(|k| k == escrow) => None,
Ok(_) => Some("encrypted without the current escrow recipient"),
```

**Consequence.** After a disaster, `tapectl init` on a fresh machine creates escrow identity B. Every tape was encrypted to identity A. Any row whose receipt survives — from a restored kit, from `db import` of a backup — now reads `NO: encrypted without the current escrow recipient`, and `volume write` refuses to re-copy without `--allow-missing-escrow`. Not because coverage is unknown, but because the catalog is comparing against the wrong key.

**Nothing says to import the original.** The operator guide's DR section mentions escrow only to describe #137. `key import --escrow` exists (`cli::key::import_escrow_key`). The kit's `COVER.txt` carries the escrow *secret* in Bech32; whether it also prints the public half in a form `key import --escrow` accepts is **unverified** — if it does not, the operator has to derive it.

**This also explains the `-` in the fixture.** `db-loss` arm (d) runs `init --no-escrow` and never imports one, so `escrow::marker` had no current key to compare against. The fixture was not testing the scenario; it was avoiding it.

**Recommendation.**

1. DR section: step one after `init` is `key import --escrow <original public key>` — before any restore, rebuild, or write. Settle.
2. Arm (d): `init` normally, import the original escrow public key from the source home, and assert the rebuilt units' escrow marker reads what the CTO decides in #137 — not `-`. Settle.
3. Verify the kit prints the public key in importable form; if not, that is a small #69 follow-up.

---

## Finding 5 — "Self-describing" is a principle without a definition — **design gap, settle (docs)**

`CLAUDE.md`: "Volumes are self-describing — full restore possible without the database or tapectl." The only other mention is a `CONTEXT.md` aside. True for **data**: the heir path is proven on real hardware, twice this run. Not true for the operator's catalog: the tape carries no `key_fingerprints`, no `tenants` table, no `locations`/`cartridges`, no verification history, no policy state. #136 is the first time anyone tried to reconstruct that view from tape, which is how the boundary became visible.

**Recommendation.** One normative paragraph in `volume-format-v2.md` stating what a sealed volume carries — data, its restore metadata, and (if Finding 2a lands) the escrow receipt — and what it does not: location, verification, policy, and anything the catalog knows only because the operator did something. Settle.

---

## Two test-design notes — **settle**

**T1. The lifecycle fixture hides escrow behaviour on its new-home arms.** `db-loss` (a)–(d) all run `init --no-escrow`, so the arms that exist to test disaster recovery are the arms in which escrow is systematically absent. `first-year` carries escrow (`fy-*.escrow` passed on the real drive), so this is arm-specific, and Finding 4's fixture change closes it.

**T2. Measure the command the output tells the operator to run next.** The three #136 follow-ups (`60dca6f`) — a silently quiet `audit`, an invented `backend_name`, a never-run `volume verify` — were invisible to twelve passing tests and a green gate, and all three fell out of running `audit` and `volume verify` once against the artifact the command had just produced. This is now in the unattended-run skill's "done" requirements; recording it here because it is the mechanism by which Finding 1 was found.

---

## #137, restated

> **Correction (later the same day):** `escrow_coverage` was already an `audit`
> **warning** (exit 1), not a violation, before any of this. The review above
> and the issue thread called it an "un-clearable violation"; that overstated
> it. What the third state buys is *distinguishability* — wording, `?` in
> `catalog locate`, and what the write gate says — not a severity change.


The original framing said the consequence "only bites once you're actually using escrow". That was wrong: `init` creates escrow by default and `volume write` refuses to write without it (#115). On a normal install a rebuilt unit shows `NO`, and the cost is paid twice — a permanent audit finding, and `--allow-missing-escrow` on the exact operation wanted after a disaster.

Findings 3 and 4 change its shape. With a kit restored and the original escrow key imported, the noise is confined to post-kit tapes, and regenerating the kit is already the prescribed remedy. With Finding 2a, it stops arising on new tapes at all.

**Revised recommendation:** 2a (put the receipt on the tape) for the future; for existing tapes, either accept the narrowed noise or add the third state ("unknown — rebuilt from tape"), which the `catalog_rebuild` event already makes possible. The choice between those two is the one genuinely open decision.

---

## What to do with this

| # | Item | Tag | Where |
|---|---|---|---|
| 1 | `audit` scoping per check | settle, **fix** | #138 — **landed `ba8a4a1`** |
| 2 | `catalog.db` becomes a complete rebuild source | defer | grilled 2026-09-11, ratified (a) — **landed `4292cf9`** |
| 3 | DR section written as kit + rebuild | settle, docs | **landed `103ba99`** |
| 4 | import the original escrow key; fix arm (d); verify the kit prints it | settle | **landed `103ba99` + `a7f2921`**; kit text already says `key import --escrow`; **#139 (`init --escrow-public-key`) landed `b10b089`** |
| 5 | define "self-describing" | settle, docs | **landed `103ba99`** |
| — | #137 decision: accept narrowed noise, or third state | defer | grilled, ratified: third state + attestation — **landed `d5c9638`** (third state) |

Two items want the CTO (2 and the #137 choice); they are the same decision seen from two sides and should be grilled together. Everything else is a settle and can proceed on ratification.
