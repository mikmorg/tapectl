# On-tape format v2: a front index, a trailing seal marker, and no end-of-tape salvage

The holistic R&D of 2026-07-21 (`docs/research/2026-07-21-ontape-format-and-write-design.md`)
reconsidered the whole on-tape format once we accepted that tapectl knows the complete
Layout before the first byte (plan-first) and that volumes are sealed-immutable
(ADR-0003). The v1 format imitated a streaming/append format — a *trailer* mini-index
written after the slices, and a three-layer end-of-tape salvage (§2.9 / Appendix C). Both
premises are wrong for a plan-first, write-once medium. We adopt **Layout v2** — a clean
redefinition, not a migration, on the design invariant that the first production tape
postdates ADR-0005 so only disposable mhvtl test tapes are `layout_version=1` (operator
one-line-confirmable; the format retains a trivial v1 reader stub so the claim degrades
gracefully if a v1 tape is ever found):

- **The navigation index moves to the front.** A plaintext **Front Index** (File 3)
  lists every file's position, on-tape type, and true byte size, plus — ratified below
  — the sha256 of each file's *ciphertext* for every file **except File 3 itself**
  (self-reference) **and the seal marker** (not yet written when File 3 is emitted).
  All of this is known before writing, so there is no reason to defer it to a trailer.
  The ID thunk's `[layout]` map subsumes into it. The seal marker's `front_index_sha256`
  is what covers File 3, giving a keyless integrity chain **seal marker → front index →
  every content file**, with the seal marker as the unhashed root of trust.
- **A minimal trailing SEAL MARKER is the completeness assertion.** Front-loading the
  index would erase the v1 signal "no trailer ⇒ unsealed," so the last file becomes a
  tiny plaintext marker (`file_count`, `sealed_at`, `front_index_sha256`). Its presence
  means "everything before me is present"; its absence means the tape is
  self-evidently unsealed. This is the honest, trailing place for "complete," and it
  binds the two ends of the tape. It is not optional — it is the price of the front
  index.
- **Envelopes are written before the data slices,** so a short or tail-damaged tape
  loses only trailing *slices*, never the decryption metadata for what landed.
- **End-of-tape salvage is deleted.** The pre-flight capacity gate (`Layout::validate`,
  ADR-0002 / #28) is the sole capacity defense; a real EOT — reachable only if MAM
  over-reports remaining capacity — is a **clean abort to an unsealed tape**, never a
  salvaged partial (a partial was never a copy — ADR-0003/0004). The
  `writes.eot_recovery` / `sacrificed_slice_id` machinery and `manifest_reserve` are
  removed; the front metadata is a known line-item in the plan total, so the capacity
  reserve collapses to just the small ENOSPC buffer.
- **Single partition, fixed 512 KB blocks.** LTO/Linux `st` partitioning exists only to
  keep a *mutable* index at BOP (LTFS); a sealed tape never rewrites, so partitioning
  buys nothing and costs guard-band capacity. Fixed blocks are required by the
  deterministic block-padding the Layout and RESTORE.sh depend on.
- **Confirm is a forward readback of ciphertext hashes.** After the seal marker, rewind
  and make one forward pass: read the front index and diff it against the Layout
  (navigable), hash each file and compare to the front index's `sha256_encrypted`
  (integrity), and read the seal marker. The verification_sessions row records which
  tier was achieved (ADR-0001: recorded strength matches what ran). Drive-level Logical
  Block Protection is **not** the basis of the claim: the Linux `st` driver does not
  expose it, and half-enabling it via `sg3_utils` breaks every write; if enabled out of
  band it is recorded only as supplementary evidence.
- **The write glue streams.** age's STREAM gives constant-memory encryption; the store's
  `execute`/`read` are defined as streaming operations so RAM tracks block size, not
  slice size.

**Ratified operator decisions (2026-07-21):**
- **Ciphertext hashes in the plaintext front index — YES.** This relaxes the stated
  "zero checksums in plaintext" rule, ratified because `sha256_encrypted` is a hash of
  age ciphertext (pseudorandom output, revealing nothing about content),
  non-attributable to a tenant (slices unlabeled, envelopes shuffled), and computable
  by anyone holding the tape — so it leaks nothing an observer couldn't derive, and the
  format already shipped plaintext per-file sizes. `sha256_plain` (hash of decrypted
  content) stays encrypted-only. The isolation invariant is retightened accordingly:
  no plaintext file reveals filenames, tenant/unit names, content sizes,
  plaintext-content hashes, or key fingerprints; on-tape byte sizes and ciphertext
  hashes are permitted as structural, non-attributable facts.
- **Envelopes before slices — YES.**

The authoritative format is `docs/design/volume-format-v2.md`; the session/write
mechanics are in `docs/design/layout-session.md`. This ADR refines ADR-0002 (confirm is
now a forward readback from BOP, not a seek-back-to-trailer) and ADR-0003 ("sealed" now
means "a seal marker binds a valid front index"); ADRs 0001, 0004, 0005, 0006 hold
unchanged in substance.

Considered and rejected: keeping the trailer index (its one edge — describing what
*actually* landed — is neutralized by plan-first + the confirm pass + the seal marker);
LTO partitioning (serves index mutation a sealed tape never needs); making confirm
depend on LBP (unreachable through the Linux data path); and salvaging partial tapes on
EOT (untestable on mhvtl, a source of the audit's HIGH defects, and never a copy anyway).
