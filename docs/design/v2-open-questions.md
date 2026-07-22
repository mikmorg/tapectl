# v2 write-path — pre-implementation R&D, round 2: resolutions & robustness

Round 1 of this sheet compiled the open questions. This round **works** them:
every item that analysis could settle is now a **proposed resolution** (adopt
unless vetoed; each will be folded into `volume-format-v2.md` /
`layout-session.md` when ratified), and an adversarial robustness pass added the
hardening items in §3. Only the three §1 calls still need the operator. Facts
cited from the tree were verified 2026-07-22 (schema in
`src/db/migrations/001_initial.sql`; navigation in `src/volume/restore.rs`,
`src/volume/write.rs`).

---

## 0. Already settled — do NOT reopen

ADR-0007 + `volume-format-v2.md` fix: zone order (front index File 3 → planning
header → envelopes → slices → seal marker), single partition, fixed 512 KB
blocks, D4 (ciphertext hashes in plaintext) and D2 (envelopes first) ratified,
EOT salvage deleted → clean abort, confirm = forward readback from BOP, the
hash-chain rule (front index omits its own hash + the seal marker's; the seal
marker is the unhashed root), and the isolation invariant (no plaintext tenant
identity; sizes + ciphertext hashes permitted).

---

## 1. Operator ratification sheet

**Status 2026-07-22: ALL THREE RATIFIED — §1.1 embedded copy ("write both"),
§1.2 integrity default (`--quick` optional), §1.3 slice size = 10G. §1.1/§1.2
are folded into `volume-format-v2.md` §4/§5/§6 and the seal-marker generator;
§1.3's default lands in config on the regear branch (policy-chain resolution
rides #35). Discussion note for §1.3: dar's `-s` is an exact per-slice cut
(every non-final slice is exactly S; the last is the remainder), a per-unit
max — units smaller than S make one natural-size slice — and S ≡ 0 mod 512 KB
means full slices carry zero block padding. Nothing on this sheet awaits a
human until the #22 PR (§6 steps 2-4 remain).**

### 1.1 Seal marker: minimal vs **embedded full front-index copy**  ·  RATIFIED: EMBEDDED COPY
Round 1 leaned "minimal." The robustness pass flips the lean. Failure analysis:
- The front index is a **single point of failure at BOT** — and LTFS keeps index
  copies at *both* ends of the tape for exactly this reason (front copy + the
  authoritative end-of-data index, SNIA v2.5 §9.2).
- Embedding a full copy of the front index inside the seal marker (same TOML
  document: a `[seal]` section followed by the same `[[files]]` entries)
  **symmetrizes damage**: front damage → recover the whole map from the tail;
  tail damage → tape reads as unsealed (fail-safe) but stays fully navigable
  from the front. Either single-region loss is survivable.
