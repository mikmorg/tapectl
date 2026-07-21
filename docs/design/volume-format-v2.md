# On-tape volume format — Layout Version 2 (normative)

This is the authoritative specification of the bytes tapectl writes to a sealed
cartridge. It **supersedes** `tapectl-design-v4_0.md` §2.6 (zone list), §8.1–§8.8
(file formats), and Appendix B/C/D (contract points, write sequence, append), and
is governed by **ADR-0007**. It pairs with `docs/design/layout-session.md`, which
owns the write-session **state machine, transitions, and operational sequence**;
this file owns the **static format, the isolation invariant, the integrity chain,
and the heir path**. Where a fact touches both, it is stated once here or there and
referenced, never restated (so the wording cannot drift). CONTEXT.md vocabulary
(Layout, Sealed, Heir Path, Escrow Recipient, Claim, Evidence) is used without
redefinition.

`layout_version = 2`. No production `layout_version = 1` tape is expected to exist
(the first production write postdates ADR-0005; only disposable mhvtl test tapes are
v1). A trivial v1 reader stub is retained so the format degrades gracefully if a v1
tape is ever found — but v2 is defined cleanly, not as a migration of v1.

---

## 1. Zone order (single partition, fixed 512 KB blocks)

A sealed v2 tape is a single partition of `M+1` tape files (each terminated by a
filemark), in this fixed order:

```
┌──── FRONT: plaintext navigation (all known pre-write, no key needed) ─────────┐
│ File 0  ID thunk        plaintext   identity: label, uuid, layout_version,    │
│                                     total_files, and the positions of the     │
│                                     front index (File 3) and the seal marker. │
│                                     "The full map is File 3."                  │
│ File 1  System guide    plaintext   heir manual (mt/dd/age/dar/sha256sum).    │
│ File 2  RESTORE.sh      plaintext   heir tool; --info reads File 3.           │
│ File 3  FRONT INDEX     plaintext   for EVERY file 0..M: position, type,      │
│                                     size_bytes; and sha256_encrypted for      │
│                                     every file EXCEPT File 3 and the seal      │
│                                     marker. ZERO content metadata.            │
├──── MIDDLE: encrypted decryption metadata (all known pre-write) ──────────────┤
│ File 4  Planning header enc(op+esc) planned packing list (optional; may fold  │
│                                     into the operator envelope).              │
│ File 5  Tenant envelope enc(t+op+esc) ┐ MANIFEST.toml (filenames,            │
│  ...    Tenant envelope enc(t+op+esc) │ sha256_plain, dar command, slice      │
│ File j   Operator env    enc(op+esc)  │ positions), RECOVERY.md, catalogs/.   │
│ File j+1 Operator backup enc(op+esc)  ┘ Tenant envelopes in position order.   │
├──── BULK: encrypted data (the mass of the tape) ──────────────────────────────┤
│ File j+2 Data slice     enc(t+op+esc) ┐ age(dar base.N.dar). One unit's       │
│  ...     Data slice     enc(t+op+esc) │ slices contiguous. The ONLY zone a    │
│ File M-1 Data slice     enc(t+op+esc) ┘ short/damaged tape can truncate.      │
├──── TAIL: the completeness assertion ─────────────────────────────────────────┤
│ File M   SEAL MARKER    plaintext   {layout_version, file_count = M+1,        │
│                                     sealed_at, front_index_sha256}. Its        │
│                                     presence = "everything before me is here."│
└────────────────────────────────────────────────────────────────────────────────┘
    esc = the permanent Escrow Recipient (ADR-0005) — in every encryption.
    op  = operator recipients.  t = the owning tenant's recipients.
```

**Rationale for the order** (full derivation in ADR-0007 / the research note
`docs/research/2026-07-21-ontape-format-and-write-design.md`):

- **Front index (File 3)** puts the whole navigation map at BOP. An heir reads it
  with `mt rewind; mt fsf 3; dd` — no spooling to end-of-data to learn what is on
  the tape or how large each file is. This replaces the v1 mid-tape "mini-index."
- **Envelopes before slices** (decision D2) means a truncated or tail-damaged tape
  loses only trailing *slices*; every envelope needed to navigate and decrypt what
  did land survives.
- **Seal marker last** is the only honest place for "complete" — completeness can
  be asserted only after the last content file exists. It is not optional: it is the
  price of front-loading the index (a front index alone would make a truncated tape
  look sealed).

## 2. Plaintext vs encrypted, and the isolation invariant

| Zone | On tape | Why |
|---|---|---|
| ID thunk, system guide, RESTORE.sh | plaintext | heir reads them with no key |
| **Front index (File 3)** | **plaintext** | navigation must work with no key; carries position/type/size and ciphertext hashes only |
| Planning header, tenant/operator envelopes, data slices | age-encrypted | *all* content metadata (filenames, content sizes, `sha256_plain`, dar catalogs) lives here and nowhere else |
| **Seal marker (File M)** | **plaintext** | structural completeness assertion; its `front_index_sha256` is a hash of a plaintext file |

