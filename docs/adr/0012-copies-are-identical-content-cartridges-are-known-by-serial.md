# A copy is identical content; a cartridge is known by the serial its chip reports; the retire family's floor is absolute

The media-generation redesign (ADR-0010, ADR-0011) was followed on 2026-09-13 by an
adversarial review — 59 confirmed findings, `docs/audits/2026-09-13-post-redesign-review.md` —
and on 2026-09-14 by a grilling that put every open question to the CTO. Every ruling was
ratified before any production write, with the standing instruction to make
non-backward-compatible changes freely: nothing has been written to a production tape yet, so
this is the last moment a definition can change without a migration of trust. This ADR records
the rulings that are hard to reverse or would surprise a reader of the code. Where the review
found ADR-0010's or ADR-0011's own text wrong, the correction is made *in that ADR*, dated, so
a reader of either never has to know this one exists to learn its text was wrong.

**A copy is identical content, and a unit is as covered as its least-covered live version.**
`policy::coverage::copy_count_expr` counted distinct eligible volumes holding *any* current
snapshot of a unit, so v1 on one tape and v2 on another read as two copies, and at the shipped
`min_copies = 2` `unit mark-tape-only` would green-light deleting the only copy of v2 (#153).
The ruling: *if any data has changed — added or removed — it is a different thing and not a
copy; a copy is identical content.* A unit's copy count is therefore the **minimum over its
current snapshots** of that snapshot's own count, and every derivation (`audit`, the reports,
`mark-tape-only`, the retire family) reads that number. Reclaimable snapshots have been
released by the operator and are not in the minimum. That derivation is only correct if a
snapshot version *is* a content identity, and today it is not: `snapshot create` mints
`MAX(version) + 1` unconditionally, so re-snapshotting an unchanged directory produces a second
version holding identical bytes, which a per-version minimum would count separately — an
under-count, the safe direction, but still wrong. So the same ruling requires that **a version
is minted only when content changed**: `snapshot create` on a unit whose walk matches its latest
current snapshot — by the unit's own `checksum_mode`, the predicate that already decides
*Dirty* — reports the existing version and creates nothing. Two versions of a unit never hold
identical content, and a version number names content.

