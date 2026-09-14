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