**Isolation invariant (v2, normative).** No plaintext file on the tape may reveal:
filenames, tenant or unit names, plaintext-content sizes, plaintext-content hashes
(`sha256_plain`), or key fingerprints. Permitted in plaintext, because they are
structural and non-attributable to any tenant — no plaintext file carries a tenant
identity, so a `data_slice` or `tenant_envelope` entry is labeled only by kind, and
the value is computable by anyone holding the tape: per-file **on-tape byte sizes**
and per-file **ciphertext hashes** (`sha256_encrypted`).

**Envelope ordering.** Because the front index publishes a ciphertext hash *per
position*, envelope order must not become a side channel that re-attaches a hash to
a tenant. Non-attributability holds regardless (no tenant identity is in plaintext,
and each hash is over pseudorandom age output), but to also defeat positional
correlation the write path SHOULD order the tenant envelopes by a **deterministic
permutation seeded by `volume_uuid`** — deterministic so Layout construction stays
reproducible (layout-session.md), UUID-seeded so the order is not the raw
`tenant_id` sequence. *Status:* the shipped v1 write path orders by `tenant_id`; the
permutation lands with the v2 write flip (#22/#24). Until then the invariant above
still holds — the drift is positional-correlation hardening, not a plaintext leak.

This tightens — and deliberately relaxes one clause of — the v1 rule ("no
sizes/hashes in plaintext"), which the v1 format already violated by shipping
plaintext per-file sizes in the mini-index. The relaxation (ciphertext hashes in
plaintext) is **decision D4**, ratified 2026-07-21: a hash of age ciphertext is a
hash of pseudorandom AEAD output, reveals nothing about plaintext, is not
attributable to a tenant, and is recomputable by anyone with the tape — so storing
it leaks nothing an observer could not already derive. `sha256_plain` (the hash of
*decrypted* content) stays encrypted-only.

**The invariant is machine-checked.** `tests/mhvtl_e2e.rs::mhvtl_no_plaintext_tenant_metadata`
(mhvtl-gated) writes a volume with sentinel tenant/unit names and scans every
plaintext file for leaks. For v2 it MUST be updated to: (a) treat
`{0, 1, 2, 3, seal_marker}` as the plaintext positions; (b) scan plaintext files
for the sentinel **filenames and the expected `sha256_plain` hex** as well as
tenant/unit names, asserting all are absent; (c) permit `sha256_encrypted` and
on-tape sizes to appear; and (d) reflect the v2 order (envelopes precede slices).
This test is D4's real enforcement — the prose above is only its statement.

## 3. The front index (File 3)

Plaintext TOML. One entry per tape file, in position order:

- `position` — tape file number (0..M). Present for **every** file (navigation is
  total: the heir can jump straight to any file, including the seal marker).
- `type` — on-tape zone kind: `id_thunk`, `system_guide`, `restore_sh`,
  `front_index`, `planning_header`, `tenant_envelope`, `operator_envelope`,
  `operator_envelope_backup`, `data_slice`, `seal_marker`. **No** tenant/unit
  identity — a `data_slice` and a `tenant_envelope` entry are unlabeled beyond kind.
- `size_bytes` — the true (pre-block-padding) byte length, so the heir can
  `head -c $size_bytes` to trim the 512 KB block padding before decrypting. Present
  for every file **except File 3 itself** (its length is self-referential; the heir
  recovers File 3 by reading the whole filemark-bounded tape file and parsing the
  TOML, whose trailing zero-padding the parser ignores).
- `sha256_encrypted` — hex sha256 of the file's **on-tape bytes** (ciphertext for
  encrypted zones; the plaintext bytes for File 0/1/2). Present for every file
  **except File 3 itself** (self-reference) **and the seal marker** (not yet written
  when File 3 is emitted). For data slices this value is taken verbatim from
  `stage_slices.sha256_encrypted`, computed once at stage time — it is **not**
  recomputed while building the index (finding ③: no double read of the slice bulk).

Because File 3 carries content hashes, every envelope and slice must be fully
materialized and its `sha256_encrypted` known **before** File 3 is generated. This
is already true: write.rs encrypts envelopes up front (to fix their sizes) and slice
ciphertext hashes exist from stage time. Plaintext File 0/1/2 are small and hashed
directly when the index is built.

## 4. The seal marker (File M) and the integrity chain

Plaintext TOML, one tape file, written last: `{ layout_version, file_count = M+1,
sealed_at (RFC 3339), front_index_sha256 }`. Its **presence** is the assertion
"every file before me is present and this is a sealed volume"; its **absence** means
the tape is self-evidently unsealed (interrupted or aborted — see layout-session.md).

The seal marker is the **unhashed root of trust**. Nothing on the tape carries the
seal marker's own hash (it is last, tiny, and self-delimiting). From it, integrity is
a keyless chain, verifiable with only `dd` + `sha256sum`:

```
seal marker  ──front_index_sha256──►  File 3 (front index)
File 3       ──sha256_encrypted[i]──►  every content file i  (0,1,2, envelopes, slices)
```

Verifying the chain proves byte-integrity of the entire tape without any key:
hash File 3, compare to the seal marker's `front_index_sha256`; then for every other
file, hash it and compare to File 3's `sha256_encrypted[i]`. This is what "confirm"
verifies (§5) and what an heir can independently re-run years later for bit-rot
detection.

## 5. What "confirm" verifies (byte level)

The write-session **states and sequence** live in layout-session.md; this section
fixes only the *cryptographic content* of the confirm pass, because it is a property
of the format. Confirm is a **single forward pass from BOP** (no seek-back — the
index is at the front):

1. Read the seal marker (last file). Absent ⇒ not sealed; confirm fails.
2. Hash File 3; compare to the seal marker's `front_index_sha256`. Mismatch ⇒ the
   tape's two ends disagree ⇒ quarantine.
3. Parse File 3; diff its `{position, type, size_bytes}` entries against the Layout
   ⇒ **navigable** tier.
4. For every file except File 3 and the seal marker, read and hash it, compare to
   File 3's `sha256_encrypted[i]` ⇒ **integrity** tier.

The `verification_sessions` row records **which tier** was achieved (navigable vs
integrity), per ADR-0001 (recorded strength must match what ran). Drive-level
Logical Block Protection is not part of this chain — the Linux `st` data path does
not expose it (ADR-0007); if enabled out of band it is recorded only as
supplementary evidence, never as the basis of the seal.

## 6. Heir path — end to end with only `mt`, `dd`, `age`, `dar`, `sha256sum`

No database, no tapectl, no operator. This is the normative Heir Path (CONTEXT.md);
RESTORE.sh and the system guide implement it literally.

1. `mt -f /dev/nst0 rewind && dd if=/dev/nst0 bs=64k` → File 0: identity + "the map
   is File 3."
2. `mt -f /dev/nst0 fsf 3 && dd if=/dev/nst0 bs=512k` → **front index**: every
   file's position, type, size, and ciphertext hash.
3. *(completeness)* space to the last file and read the **seal marker**; check
   `file_count` and that `front_index_sha256` matches File 3's hash. Absent or
   mismatched ⇒ the tape is unsealed or damaged — proceed knowing some trailing
   *slices* may be missing (the front index still says exactly which).
4. For each envelope position: `dd` it out, `age -d -i KEY` — the one that decrypts
   is yours ⇒ `MANIFEST.toml` + `RECOVERY.md` + `catalogs/`.
5. For each slice position in your manifest: `dd bs=512k` it out; `sha256sum` vs the
   front index's `sha256_encrypted` (**keyless** integrity) and/or the envelope's
   value; `head -c size_bytes` to trim padding; `age -d -i KEY` → `base.N.dar`;
   optionally `sha256sum` vs the envelope's `sha256_plain`.
6. `dar -x base -R /dest` (dar reassembles `base.1.dar … base.N.dar`).

## 7. Block mode

Fixed-block mode, 512 KB (decision D7). Plan-first depends on deterministic block
padding — the Layout's `pad_to_blocks` and RESTORE.sh's trim-to-`size_bytes`
contract both require a fixed block size; variable-block mode (v4 design §2.29,
stale drift) buys nothing when every size is pre-planned. 512 KB is safe under both
the `st` ~2 MB memory ceiling and the drive's 8 MB encrypted-block ceiling; 1 MiB is
the throughput sweet spot but is a real-hardware tuning question, gated on the
LTO-6 validation session, not a format question.

## 8. What v2 removes

- The v1 **mid-tape mini-index** — its position/type/size data moves to File 3 and
  gains ciphertext hashes; its generator is repurposed, not deleted.
- The three-layer **end-of-tape salvage** (v4 §2.9 / Appendix C:
  overwrite-incomplete, sacrifice-last-slice). A real EOT is a **clean abort to an
  unsealed tape** (layout-session.md), never a salvaged partial — a partial was
  never a copy (ADR-0003/0004), and the salvage path is untestable on mhvtl (the
  2026-07-20 drill) and was a source of the audit's HIGH-severity defects. The
  `writes.eot_recovery` and `writes.sacrificed_slice_id` columns become inert schema
  reserve (documented dead in `docs/design-errata.md`; not dropped, per the repo's
  inert-reserve convention).
- The **manifest reserve**. Front metadata is a known up-front line item in the
  plan total, not an end-reservation, so `CapacityBudget.reserve_bytes` collapses to
  just the small ENOSPC buffer (a margin against MAM over-reporting remaining
  capacity). This is an active code change (config `manifest_reserve` →
  `enospc_buffer`), not doc-only.
- **Appendix D tape append** — already rejected by ADR-0003; reaffirmed.