*Correction 2026-09-17 (issue #206): that last sentence claims more than the rule beside it
delivers, and the gap is now pinned by a test rather than left as an assumption.* The rule is
scoped to **the latest current snapshot**, and the code implements exactly that. It therefore
does not prevent a new version matching an OLDER, superseded one: revert a unit's bytes to a
previous version's and `snapshot create` mints a fresh version holding byte-identical content,
because the only row it compares against is the latest — which differs.
`staging::tests::reverting_to_a_superseded_versions_content_mints_an_identical_sibling`
demonstrates it. So "a version number names content" is true of the live sequence and not of
the full history.

The consequence is bounded and in the safe direction, which is why this is a correction to the
text rather than an emergency: `copy_count_expr`'s minimum is over **current** snapshots, so the
dead sibling is not credited and the fresh version reports the copies it actually has — zero.
The operator is told to write bytes that already sit on a cartridge under a superseded version,
which wastes a cartridge rather than losing data, and every deletion gate stays conservative
because a shortfall blocks. Whether the rule should widen to compare against every live version
— making the original sentence true — is a **decision the CTO has not been asked**, and is
parked on #206 rather than settled here.

**The retire family has an absolute floor, and the code had the tiers inverted.** ADR-0008
puts degraded-but-nonzero coverage in Tier 2 (evidence shown, prompt, `--force`/`--yes`
passes) and zero coverage in Tier 3 (refused, no flag). `volume retire`, `cartridge retire`
and `volume compact-finish` shipped the other way round: they prompted only at zero and let
`--force` through. The ruling restores ADR-0008 as written. Tier 3 fires when the thing being
retired is currently an *eligible* copy — sealed, unquarantined, unretired — of a current
snapshot and is the **last** one; it is defined by what the act removes, so retiring a
quarantined or unsealed volume, which counts as nothing, removes nothing and is not Tier 3.
Under the copy ruling "last one" is per version: a unit with v1 elsewhere and v2 only here has
zero remaining for v2, and that is exactly the case the floor exists for. The escapes are
commands, not flags: make another copy first (`volume read-slices` then `volume write` to
another cartridge, or re-stage from a source that still exists), or release the version with
`snapshot mark-reclaimable`, whose own preconditions apply and whose `--force` is the operator
saying in so many words that the version is given up. The hard case — a tape with read errors
holding the only copy — resolves through `volume verify`: a failed verify quarantines the
volume, it stops counting, and retiring it is then Tier 2 at most. Refusing to retire an
*unverified* sole copy is the point: the catalog saying "one copy, unverified" is true, and
"no copy" for data nobody has tried to read is not.

**A cartridge is known by the serial its chip reports; a barcode is a label.** The CTO's
requirement: a tape must be writable before it has a sticker, and a sticker must be
addable later; identity comes from the medium, with the barcode a fallback only where no
serial can be read. So `cartridges.serial_number` is the identity — written once, from MAM,
and never changed — and `barcode` is a mutable label that `cartridge relabel` changes.
Auto-registration sets the barcode *to* the serial as a placeholder. The tape records its own
identity at init: File 0's `[media]` table keeps `cartridge_serial` and gains
`cartridge_identity_source`, `"mam"` or `"operator"`. When no serial is readable at init the
operator **must** name the cartridge (`--cartridge <barcode>`) and the recorded identity is
that barcode with source `operator`; the former outcome — a volume written unbound with a
warning — no longer exists, because a volume that no cartridge claims is a copy the catalog
cannot place. Corroboration at every later contact follows from that: when the medium and
the bound row both have a serial they must agree, and a different serial is a different
cartridge, refused; a bound row that has no serial yet learns it at that contact, once
(`NULL` → value, never value → value) — unless another row already holds that serial, in
which case the loaded tape *is* that other cartridge and the write is refused; when the
medium reports no serial, `--cartridge` must name the bound row. A File 0 whose identity
source is `operator` is corroborated through the catalog binding, not by comparing its
recorded barcode to the row's — the row may since have been relabelled — and a
`catalog rebuild` from such a tape, with no catalog to consult, registers the cartridge under
that barcode and learns the serial if the drive reads one.

That closes the hole the review found in ADR-0010 (#155). "Binding records a displacement,
it never gates one" rests on two facts: File 0 has already decided consent, *and* the serial
proves the blank tape in the drive is the same cartridge whose volume is being displaced. With
no serial the second fact is missing — a blank tape plus `--cartridge B`, where B is bound to a
live volume, is either B erased or a different tape wearing B's sticker, and tapectl cannot
tell which. That case is refused: `volume retire` (or `cartridge mark-erased`) first, the
operator saying the bytes are gone. With a serial match the displacement is recorded exactly
as ADR-0010 says. Binding is also *ordered* after the tape-contact check, so a refused
`volume write` displaces nothing (#154), and a binding is permanent once its mount is closed.

**Amendment, 2026-09-16 — a typed serial and a chip-read serial are different
facts and live in different columns (#197); and "is this volume a write target"
stops being answered by a status (#199).** Two rulings from the same session,
both prompted by defects found while implementing the above.

*The serial.* `cartridge register --serial` lets an operator type a medium
serial by hand, for pre-registering a cartridge that has not been loaded yet. It
landed in `cartridges.serial_number` — the same column a real MAM read writes —
so the schema could not distinguish an operator's *assertion about* a chip from
what the chip *said*, and no code could either. A typo was then permanent: a MAM
read will not overwrite a recorded serial (write-once, above), `--cartridge` on
the real medium is refused as a different cartridge, and no command edits the
field. Every escape produced a duplicate row or a refusal.

Two shapes were put to the CTO — a `serial_source` provenance flag on the one
column, or a correction command — and both were declined in favour of a third:
**store them in different fields.** `serial_number` remains the identity and is
written *only* from a MAM read, which makes "never overwrite a chip-read serial"
structurally true rather than a rule code must remember; the operator's claim
lives in a new `operator_serial` and is the only one any operator command may
write. The naming follows File 0's existing `cartridge_identity_source` values,
`"mam"` and `"operator"`.

This dissolves the question that was actually asked, which is why it is the
better answer: there is no promote-or-refuse decision when a MAM read arrives,
because the two values never occupy the same slot — the read always writes its
own column and overwrites nothing. What follows is mechanical, not a further
trade-off: `lookup_cartridge` matches `serial_number` first, and falls back to
`operator_serial` only while `serial_number` is NULL, which is what makes
pre-registration work at all; a MAM read that confirms an assertion fills
`serial_number` and the assertion stands as the record of what was claimed; a
MAM read that *contradicts* a named row is refused naming both values — "this
row asserts X, the loaded medium reports Y" — and points at the correction,
exactly the shape `resolve_media` uses for a contradicted generation. Once
`serial_number` is set, `operator_serial` is never consulted again: the chip has
spoken. `cartridge info` shows both, labelled.

*The write target.* `policy::coverage::is_write_target` was `status ==
"initialized"`, and `catalog rebuild --from-volume` landing on a pre-existing
`initialized` row leaves the status alone (deliberately — overwriting an
operator's `quarantined` would destroy a fact a failed verify established). So a
rebuild could attach a whole tape's contents to a row that the catalog still
called a write target. Bytes were never at risk: the tape carries a seal marker,
`check_fresh_write_contact` refuses, and ADR-0003 means `--force` cannot
override it.

*Correction 2026-09-18 (issue #242): the parenthetical above is superseded by
this ADR's own later amendment, "the status column is the operator's; a
medium's condition is its own fact". A failed verify no longer establishes its
fact in `status` — `quarantined` ceased to be a legal `status` value at
migration 017 and lives in `volumes.observed_condition` — so there is no
operator `quarantined` for a rebuild to overwrite. The reasoning that follows
is unaffected: the gap this ruling closed was that a status alone could call a
row a write target while it already held a tape's contents, and the fix
(`has_completed_write`) is unchanged. `is_write_target` now takes the
condition as a second argument for a related but distinct reason, given in
that amendment.* What was lost is the *ordering* #161 exists to guarantee — the
refusal arrived from the tape side after `find_staged_data`, the
`mam_capacity_bytes` UPDATE and `TapeStore::open` had already run.

The ruling is **not** to have rebuild write a better status. It is to stop using
a status as the proxy: `is_write_target` additionally refuses a volume that
already has write or slice rows attached, because the question being asked is
"does this volume hold bytes we know about?" and status only approximates it.
The rejected alternative — rebuild marking such a row `sealed` — is narrower but
can mark a *blank, freshly initialised* tape sealed when two cartridges share a
label, and ADR-0003 then makes it unwritable without a real erase; a guard that
is right in the common case and destroys something in the uncommon one is the
trade this queue exists to refuse. The cost of the chosen option is that the
resume path must keep working, so the attached-rows test must distinguish the
resumable states (`planned`/`in_progress`/`interrupted`) from `completed` — a
coding risk, which tests pin and review catches, rather than an operator-facing
one.

**Cartridge capacities are decimal; data sizes are binary; the two are named apart.** The
generation table holds the marketed decimal figures (LTO-6 2.5 TB = 2 500 000 000 000), and a
`--capacity 2.5T` on `cartridge register`/`import` or a drive's `capacity_override` must mean
what the box says — parsing it as binary over-states the tape by up to 10 % (audit finding). Slice
sizes, thresholds and the ENOSPC buffer stay binary, as dar and the block layer count. One
parser cannot serve both, so there are two, and each flag's help says which it is.

**Unknown config keys are errors everywhere; closed-set values are validated at load.** A
misspelled key was a hard exit in `[[backends.lto]]` (ADR-0010), an advisory in `[defaults]`
and silent everywhere else, and `compression = "banana"` surfaced as a raw dar failure at
stage time. One rule: `deny_unknown_fields` on every section, and every closed set
(generation, compression, checksum mode, statuses on `--status` filters) rejected by name at
the boundary. Lenient warnings were considered and rejected — a key that silently reads as its
default is the failure that never gets noticed.

**`volume calibrate` is not built; capacity is settled by rehearsal.** The design's worked
example (§2.8, §5, #145) measured a per-drive overhead constant. The capacity model now has
the generation table, the cartridge row, MAM (informational, ADR-0010) and the ENOSPC buffer
under a conservative `usable_capacity_factor`, so a measured constant buys accuracy, not
safety. What *is* uncertain is MAM's maximum-capacity attribute: the real HP LTO-6 reports
`2499053`, which is either MiB or MB — a 10 % question — and the code assumes MiB. The ruling:
write an expendable cartridge to end-of-tape in the pre-production rehearsal on the real
drive, record where ENOSPC fell, and settle the unit from that measurement. A command that
computes it would answer a question the rehearsal answers for free.

**Rulings recorded as consequences** — mechanical follow-throughs, not trade-offs, listed so
nobody re-opens them:

- `volume write` and `resume` refuse a volume whose status is not one they can write; a
  sealed, retired or erased volume is not a write target.
- `catalog rebuild --from-volume` binds the cartridge it observed (from File 0's identity)
  and asserts `sealed` only after reading the seal marker; otherwise the row is
  present-but-unverified and ineligible (#158). Tape is authoritative (ADR-0001); a rebuild
  that asserts more than it read is a claim with no evidence.
- The flag is `--generation` on every command; `--media` and `--media-type` are gone, with
  no aliases, because CONTEXT.md names the concept *Generation* and a flag spelled otherwise
  teaches the wrong word.
- Move events carry location *names* on both sides, never an id on one and a name on the other.
- `logging.level` and `logging.format` are wired; `labels.format`, `packing.strategy`,
  `packing.fill_threshold` and `defaults.hash` are deleted rather than left inert.
- `cartridge edit --generation` corrects a wrong generation (Tier 1: it is a fact
  correction, and the wrong-medium check at the next init still applies).
- `collection run` budgets each batch against the destination volume's `capacity_bytes`,
  never the drive's generation — ADR-0010's rule, applied to the one caller that skipped it.
- `cartridge unretire` (Tier 1) reverses `cartridge retire` and restores the volumes' prior
  statuses; `cartridge mark-erased` remains the statement that the bytes are gone. ADR-0011's
  "mark-erased is the only way back" is corrected there.
- `import` requires `--generation`; the `LTO-6` default that wrote unvalidated rows is gone.
- Not built before first production use, and deliberately: `config set/add/remove` (#143)
  and `--policy-aware` packing (#144).

Considered and rejected: **counting copies per unit with a uniqueness rule on `current`**
(demote the previous version at seal — simpler, but it makes archiving v2 silently release
v1's coverage, which CONTEXT.md's *Current* entry exists to forbid); **a content fingerprint
column instead of the minting rule** (pools identical-content versions correctly, but leaves
`snapshot create` producing versions that mean nothing, and the fingerprint is a second
definition of "unchanged" next to *Dirty*); **a mutable serial with an audit trail** (lets a
mis-read chip be corrected, but an identity that can be edited is a label, and the tape's own
record would then disagree with the row forever); **`--force` passing the retire floor with a
scarier prompt** (ADR-0008 already ruled that a flag cannot resolve an incoherence); and **one
size parser with a unit flag** (would make `2.5T` mean two things depending on a switch the
operator has to remember).

---

**Amendment, 2026-09-17 — a failed verify quarantines only when it proves the MEDIUM is
bad (issue #234).** The Tier-3 paragraph above says "a failed verify quarantines the
volume", and §"the write target" below it relies on the same claim ("overwriting an
operator's `quarantined` would destroy a fact a failed verify established"). **No verify
path ever wrote that status.** Every production writer of `quarantined` was on the
write/resume confirm path (`volume::session`), reached through `log_quarantine`;
`volume_verify` recorded a `verification_sessions` row and left `volumes.status`
untouched. So the escape hatch this ADR names from an absolute refusal did not exist: an
operator with a tape they had *proved* unreadable, holding the only copy, met the Tier-3
refusal — which names no flag, by construction — and had no command that changed
anything.

The blanket wording was also too strong, and that is the substance of this amendment
rather than a wording fix. A verify can fail for reasons that say nothing about the
medium: a dirty drive, a wrong block size, a transient SCSI error, a tape not loaded.
`layout-session.md` states the hazard in terms — quarantining a good tape is "silent
corruption, not a loud failure" — and because `quarantined` is precisely what makes a
volume stop counting as a copy, a false quarantine silently takes real coverage to zero.
A bad drive could condemn a library one cartridge at a time.

**Ruled:** a failed verify quarantines **only** on a failure that proves the medium is
bad — **a checksum mismatch: bytes that came back and hashed wrong.** Drive and
transport errors are reported and do **not** quarantine; they leave the volume exactly as
it was, because "we could not read it today" is not "the bytes are gone".

*Correction 2026-09-17, same day, during implementation (issue #239).* This sentence
originally read "a checksum mismatch, **or an unreadable block at a position the layout
says carries data**" — which contradicts the sentence immediately after it, because an
unreadable block at a data position is precisely how a drive or transport error
manifests. The two clauses gave opposite answers for the same event, and the worker
implementing the rule found the contradiction rather than silently picking a side.

The resolution keeps the sentence that expresses the DECISION's purpose. `chain_walk`
produces the same `ContentHashMismatch` from three different situations: a genuine hash
disagreement, a raw `Err` from the read, and a short read. Only the first is evidence
about the medium. The other two are now `ContentUnreadable`, which does not quarantine —
matching the reasoning already written on the `FrontIndexUnreadable` arm, which rules on
the identical event at a different tape position: neither a read error nor a short read
distinguishes a bad tape from a dirty drive, a wrong block size, or a transient SCSI
error. The distinction must be visible in what `volume verify` prints and in its
`--json`, so an operator can tell "this tape is bad" from "this drive could not read
it".

This requires classifying verify failures, which `VerifyReport` does not currently
support; that classification is the work, not the status write. Considered and rejected:
**an explicit `volume quarantine` command** (smaller and it cannot misfire, but it makes
the operator assert a fact the tool just measured, and leaves the catalog unable to
record "the operator tried to read this and it failed" — the very fact §"the write
target" says must not be overwritten); and **correcting this ADR to name a different
resolution** (there is none — `read-slices` needs a readable tape, and
`snapshot mark-reclaimable --force` gives up the *version*, a different act with
different consequences).

**Amendment, 2026-09-17 — `collection run` takes ONE destination label (issue #229).**
§11 of `v2-open-questions.md` describes batch execution as "session on cartridge A →
seal + confirm → session on cartridge B → seal + confirm → release staging. Stage once,
write N times." It settles the shape and is silent on how cartridge B reaches the drive.
`execute_batch` looped `volume_write` over every `--label` against a single `device`,
with no prompt, eject, pause or changer call anywhere in the tree — so copy 2 always met
the wrong-tape refusal with copy 1's cartridge still loaded, and the documented primary
route to `min_copies = 2` could never complete. It had never run: the lifecycle suite
passes exactly one `--label`.

**Ruled:** `collection run` accepts one destination label and **refuses more than one**,
naming the per-copy `tapectl volume write <label>` invocations to run between cartridge
swaps.

*Correction, same day, before implementation.* This paragraph originally justified the
ruling by asserting that "staging survives the first copy — `staging clean`'s non-force
guard retains a set while any `writes` row is non-`completed`". **That is false**, and the
worker sent to implement the ruling stopped and refused to write a recipe resting on it,
which is the correct outcome. The guard is real, but `execute_batch` calls
`clean_staging(force = false)` **unconditionally** immediately after the copy loop, and
after a single copy each stage set has exactly one `writes` row, `completed` — so the
guard passes *vacuously*, staging is released, and a later
`tapectl volume write <label2>` finds nothing: `find_staged_data` selects only
`status = 'staged'`. A green regression test,
`default_guard_cleans_when_the_only_planned_copy_completed`, pins exactly that.

The multi-label loop only ever *appeared* safe because copy 2 failed at the wrong-tape
refusal and returned before `clean_staging` was reached.

So the ruling stands and gains a second half, which is a **pre-existing defect the ruling
merely exposes**: releasing staging after one copy is already wrong today whenever a unit
resolves `min_copies > 1`, on the single-label path that works. `execute_batch` must
release staging only when the copies actually written satisfy each unit's resolved
`min_copies`, and otherwise retain it and say what remains. Consulting policy from the
collection layer is established, not new: `collection/status.rs` already compares
`policy::coverage::copy_count_expr` against `resolved.min_copies`.

§11's stage-once/write-N-times is preserved by that retention, not by the guard.

The reasoning is that tapectl drives no changer. `CONTEXT.md` says the changer is "an
autoloader **or a human hand** otherwise", and a human hand needs a point at which to
act. Refusing is honest about that; it also keeps every write path scriptable and
non-interactive. Considered and rejected: **pausing between copies** for an operator to
swap cartridges — closest to §11's literal wording, but a blocking prompt inside a
multi-hour batch is a new failure mode of its own, and it would make `collection run`
the only write path that cannot be scripted.

`destination_budget`'s minimum-across-destinations rule is unaffected either way and
stays: the batch must fit the smallest planned destination.

---

## Amendment, 2026-09-17 — the status column is the operator's; a medium's condition is its own fact

**Raised by:** issue #242, flagged twice (by #234's worker and again by #239's) and left
undecided by both, correctly, because it is a policy question rather than an
implementation detail.

**The behaviour.** `volume_verify`'s quarantine `UPDATE` is unconditional on the
volume's current status, matching the three existing writers in `src/volume/session.rs`
(`:744`, `:760`, `:1050`). Verifying a `retired` volume therefore overwrites the
operator's deliberate terminal status with `quarantined`.

Verifying a retired tape is a **reasonable thing to do** — before physically disposing
of a cartridge, before trusting a warehouse deposit recorded against it, or simply to
learn whether a condemned tape is still readable. Doing so today silently rewrites *why*
the volume is out of service: from "the operator retired this" to "a verify found the
medium bad". Those are different facts with different remedies. And under ADR-0011 a
`retired` volume is unfit to write but **not unreadable**, so the catalog losing that
distinction is exactly the conflation ADR-0011 was written to prevent.

**Ruled: `volumes.status` is operator-owned, and what a verify observes about the medium
moves to a column of its own.** The two facts stop competing for one slot instead of
being ordered against each other.

This is deliberately *not* the narrower fix that was recommended (verify declines to
overwrite a terminal status, recording the evidence only in the `events` row and the
`verification_sessions` row). That fix keeps both facts but leaves them asymmetric: the
medium's condition is recoverable only by reading the audit trail, so every surface that
wants to ask "is this tape known bad?" must either re-derive it from events or go
without. The same question is already asked in several places, and a fact that several
callers need is a column, not an inference.

It is the same shape as this ADR's 2026-09-16 ruling on #197, and worth naming as a
recurring pattern: when a question presents as *"which of these two values wins this
field"*, check first whether they can simply **stop sharing the field**. Two values that
never occupy one slot need no precedence rule, and the fork dissolves rather than being
decided.

### What follows from it, and what must not be got wrong

1. **`policy::coverage` owns the consequence.** `eligible` is `status = 'sealed'` today,
   and quarantine removes a copy *by moving the status out of `sealed`*. Once the
   condition is a separate column that mechanism is gone, so `eligible` must consult
   both — a sealed volume whose medium is known bad is **not** a copy. This is the
   load-bearing change: miss it and every quarantine silently stops reducing the copy
   count, which is the coverage-misstatement class #153 was. `coverage.rs` is the
   declared sole owner of every `volumes.status` predicate (#96); the condition
   predicate belongs there too, beside the others, and is never inlined.
2. **`is_write_target` likewise** (#199's ruling): `retired`/`erased` stay status facts,
   a bad medium becomes a condition fact, and both must still refuse a write.
3. **The three `session.rs` writers move with the fourth.** They quarantine on the same
   evidence and are unconditional for the same reason; leaving them writing `status`
   while verify writes the new column would reintroduce the divergence this fixes. One
   writer, one meaning — the rule that produced `render_displacement` (#235).
4. **`quarantined` as a `status` value is retired, not repurposed.** Existing rows
   carrying it must migrate to the new column with their status restored to what it was
   before quarantine where the `events` row records it, and to `sealed` where it does
   not — a quarantine only ever fired on a volume that was otherwise in service.
5. **Only a medium-proving failure sets the condition** — unchanged from this ADR's
   2026-09-17 ruling on #234 (`MismatchKind::proves_medium_bad`). Drive and transport
   errors still set nothing.

**Severity is unchanged by this ruling: low.** No data is at risk today and every fact is
already recorded somewhere. The cost of the larger fix is a migration, a predicate
change and a display change across every volume-listing surface; it is taken because the
model is right, not because the symptom is urgent.

---

## Amendment, 2026-09-17 — `staging clean` refuses to release a stage set below its policy

**Raised by:** issue #244, found while writing #226's `collection-second-copy` scenario.

#238 established that `clean_staging`'s non-force guard passes **vacuously** after a
single copy: it requires a `writes` row to exist and none to be non-`completed`, but a
row is created per volume only when that copy is attempted, so one completed copy
satisfies it. #238 fixed the one caller it named, `collection::batch::execute_batch`,
which now gates release on `under_copied_units`. **The CLI caller was not touched**, and
`src/cli/staging.rs` passes `*force` straight through with no policy lookup at all.

The defect is *more* reachable after #238, not less, because #238's fix prints the
recipe an operator is meant to follow — "swap in the next cartridge and run
`tapectl volume write <label>`" — and #229's refusal names the same one. That defines a
window in which staged bytes are deliberately being kept alive, and `tapectl staging
clean` is routine housekeeping run without ceremony. Inside that window it destroys
exactly the material the printed recipe depends on. Recovery is a full re-stage from
source, and impossible for a `mark-tape-only` unit whose source is gone.

**Ruled: `staging clean` names the under-copied units and refuses, unless `--force`.**

`--force` already exists on this command and is already documented as the override for a
stage set stuck behind a copy that will never complete — which is precisely this
situation when the operator means it. No new flag, no new vocabulary.

The alternative considered was warn-and-proceed, consistent with ADR-0004's advisory
posture. Rejected here because ADR-0004 governs *policy compliance* reporting, not the
deletion of the only cheap route to a copy the operator's own policy requires: a warning
printed after the unlink is not advice, it is a receipt.

Two constraints on the implementation:

- The count routes through `policy::coverage::copy_count_expr` against
  `policy::resolve(...).min_copies` — the same derivation `execute_batch` uses, never a
  second one (#96).
- `clean_staging` itself stays policy-free and its existing test
  (`default_guard_cleans_when_the_only_planned_copy_completed`) stays correct: it
  describes that function in isolation, and the gate belongs in the callers, which is
  what #238 already concluded. Pushing the check down would make `execute_batch`'s gate
  redundant and put policy inside a function deliberately without it.

---

## Amendment, 2026-09-18 — a readback that did not succeed is not a verdict about the medium

**Raised by:** issues #260, #267 and #268, found by the second pre-production adversarial
review (`docs/audits/2026-09-18-preproduction-review-2.md`) and its write-path impact
triage. Three rulings from one question, taken together because each is unsafe without
the others.

**The behaviour.** This ADR's 2026-09-17 amendment (point 5) says only a medium-proving
failure sets the condition, and drive and transport errors set nothing. That was applied
to `volume verify` and not to `SealedPending::confirm`, which is `let passed =
evidence.mismatches.is_empty()` — it never consults `MismatchKind::proves_medium_bad`.
Because `Tier::default()` is `Tier::Integrity`, confirm reads back the **whole cartridge**:
on a 2.5 TB LTO-6 that is an hours-long window in which one transient SCSI error condemns
a tape that is physically fine.

Two consequences make it worse than a misclassification. Confirm's failure arm marks its
`writes` rows `aborted`, and `retire_impacts` only considers `completed` rows — so a
confirm-failed volume yields **no impacts at all** and `volume retire` proceeds with no
ADR-0008 Tier-3 refusal, leaving the zero-coverage floor blind to exactly the tape it
exists to protect. And `scripts/first-run.sh`'s ADR-0003 branch then walks the operator
through retiring the volume and bulk-erasing the cartridge.

### Ruled: confirm gains a third outcome, `Inconclusive`

`ConfirmOutcome` has exactly two variants, which is *why* the code had to choose between
two wrong answers. Neither was acceptable: sealing on an unproven readback makes the
catalog assert a copy it did not verify, which is the claim ADR-0001 forbids; quarantining
on a transport error condemns a sound cartridge.

So the fork is dissolved rather than decided, the same move as #197's two columns and
#242's two fields. An `Inconclusive` confirm:

- does **not** seal — the readback did not succeed, so the durability claim is unproven;
- does **not** write `observed_condition` — nothing was learned about the medium;
- leaves the session **resumable and re-confirmable**, because confirm is idempotent and
  the tape is physically unchanged by a failed read.

`proves_medium_bad` decides which arm: true ⇒ `Quarantined`, false ⇒ `Inconclusive`,
no mismatches ⇒ `Sealed`. That makes the four quarantine writers genuinely identical in
meaning, which is what point 3 of the 2026-09-17 amendment asked for and did not get.

`docs/design/layout-session.md`'s confirm section is normative for the state machine and
moves with this.

### Ruled: a passing full verify clears the condition

Nothing in the tree ever set `observed_condition` back to `'ok'` — twelve write sites, all
writing `'quarantined'` — and `catalog rebuild` only records the mismatch. Every
quarantine was therefore permanent regardless of cause, which is what made a false one
expensive enough to argue about.

A `volume verify --full` that completes with **zero** mismatches sets the condition back
to `'ok'` and records the transition in `events`. The reasoning is definitional: the
column holds what tapectl *observed* about the medium, so a later and better observation
is precisely the thing entitled to update it. No new command and no new consent surface —
the operator instinct this serves (clean the drive, verify again) is one the tool should
simply reward.

This is deliberately **not** an operator override. A `clear-condition` command was
considered and rejected for now: the condition is evidence, and the way to replace
evidence is to gather better evidence. If a tape genuinely cannot be re-verified — drive
gone, cartridge offsite — the remedy is the existing one, copy the data elsewhere and
retire the volume.

*Partial verifies do not clear it.* Only a full readback can license the claim that the
medium is sound, so a tier below `Integrity` leaves the condition exactly as it found it.

### Correction to this ADR's own point 4 (2026-09-17 amendment), issue #256

Point 4 says an existing `quarantined` row migrates with its status restored from the
`events` row, "and to `sealed` where it does not — a quarantine only ever fired on a
volume that was otherwise in service."

**That premise is false for three of the four writers.** The `session.rs` writers fire
mid-write, on a volume that never reached `seal`; there is no sealed state to restore to.
Migration 017 therefore restores `'initialized'` in the no-events case and argues it in
its header. **The code is right and this ADR was wrong**; the text is corrected here
rather than the migration being changed to match it.

The verify-path half of point 4 stands unaltered: where an `events` row records the prior
status, that value is restored — constrained to the statuses the post-017 CHECK still
admits, because the trail was written under the old rules and can legally contain
`'quarantined'` itself.

---

## Amendment, 2026-09-21 — two corrections from implementing the two amendments above

**Raised by:** issues #262 and #260/#267, both found by implementing this ADR's own
2026-09-17 and 2026-09-18 amendments and discovering each ruling's mechanism did not
survive contact with the code.

### `staging clean` retains per unit; it does not refuse per command (#262)

The 2026-09-17 amendment ruled, verbatim: *"`staging clean` names the under-copied units
and refuses, unless `--force`."* **That sentence is corrected here: the command names the
under-copied units, RETAINS their staged data, releases everything else, and succeeds.**

The ruling's purpose is untouched and is in fact better served — the at-risk unit's bytes
are still kept, which is the whole of #244. What changes is the blast radius, and the
blast radius was a defect the ruling did not foresee:

- The refusal was whole-command, so one unit stuck below `min_copies` — a lost second
  cartridge, a failed drive — blocked the release of **every fully covered unit**
  indefinitely. Staging fills, and `stage create` starts failing on ENOSPC. The archive
  stops making progress because one unit is stuck.
- `--force`, the only documented escape, is **strictly less safe than the gate it
  bypasses**. Its candidate set is `status IN ('staged','failed')` with no `writes`-row
  condition at all, against the non-force branch's requirement of a completed write — so
  it also discards staged ciphertext for sets never written to **any** tape. The
  operator's only way past a gate protecting one unit was an act endangering all of them.
  That extra reach looks like an accident rather than a decision, and is worth revisiting
  on its own.
- The repo's own harness is primary-source evidence: `scripts/lifecycle-suite.sh` carries
  two `staging clean --force` call sites whose comments exist solely to explain why the
  bare command now refuses.

The two implementation constraints of the original ruling are unchanged and still bind:
the count routes through `policy::coverage::copy_count_expr` against
`policy::resolve(...).min_copies`, never a second derivation (#96); and `clean_staging`
stays policy-free, with the decision in the callers. `CleanScope` (#248) is a
**selection** parameter and does not breach that line.

**A related contradiction, fixed with it:** `clean_staging`'s doc says `'failed'` stage
sets are swept unconditionally because they carry no copy requirement — and its SQL did
not. A `'failed'` set was collateral to a refusal it could never be the cause of. The
scope now applies to the `'staged'` branch only; `'failed'` sweeps regardless, including
under an empty unit slice.

### `volume resume` re-confirms a tape that is already sealed (#260, #267)

The 2026-09-18 amendment ruled that `confirm` gains an `Inconclusive` outcome which
leaves the session "resumable and re-confirmable". **The mechanism is ruled here, because
the obvious one does not work.**

`InterruptedSession::rehydrate` selects only `writes.status = 'interrupted'`, so that is
the only state `volume resume` can reach. But `confirm` runs *after* `seal`, so the tape
is physically sealed at that moment: a resume re-enters `check_tape_contact`, meets
`AlreadySealed`, and quarantines — reproducing the exact false quarantine the
`Inconclusive` ruling exists to prevent, one command later.

**Ruled: `resume` re-confirms.** When `check_tape_contact` returns `AlreadySealed` **and**
the File 0 identity matches **and** the recorded seal position agrees with the session's
own layout, the tape is exactly where this session left it. Resume then skips the write
phase and re-enters `confirm`, which is already idempotent. No migration, no new
`writes.status` value, no new command — and `volume resume` comes to mean what its name
says for every interruption rather than only mid-write ones.

**The cost is named rather than discovered later.** This makes `session.rs`'s
`AlreadySealed` arm conditional, and that arm is the ADR-0003 guard whose failure mode is
rewriting a sealed cartridge. This project has got that exact path wrong once: the v2
regear shipped a resume that would have rewritten a sealed tape, it passed
fmt/clippy/test, and it was caught only by reading a flagged residual. So the three
conditions above are conjunctive and none may be inferred: identity from File 0, seal
position from the session's own layout, and the tape's own seal pointer — never the
caller's guess, which is why #208 hoisted that probe in the first place. Anything short
of all three keeps today's behaviour.

The alternatives considered and rejected: a new `writes.status = 'unconfirmed'` with a
migration and a dedicated command (safest for the ADR-0003 arm, but adds a schema value
and a command on the eve of first production use); and making `volume verify --full` the
promotion path (attractive now that verify already clears the condition, but it gives a
command operators run casually the power to mutate `volumes.status`).

### The seal is RECORDED, not inferred — correcting the amendment above (2026-09-21, #277)

The amendment immediately above ruled that `resume` re-confirms an already-sealed tape,
on three conjunctive conditions, and explicitly **rejected** "a new `writes.status`
value with a migration" as adding schema on the eve of first production use.

**That rejection was wrong, and the third pre-production review found why.** The ruling's
outcome was right; its mechanism has a hole that its own cost paragraph did not
anticipate.

**The hole.** All three conditions route through `seal_marker_parses_at`, which returns
`false` when the read **errors**, not only when the position holds no marker. That
conflation is deliberate and is correct for a fresh write — a blank tape's positions do
not read, and that must mean "not sealed". On resume it is fatal, because the one
`MismatchKind` that produces `Inconclusive` in the first place is `SealUnreadable`. So
the seal file this session wrote is exactly the file the resume cannot read: no
`AlreadySealed`, identity matches, `ContactOutcome::Matches`, the empty arm, and
execution falls through to `reposition_for_resume` and `seal()` — **a write to a
physically sealed cartridge, with ADR-0003 bypassed and `resume_reconfirm_eligible` never
consulted, since it is called only inside the `AlreadySealed` arm.** If the seal was
unreadable because of a real flaw, the overwrite can fail partway and convert a
drive-side read failure into medium-side destruction: the inversion of this ADR's own
premise that a readback which did not succeed is not a verdict about the medium.

**Why no cleverer probe fixes it.** Two states are indistinguishable to the tape *and*,
today, to the catalog:

- (a) execute finished, `seal()` never ran (interrupted between the two) — resume **must** seal;
- (b) execute finished, `seal()` ran, confirm was `Inconclusive` — resume must **never** seal.

Both leave `writes.status = 'interrupted'`, `volumes.status = 'initialized'`, every
`write_positions` row `'written'`, and a seal position that does not read. Any rule
derived from the tape alone must get one of the two wrong.

**Ruled: record the seal.** At the moment `seal()` returns `Ok`, the fact that this
session sealed becomes durable state (migration **018**), and `resume` reads it instead
of inferring it:

- recorded-sealed → re-enter `confirm`; **never** `reposition_for_resume`, never `seal()`
- not recorded-sealed → the seal is still owed; resume seals as it does today

The three conjunctive conditions above remain as **defence in depth** for the case where
the tape *can* be read — they are not replaced, and none may be dropped. What changes is
that an unreadable seal no longer silently means "unsealed".

**This is the #242 pattern for the third time** (after #197 and #242 itself): when the
question is "how do we infer X", check first whether X can simply be recorded. A fact
several callers need is a column, not an inference. The rejected alternative was costed
as "a schema value on the eve of first production use"; the actual cost of not having it
is a rewritten sealed cartridge, which is the outcome ADR-0003 exists to forbid.

**A second caller is already waiting for it.** Issue #276 — `retire_impacts` filters
`writes.status = 'completed'`, so a confirm-failed volume yields no impacts and the
Tier-3 zero-copy floor never fires, letting `volume retire` and `cartridge mark-erased`
destroy the only copy of a sealed, restorable tape — is the same blind spot about the
same state. It is to be fixed against this recorded fact, not against a second inference.

*Fact correction, 2026-09-22 (#289).* The sentence above is right about the outcome and
wrong about the mechanism for one of the two commands it names. `volume retire` was
blinded by the `writes.status = 'completed'` filter, and widening it (#276) restored its
floor. `cartridge mark-erased` was not blinded by that filter or any other: it never
called `retire_impacts` or `refuse_last_eligible_copy` at all, so it was blind to every
state equally and no change to the filter could have reached it. It was a fourth command
with the inverted shape ADR-0012 named three of, and #147's sweep did not visit it. The
floor now runs there too, unconditionally and before the Tier-2 consent — a structural
no-op on the ordinary retire → bulk-erase → mark-erased lifecycle, because `volume
retire` has already moved the volume to `retired` by then and `holds_sealed_bytes`
excludes it. The ruling is unchanged; only the account of how the door was open.

**Constraint on the implementation.** The new state must not make a sealed-but-unconfirmed
volume look like a completed one to anything that counts copies: `policy::coverage`
remains the sole owner of that question (#96), and a volume whose confirm has not passed
is not yet a copy. The change is about what `resume` and the retire family may *do*, not
about what counts as coverage.

## Amendment, 2026-09-22 — an unparseable unit dotfile refuses that unit, not the collection

`#263` added `#[serde(deny_unknown_fields)]` to the `.tapectl-unit.toml` types and turned a
previously infallible per-unit read into a fallible one on a whole-collection path
(`collection plan|status|sync|run` → `collection::fingerprint` → `staging::exclude`, with
`?` all the way up). Its stated intent — "stops the write" — did not distinguish
**per-unit** from **per-collection**, and the implementation silently chose per-collection:
one typo in one unit's dotfile anywhere under a collection root archives **zero** of N
units where N-1 were archivable before.

**Ruled: per-unit, and the command exits non-zero.** The offending unit is named, with its
file path, and is REFUSED — never archived. Every other unit proceeds. The command's exit
status is non-zero.

This is the same reasoning as the 2026-09-21 `staging clean` ruling (#262), applied to the
same shape: a fault attributable to one unit must not hold every healthy unit hostage, and
the archive must keep making progress. A parse failure in unit A is evidence about unit A's
file and about nothing else — unlike a capacity or policy fact, it does not generalise.

Two constraints make the narrower blast radius safe rather than merely smaller, and both
bind:

- **The offending unit is refused, not best-efforted.** A dotfile that will not parse may
  carry an `[excludes]` section that cannot be honoured, and archiving that unit while
  silently ignoring its exclusion list would put excluded data on a tape permanently. The
  skip is a refusal of that unit, never an archive-with-defaults.
- **The exit status is non-zero.** `collection run` is the unattended path. An exit-0
  "success" that quietly omits one unit forever is the failure mode this ruling would
  otherwise create — worse than the whole-collection refusal it replaces, because nobody
  is watching. The non-zero exit is what makes per-unit safe for a cron.

Separately, and needing no ruling: **every dotfile parse error names the file path.**
`unit::dotfile`'s `toml::from_str(...).map_err(|e| TapectlError::Other(e.to_string()))`
drops it, and no layer above adds it, so the same typo is reported three different ways —
`report dirty` names the unit, `policy::resolve` names unit and path, and `collection plan`
names neither. Attach the path at the read site.

## Amendment, 2026-09-22 — the three no-flag refusals of this diff, tiered (#286)

The third pre-production review's completeness critic noted that this diff added three
refusals no flag can override, and that none had been checked against ADR-0008. Audited;
the result is that **no code change was needed**, and the reasoning is recorded here so the
question is not re-opened at 2am by someone hitting one of them.

**ADR-0008 is about DESTRUCTIVE consent, and none of these three destroys anything.** Its
opening sentence enumerates the operations it governs — `volume retire`, `unit
mark-tape-only`, `compact-finish`, `db import`, `cartridge mark-erased`, `snapshot delete`
— all of which give something up. These three are *precondition failures*: they stop an
operation before it starts, and nothing is lost when they fire. So the tier ladder does not
strictly apply to them.

What does apply is ADR-0008's own test for where the ladder tops out:

> *"The distinction Tier 2/Tier 3 draws is between risk and incoherence. `--force` should
> mean 'I accept a degraded but non-zero safety margin.' ... The escape hatch for Tier 3 is
> not a flag but a single command, which resolves the incoherence instead of waiving it."*

By that test all three are incoherences with a resolving action, not waivable risks:

1. **`resolve_lto_backend`'s ambiguity gate (#272).** Two drives are configured and the
   operator has not said which to write to. There is no safety margin to accept here —
   there is no fact the operator could assert that makes "either drive" coherent. The
   escape is an argument that supplies the missing fact, and the message already names it:
   *"multiple LTO backends configured (...); pass `--device` to select one"*. The
   collision branch likewise names `tapectl config check`. **Correct as it stands.**

2. **`Config::load_tolerating_backend_ambiguity` (#261) is not a refusal at all**, and the
   review's premise is wrong on this one. It is a lenient *loader* — it TOLERATES a
   collision that `Config::load` refuses, precisely so read paths (`volume identify`,
   `verify`, `restore`, `catalog rebuild`) keep working during disaster recovery. It
   widens what is accepted rather than narrowing it. Nothing to tier. The refusal that
   sits downstream of it is item 1, which is tiered above.

3. **Dotfile strictness (#263).** A `.tapectl-unit.toml` that does not parse cannot yield
   the exclusion list that decides what reaches the tape. Archiving the unit anyway would
   not be a thinner safety margin — it is writing data to write-once media under
   exclusions nobody can read, which is the same shape as marking a Never Archived unit
   tape-only. The escape is fixing the file, which resolves the incoherence. Issue #285
   made the error name the file path and the offending key, which is what makes that
   escape actionable. **Correct as it stands.**

**On "a Tier 3 message must say why no flag exists":** applied where the absence is
surprising, not everywhere. It earns its place in the retire family, where an operator
reasonably expects `--force` to work and ADR-0008 deliberately withholds it. It would be
noise on a TOML parse error, where naming the file and the bad key already tells the reader
exactly what to do and no one expects a flag to parse a broken file for them.

## Amendment, 2026-09-23 — `volume resume` adopts an aborted session whose seal is recorded and whose medium a clean verify has since cleared (#280)

*Ruled by the CTO on 2026-09-22 (Option 2 of the three in #280); recorded here before
implementation, as #280's acceptance requires.*

**The state.** `SealedPending::confirm`'s **Quarantined** arm leaves a volume with its seal
marker physically on the tape (`volumes.sealed_at` set, migration 018),
`observed_condition = 'quarantined'`, `volumes.status = 'initialized'` and every `writes`
row `'aborted'`. The 2026-09-18 amendment's "a passing full verify clears the condition"
then applies. After a clean full verify the bytes are provably good on sealed media, but
**no command could make the volume count as a copy**: `rehydrate` selects only
`interrupted` rows, so `volume resume` could not reach it, and nothing else writes
`status = 'sealed'`. The `Inconclusive` arm never had this problem, because it leaves its
rows `interrupted` and the 2026-09-21 amendment's re-confirm handles it.

**Ruled: `volume resume` adopts such a session and re-confirms it**, on all of the
following, conjunctively:

1. the seal is **recorded**: `volumes.sealed_at` is set (the 2026-09-21 amendment: the
   seal is recorded, not inferred);
2. the medium has been **cleared by evidence**: `observed_condition = 'ok'`, **and** a
   passing full verify of this volume is recorded **after** the session was aborted. The
   condition alone is not enough, because `volume abort` also leaves `aborted` rows, and
   an operator's deliberate abort is not undone by a condition that was never
   quarantined. The evidence must be a recorded row, not an inference from the current
   state;
3. the existing defence in depth still holds where the tape can be read: the File 0
   identity matches and the seal position agrees with the session's own layout.

When all three hold, resume skips the write phase entirely, **never** calls
`reposition_for_resume` or `seal()`, and re-enters `confirm`, the same path the
2026-09-21 amendment gives a recorded-sealed `interrupted` session. Only a passing
`confirm` writes `status = 'sealed'`. **That invariant is what this option was chosen to
protect:** a medium observation (the verify) never moves an operator column. It only
removes the obstacle to the one command that may.

If any condition fails, resume refuses, naming the first unmet condition and, where one
exists, the command that resolves it (for example, `volume verify <label>`, which is full
by default).

**Rejected.** Option 1, where a clean verify advances `status` itself: it lets a medium
observation move an operator column, the separation the 2026-09-17 amendment drew.
Option 3, accepting that the volume never counts: it leaves the catalog permanently
disagreeing with a tape somebody has successfully read back in full.

**The cost, accepted explicitly.** `aborted` stops meaning "never resumable" and comes to
mean "not resumable until the seal is recorded and a later clean full verify has cleared
the medium". Every operator-facing sentence that says otherwise must change **in the same
commit**, or two texts will contradict each other (#292 was filed for exactly that
shape). In particular this covers `volume abort`'s consent block and the clean-clear
message #280's first half landed (`c525498`), which deliberately named no remedy for this
state. It now names `volume resume`, with a test pairing that presence against the
resume actually succeeding.

**Constraint, unchanged from the 2026-09-21 amendment:** until `confirm` passes, the
volume is not a copy. `policy::coverage` remains the sole owner of that answer (#96).
