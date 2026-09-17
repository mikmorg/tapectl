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
override it. What was lost is the *ordering* #161 exists to guarantee — the
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
bad — a checksum mismatch, or an unreadable block at a position the layout says carries
data. Drive and transport errors are reported and do **not** quarantine; they leave the
volume exactly as it was, because "we could not read it today" is not "the bytes are
gone". The distinction must be visible in what `volume verify` prints and in its
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
swaps. §11's stage-once/write-N-times is preserved, because staging survives the first
copy — `staging clean`'s non-force guard retains a set while any `writes` row is
non-`completed` — so the per-copy invocations consume the same staged bytes and do not
re-stage.

The reasoning is that tapectl drives no changer. `CONTEXT.md` says the changer is "an
autoloader **or a human hand** otherwise", and a human hand needs a point at which to
act. Refusing is honest about that; it also keeps every write path scriptable and
non-interactive. Considered and rejected: **pausing between copies** for an operator to
swap cartridges — closest to §11's literal wording, but a blocking prompt inside a
multi-hour batch is a new failure mode of its own, and it would make `collection run`
the only write path that cannot be scripted.

`destination_budget`'s minimum-across-destinations rule is unaffected either way and
stays: the batch must fit the smallest planned destination.