- The tail copy can be strictly *more* complete: by seal time File 3's bytes are
  known, so the copy can carry File 3's own `size_bytes` + `sha256_encrypted`
  (only the seal marker's own entry stays hash-less — self-reference).
- Cost ≈ zero: the index is KB-scale on a 2.5 TB medium; generated once,
  serialized twice; parse complexity unchanged (same grammar, same shell
  parsing). No new file — the seal marker *is* the copy plus the seal fields.
**If ratified:** `generate_seal_marker` gains the `[[files]]` body;
`volume-format-v2.md` §4 gains the two-ended-redundancy rationale; the heir
degradation ladder (§3.4) gets its second rung.

### 1.2 Confirm default at seal: navigable vs **integrity**  ·  RATIFIED: INTEGRITY (opt-down `--quick`)
Round 1 leaned "navigable default, `--full` opt-in." Flipped by two arguments:
- **The asymmetry of when a bad copy is discovered.** At seal time the staged
  slices are still on disk — a failed confirm costs a fresh cartridge and hours.
  Discovered years later on a bit-rot pass, the source may be long gone. The
  system's entire promise is the copy; integrity-at-seal is the moment the
  promise is cheapest to keep.
- **Path errors are otherwise uncovered.** The drive's read-while-write verifies
  what the *drive received*, not what the host intended; with LBP unreachable
  through `st` (ADR-0007), the readback hash is the **only** control that spans
  host RAM → HBA → drive → medium. Skipping it at seal means no end-to-end check
  ever ran on the sealed artifact.
- Cost: one full forward read, ≈ write-duration again (~3.5 h for a full LTO-6).
  For an overnight personal archival workflow this is wall-clock, not labor.
**If ratified:** seal default = integrity pass; `--quick` opts down to navigable
and is recorded honestly. Schema note: **no new column needed** — the existing
`verification_sessions.verify_type CHECK('full','quick')` maps integrity→`full`,
navigable→`quick` (#23's honesty requirement lands on the existing column).

### 1.3 Slice-size default  ·  RATIFIED: **10G**, policy-resolved
The shipped default is `2400G` (`config.rs:155`) — one slice per tape: OOM under
the current buffering glue, a whole-tape blast radius on damage, and no
per-slice retry quantum. Sizing analysis for LTO-6 (2.5 TB):
- **25G** → ~100 slices/tape; blast radius 1% of tape; padding waste ~25 MB
  (negligible); re-read of one slice ≈ 2.6 min at 160 MB/s. dar reassembles
  hundreds of slices without complaint (research §F).
- 10G halves blast radius but 2.5× the file count; 50G the reverse. Any is sane;
  25G is the balanced default.
**If ratified:** change `default_slice_size()` to `"25G"` and resolve slice size
through the policy chain (dotfile > archive_set > default) with the streaming
work (#35). Existing test fixtures pin their own sizes — unaffected.

---

## 2. Resolved this round (proposed-normative; adopt unless vetoed)

### 2.1 Envelope-order permutation — the algorithm
Sort tenant envelopes by `SHA-256(volume_uuid_bytes ‖ 0x00 ‖ le64(tenant_id))`
(hex compare, stable). Plain SHA-256, not HMAC — there is no secret here
(`volume_uuid` is on the tape); the goal is only to decorrelate on-tape order
from the `tenant_id` sequence so per-position ciphertext hashes don't acquire a
stable cross-tape tenant mapping. Deterministic and RNG-free, so Layout
construction stays reproducible (`layout-session.md` determinism rule). Scope:
tenant envelopes only — the operator envelope + backup keep their fixed
positions at the end of the middle zone, and slices stay unit-contiguous (they
carry no tenant label; contiguity is what makes restore sequential).

### 2.2 ContentSource memory model — **materialize-to-staging**
Round 1 asked "buffer generated zones or stream them?" The answer is neither:
**write every generated zone to the session's staging directory at build time**,
then make every Layout entry — generated and staged alike — a disk path + size +
hash. Execute then streams *uniformly* from disk in position order; peak RAM is
block-sized for everything, with no special cases. Three robustness dividends:
1. **Frozen bytes fix resume.** The ID thunk embeds `created_at`; regenerating
   it on resume would produce different bytes than what File 0 already holds,
   breaking the cursor contract. Materialization generates each zone exactly
   once; resume re-reads the identical bytes. (This turns the determinism rule
   from "same inputs + same timestamp ⇒ same Layout" into something the
   filesystem enforces.)
2. **The plan is inspectable.** The complete volume-to-be exists on disk before
   contact — auditable (`volume plan --emit-dir` becomes trivial), diffable,
   and testable without a tape.
3. `ContentSource::Generated` becomes `Generated(PathBuf)` (or collapses into
   `Staged` with a provenance flag) — no `Vec<u8>` in the Layout, and the
   envelope-size question (round-1 §1.3) stops mattering: a 1.5 GB operator
   envelope streams like everything else.
Lifecycle: materialized zones live under the session staging dir and are
subject to the §3.5 GC guard.

### 2.3 ID-thunk v2 field set
File 0 = **identity + pointers, nothing per-file**:
- `[volume]`: `magic = "tapectl-volume-v2"` (keep the versioned-string pattern;
  readers match the `tapectl-volume` prefix and dispatch on `layout_version`),
  `label`, `uuid`, `layout_version = 2`, `tapectl_version`, `media_type`,
  capacities, `created_at`.
- `[layout]`: `front_index = 3`, `seal_marker = <total_files - 1>`,
  `total_files`. **Everything else** (`data_start/end`, `first_envelope`,
  `num_envelopes`, `mini_index`, operator positions) is deleted — those facts
  live only in File 3 now, as `[[files]]` entries typed by kind.
- The human-readable header keeps the "guide is File 1, map is File 3" text.
`volume_init`'s thunk remains a **provisional identity stamp** (positions
unknown at init); the write session rewrites File 0 from BOT with the real one —
#22 must not try to preserve init's File 0.

### 2.4 Tri-layer integrity (resolves a round-1 contradiction)
Round 1's guard rail ("validate must keep full-hashing") contradicted the
committed `layout-session.md` text ("validate trusts the stage hash, size-check
only"). Resolved — **both simple answers were wrong**; the robust model is three
layers, each covering a window the others can't (now committed in
`layout-session.md` validation point 2):
1. **validate** full-hashes staged slices from disk — pre-flight insurance; a
   disk read costs minutes and prevents burning a cartridge + ~7 h on a slice
   that rotted since stage time.
2. **execute** re-hashes inline on the *same* streaming read that feeds the
   tape (hash is free once streaming lands) and **cleanly aborts to unsealed**
   on mismatch — closes the validate→write TOCTOU window. A tape can't unwrite,
   but an unsealed abort beats sealing known-bad bytes.
3. **confirm** (#23) hashes the tape readback against the front index — the
   only end-to-end (host→medium) check, per §1.2.
Finding ③'s "no double read" survives *only* as: front-index generation reuses
`stage_slices.sha256_encrypted` verbatim (no third read).

### 2.5 Fail-safe reader precedence (malformed-metadata rules)
The spec says what a *valid* tape means; readers need rules for invalid ones.
Proposed normative precedence, biased so every failure degrades toward "less
trusted," never toward "assume sealed":
- **Seal marker absent or unparseable ⇒ unsealed.** A torn/partial final write
  must land the tape in the same state as never-sealed. (Parse failure is not
  an error condition — it *is* the unsealed signal.)
- **Seal marker valid but front index unparseable or hash-mismatched ⇒
  divergence ⇒ quarantine-grade.** The tape's two ends disagree; no claim from
  either is trusted until reconciled. (Heir analog: RESTORE.sh warns loudly and
  falls to the §3.4 ladder.)
- **Both valid but chain fails on some file ⇒ that file is bad, the map is
  good** — report per-file, don't discard the index.
- **Front-index self-consistency check** at every parse (tapectl and
  RESTORE.sh's `require_uint` alike): positions strictly increasing from 0,
  exactly one `front_index` entry at position 3, exactly one `seal_marker`
  entry at the last position, `file_count` in the seal matches. Cheap, and
  turns "subtly wrong map" into "loud parse failure."

### 2.6 Quarantine vs the heir (seal marker present, confirm failed)
If confirm fails after the seal marker is written, the catalog quarantines the
volume — but the *tape* still carries a seal marker, and the heir has no
catalog. Consequence (proposed for `volume-format-v2.md` §5 and the guide):
**RESTORE.sh and the guide must teach verifying the hash chain, never trusting
the marker's presence alone.** The heir then discovers exactly what confirm
discovered — the chain fails — and treats the tape as damaged rather than
authoritative. (A sealed-then-quarantined tape is physically immutable; the
quarantine lives in the catalog, the *evidence* of it is recomputable from the
tape by anyone.)

### 2.7 Reader-surface inventory (what the v2 flip actually touches)
Verified against the tree — smaller than round 1 feared:
- `restore.rs` and `volume_verify` navigate from **`write_positions` (DB)**,
  not the on-tape index — no hits for `first_envelope`/`mini_index` in the
  restore path. Operator restore/verify are **unaffected** by index relocation.
- On-tape index consumers today: **RESTORE.sh (heir) + the mhvtl e2e/leak
  tests** — plus the future #23 confirm and #27 contact check.
- `volume_identify` reads File 0 only (magic + label) — needs the v2 magic
  accepted alongside v1.
So the flip = write path + RESTORE.sh + tests + the two new session legs;
the DB-driven operator paths ride along untouched.

---

## 3. Robustness hardening (new items from the adversarial pass)

### 3.1 Front-index grammar: line-oriented forever
RESTORE.sh parses tape metadata with `grep`/`awk`/`require_uint`, not a TOML
library. Proposed formal constraint in `volume-format-v2.md` §3: the front
index and seal marker MUST remain **line-oriented** — one `key = value` per
line, no inline tables, no multi-line strings, `[[files]]` delimited exactly —
so shell parsing stays sound for decades. (The generators already emit this;
the constraint makes it a contract rather than an accident.)

### 3.2 Stale-tail unreachability (reused cartridges)
A reused cartridge may carry an old seal marker beyond the new session's last
filemark. Non-issue by tape physics — any write from BOT places EOD after the
new data, and forward operations past EOD error out — but the spec should say
so: **readers navigate only via the front index / `total_files` and never walk
past EOD**, so a stale seal marker from a previous life is unreachable through
the defined read paths. (mhvtl honors EOD; real-drive behavior is on the §5
hardware checklist for confirmation.)

### 3.3 Zero-strip recovery trick (degraded mode, documentation-only)
If the front index is lost (pre-embedded-copy tapes, or both-ends damage), an
heir can still recover: envelopes are early files (D2), and block padding can be
defeated **without knowing exact sizes** — strip all trailing zero bytes from a
`dd`-read file, then retry `age -d` appending one zero at a time (true
ciphertext rarely ends in many zeros; a handful of retries suffices). Decrypted
envelopes then supply exact sizes + hashes for every slice from their MANIFEST.
Pure documentation (system guide "if all else fails" section) — zero code, real
survivability.

### 3.4 The heir degradation ladder (make it explicit in the guide)
1. **Front index (File 3)** — normal path: positions, sizes, hashes.
2. **Seal-marker embedded copy** (§1.1, if ratified) — front-of-tape damage.
3. **Filemark walk + zero-strip** (§3.3) — both indexes lost: `mt fsf`
   sequentially, trial-decrypt early files with your key, envelopes yield the
   map.
Each rung is independently documented today except rung 2 (pending §1.1) and
rung 3 (pending §3.3's guide text). The ladder framing itself belongs in the
guide so an heir knows there *are* three tries before giving up.

### 3.5 Staging GC must be session-aware
The tri-layer model and materialize-to-staging both assume the session's inputs
(staged slices + frozen generated zones) exist until **sealed** — they are the
re-write source if confirm fails. `staging clean`'s current guards predate
sessions; **verify during #22** that GC refuses to delete anything referenced by
a `writes` row not in a terminal-success state, and add the test. (A GC that
reaps an interrupted session's slices silently converts "resumable" into
"aborted, data must be re-staged" — or worse if the source changed since.)

### 3.6 Escrow schema — concrete shape (grounds round-1 §2.3)
`encryption_keys` already has `key_type CHECK('primary','backup')`. SQLite can't
alter a CHECK without a table rebuild, so: **`ALTER TABLE encryption_keys ADD
COLUMN is_escrow INTEGER NOT NULL DEFAULT 0`** (no rebuild), leaving `key_type`
untouched. Migration 003 then contains exactly two operations:
1. `volumes` table rebuild (12-step) extending the status CHECK
   (`'blank','initialized','active','full','sealed','quarantined',…` — legacy
   `full` retained, read as sealed-equivalent per `layout-session.md`).
2. The `is_escrow` ADD COLUMN.
No `verification_sessions` change (§1.2 maps tiers onto `verify_type`).

---

## 4. Verification harness (design before #22 code)

### 4.1 MemStore synthetic-heir round-trip (the keyless unit test)
Build a Layout → run the session against `MemStore` → then, using **only the
recorded bytes**: parse File 3, walk the full hash chain (seal → front index →
every file), trim padding via `size_bytes`, verify positions/order/types, and
assert the §2.5 self-consistency rules. Extended this round to also pin:
- **permutation determinism** — same `volume_uuid` + tenants ⇒ identical order
  across two builds; different uuid ⇒ (almost surely) different order;
- **frozen-zone identity** — a simulated resume re-reads materialized zones
  byte-identical (no regeneration drift);
- **fail-safe truncation** — drop the final file from the MemStore record ⇒
  reader must report *unsealed*, never sealed.
This makes a wrong byte layout fail in `cargo test` with no tape anywhere.

### 4.2 Leak-scan v2 (D4's enforcement, strengthened)
`mhvtl_no_plaintext_tenant_metadata` updates: plaintext position set
`{0,1,2,3,seal}`; envelopes-before-slices in the range assertions; and the
stronger needle set — assert source **filenames** and expected **`sha256_plain`
hex digests** are absent from every plaintext file, while `sha256_encrypted`
and sizes are permitted. (Today it scans only tenant/unit names — too loose to
enforce D4.)

### 4.3 mhvtl scope honesty
mhvtl (loaded now, `/dev/nst0` live) verifies: round-trip, leak scan, chain
walk, resume. It **cannot** verify the EOT clean-abort (it silently corrupts
overflow instead of returning ENOSPC — 2026-07-20 drill); that leg is
real-hardware-only and stays on the §5 checklist, with the MemStore
fail-safe-truncation test (§4.1) as the software-side stand-in.

---

## 5. Deferred to LTO-6 hardware validation (unchanged from round 1, +1)

Block size 512 K vs 1 M throughput; LBP MODE SELECT acceptance + `st` readback;
MAM over-report bounds (sizes the ENOSPC buffer); real ENOSPC behavior (the
clean-abort trigger); v1-tape disposal confirmation; **and (new) EOD semantics
on the real drive** — confirm forward ops past EOD error rather than reading
stale data (§3.2's physics assumption).

---

## 6. Definition of ready — updated

1. Operator answers §1.1 / §1.2 / §1.3 (one word each; leans are argued above).
2. Fold §1 answers + §2 resolutions into `volume-format-v2.md` /
   `layout-session.md` (§2.4 is already committed); re-spec stale issues
   #22/#23/#25/#26/#27/#28 to v2 (round-1 §2.2 list, unchanged).
3. Write migration 003 per §3.6; land #68 escrow against it.
4. Stand up §4.1 + §4.2 harnesses (failing) — then build #22 against them.

Everything else on this sheet is done on paper. After step 1, nothing awaits a
human until the #22 PR review.
