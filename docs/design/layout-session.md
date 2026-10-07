# Layout & Write Session — the shared skeleton for epic #20

This note is the one design artifact the epic's children (#21–#28, #71) build
against. ADR-0002 gives the philosophy; this gives the shape. It is normative
for the state machine, the persistence mapping, and the transition rules;
everything finer-grained (types, method names) is decided in the children. Its
companion `docs/design/volume-format-v2.md` (governed by ADR-0007) is normative
for the **on-tape byte format** — zone order, the front index and seal marker,
the isolation invariant, and the integrity chain; this file references it rather
than restating it, so the two cannot drift. CONTEXT.md vocabulary (Layout, Write
Session, Sealed, Unsealed, Contact, Evidence, Quarantine, Store) is used without
redefinition.

## The Layout (child #21)

A Layout is a **value**: the complete ordered enumeration of every file a
volume will hold, constructed and validated before the first byte is written,
and the single source from which *all* on-tape metadata is generated.

Each entry carries: position index; zone kind (`id_thunk`, `system_guide`,
`restore_sh`, `front_index`, `slice{stage_slice_id}`,
`tenant_envelope{tenant_id}`, `operator_envelope`, `operator_envelope_backup`,
`seal_marker` — the v1 planning header is folded into the operator envelope as
`PLAN.toml`, `volume-format-v2.md` §8); byte size (exact for staged slices,
computed at generation for generated zones); sha256 of the on-tape bytes (from
`stage_slices` for slices; computed for generated zones); and content source (a
staged or materialized file path — generated zones are frozen to the session
staging dir at build time, v2-open-questions.md §2.2). Order is fixed (see `volume-format-v2.md` §1): the front index
is position 3, envelopes precede the data slices, and the seal marker is last.
The Layout also carries volume identity (label, uuid) and the capacity budget
(nominal capacity + ENOSPC buffer per §2.8 once #28 lands — the v1 "manifest
reserve" is gone: front metadata is a known up-front line item in the plan
total, not an end-reservation).

**Validation predicate** (all must hold before a session may start):
1. Capacity: Σ block-padded sizes + ENOSPC buffer ≤ available (per-store
   capacity oracle; tape = §2.8 formula via #28, until then nominal-capacity
   config). This pre-flight gate is the sole capacity defense (ADR-0007).
2. Every staged slice exists on disk at its recorded encrypted size, and has a
   recorded sha256. Under `--prewrite-hash` it must also match that sha256 (a
   full streamed hash); by default it is not read (ADR-0012, "Amendment,
   2026-09-30 (later)" — at production size the read costs hours per copy, and
   the inline re-hash below already keeps a rotted slice off a sealed tape).
   This is the first layer of the **tri-layer integrity model**:
   *validate* checks from disk (with the full hash, insurance against wasting a
   3.5 h tape write on a stale/rotted slice), *execute* re-hashes inline on the
   same streaming read that feeds the tape and cleanly aborts to unsealed on
   mismatch (closes the validate→write TOCTOU window at zero extra I/O), and
   *confirm* (#23) hashes the tape readback against the front index. Each layer
   catches a window the others cannot. Finding ③'s "no double read" applies to
   front-index generation only — it reuses the stage-time `sha256_encrypted`
   verbatim rather than re-reading slices a third time.

   **Execute's data path (since 1.0.6, #390).** The staged-file read, the
   inline hash and the tape write run on three threads (`src/pipeline.rs`):
   a reader fills tape-block buffers from the staged file, a hasher hashes
   them in order and passes them on, and the store writes them on the
   session's own thread. They are joined by a queue bounded at 256 MiB of
   blocks (`pipeline::QUEUE_BYTES`), so the drive streams at the slowest
   stage's rate instead of waiting out each disk read and each hash in turn
   (L6-0001's serial write spent ~49 min of 5 h 22 m in tape write calls).
   The L2 verdict is now reached **before the file's last block is handed to
   the store**: the hasher holds the last block until the hash is finished,
   and on a mismatch drops it, so the store's write ends in an error with
   neither that block nor the filemark after it written (by tapectl — the st
   driver writes a filemark when the device closes after a write, the same
   tape shape an ENOSPC abort leaves). Before 1.0.6 the hash was compared
   after the whole file and its filemark were on tape; either way no seal
   follows. An empty file has no block to hold back and is judged after, as
   before. Everything else is unchanged: the abort reason, the `writes`
   rows (`aborted`), the slice's cursor row (`failed`, with the hash its
   bytes actually have), and a store error (ENOSPC, a drive fault, a staged
   file that fails or ends short) wins over the hash, with its own message,
   exactly as when the stages ran in turn. The threads are scoped and joined
   before the file's verdict is recorded, on every path; Ctrl-C is still
   honoured between files only.
3. Keys resolvable: every tenant on the volume has ≥1 active key; operator
   keys present; **escrow recipient present** (once #68 lands — its absence
   fails validation the same way rotate refuses).
4. Generated zones parse (front index and seal marker round-trip as TOML;
   envelope members — MANIFEST.toml, PLAN.toml — parse; RESTORE.sh passes
   `bash -n`).
5. Block padding computed: every entry's on-tape size rounded to 512 KB blocks;
   the front index records each file's true byte size and ciphertext hash (the
   padding-trim + keyless-integrity contract RESTORE.sh and confirm depend on).

Determinism: given the same volume identity, ordered stage_sets, key set, and
generation timestamp, Layout construction is reproducible — this is what makes
"regenerate metadata from the Layout as it stands" meaningful after a
transition.

## Session states and persistence (children #22, #25, #26)

The existing schema already carries the vocabulary; **no new `writes` states
are needed**. Mapping (one session = the set of `writes` rows sharing a
volume + started_at, driven as a unit; `write_positions` rows are the cursor):

| State | `writes.status` | Meaning / entry condition |
|---|---|---|
| Planned | `planned` | Layout validated, rows inserted, nothing on tape. |
| Executing | `in_progress` | Store is executing entries; `write_positions` advances `pending → writing → written`. |
| Interrupted | `interrupted` | SIGINT (clean mark) **or** startup sweep found an orphaned `in_progress` row whose volume lock is free (crash, #376). Resumable while the Layout revalidates. |
| Sealed | `completed` | Confirm readback passed. Terminal. |
| Aborted | `aborted` | Operator explicitly abandoned an interrupted session; resume revalidation failed unrecoverably; **or** a real EOT was hit mid-write (MAM over-reported capacity — clean abort, no salvage). Terminal; the tape is not a copy. |
| Failed | `failed` | Store error other than EOT/interrupt (device gone, I/O error) with no transition available. Terminal unless operator retries → new validation → resume semantics. |

Volume status: migration 003 added **`sealed`** and **`quarantined`** to
`volumes.status`; **migration 017 removed `quarantined` again** — see the note
below. Lifecycle as the code actually walks it: `volume init`
inserts `initialized`, and the row stays there for the whole write session —
**no code makes an `initialized → active` transition.** Session progress lives
entirely in `writes.status` (the table above), never in `volumes.status`. From
`initialized` the row moves `→ sealed` (confirm passed; ADR-0003: never written
again) or `→ erased` when a re-initialised cartridge displaces it (ADR-0010).

**Corrected 2026-09-18 (issue #242), per ADR-0012's amendment "the status
column is the operator's; a medium's condition is its own fact".** Quarantine
is no longer a `status` at all. `volumes.status` is operator-owned; what a
session or a verify OBSERVES about the medium lives in
`volumes.observed_condition` (`'ok'` | `'quarantined'`, migration 017), and
the two no longer compete for one slot. Divergence at contact therefore sets
the CONDITION and leaves the status where the operator put it — which is what
makes verifying a `retired` tape safe, ADR-0011 having established that
`retired` means unfit-to-write and not unreadable.

The consequence to keep in mind when reading the rest of this document: a
quarantine used to remove a volume from coverage *by* moving its status out of
`sealed`. That mechanism is gone. `policy::coverage::eligible` and
`in_service` now consult both columns, and `is_write_target` takes the
condition as a second argument — because a write-path quarantine no longer
moves the status off `initialized`, and without that second argument a
quarantined volume would silently become a legal write target again.

`active` and `full` are read-only holdovers: `active` is written only by
`tapectl import`, describing a tape written elsewhere, and `full` is the
pre-renovation sealed-equivalent for legacy volumes. This write path produces
neither. `blank` and `missing` are schema-legal with no writer at all.

ADR-0012: `initialized` is the only status `volume write` and `volume resume`
may target. Every other status is refused by status, before either command does
anything else, and no flag overrides it. Only `sealed` volumes contribute
copies (ADR-0004).

**Status is necessary but no longer sufficient** (ADR-0012 amendment
2026-09-16, issue #199). An `initialized` volume that already carries a
`completed` `writes` row is also refused, because the real question is *does
this volume hold bytes we know about?* and status only approximates it. The two
coexist only where a `catalog rebuild` attached a tape's contents to a
pre-existing `initialized` row — #158 deliberately leaves an existing row's
status alone, so without this second guard the catalog would call a rebuilt
tape a write target. The refusal names the volume rather than its status, since
the status is genuinely still `initialized` and saying so would read as a
contradiction.

This does not change what resume may target: `planned`, `in_progress` and
`interrupted` rows are the resumable set and still pass, which is the whole
distinction the guard has to get right.

The retry-vs-UNIQUE fact: `writes` has `UNIQUE(stage_set_id, volume_id)` —
**resume reuses the existing rows**; it never inserts. (This is the H3 raw
constraint error, fixed structurally.)

## Transitions

```
            validate ok                 entry done, more remain
  (none) ────────────► Planned ────► Executing ─────────────────┐
                                        │  ▲                    │
                              SIGINT /  │  │ resume:            │ last entry +
                              crash     │  │ revalidate Layout, │ filemark
                              sweep     ▼  │ verify tape id,    ▼
                                   Interrupted ── reposition  Confirming
                                        │                       │
                       operator abandons│            readback == Layout?
                                        ▼               yes │       │ no
                                     Aborted                ▼       ▼
                                                         Sealed  volume
            real EOT during a slice                             QUARANTINED,
            (only if MAM over-reported capacity)                session
  Executing ───────────────► ABORT. No overwrite, no sacrifice. Aborted
                             Volume stays UNSEALED (no seal marker).
                             Operator reloads a fresh cartridge and
                             re-plans. The pre-flight capacity gate (#28)
                             is the real defense; this is the rare-miss
                             backstop (ADR-0007).
```

Rules that hold in every path:
- **Metadata is generated from the Layout, never from what happened to get
  written.** The front index, seal marker, planning header, and envelope
  manifests all come from the Layout (#24). (v2 has no truncate transition that
  rewrites the Layout mid-session — an EOT aborts cleanly — so the only path
  that revalidates is resume, against the unchanged Layout.)
- **Interrupt/abort skips the seal marker entirely** — an interrupted or
  EOT-aborted tape has no seal marker and is self-evidently not sealed; that is
  what Unsealed means. It is never recorded `completed`, and snapshots are not
  flipped `current`. (The front index may already be on tape at File 3, but
  without a seal marker binding it the tape is unsealed — `volume-format-v2.md`
  §4.)
- **Resume** (same session, same tape): revalidate the Layout (staged slices
  present at their recorded size — full-hashed only under `--prewrite-hash`,
  otherwise execute's inline re-hash catches a changed one; frozen generated
  zones re-hash byte-identical, always), rewind, read
  file 0, require ID-thunk identity match (label + uuid) — mismatch =
  divergence = quarantine, not overwrite (#27; since migration 017 the
  quarantine is written to `observed_condition`, not `status`). Then the **two-case cursor
  rule** (`write_positions.stage_slice_id` is NOT NULL, so only slices have
  cursor rows — metadata files never do): if **zero slices** are recorded
  `written`, restart from BOT — the front zone is pennies and regenerates
  byte-identical from the frozen staging files; if **≥1 slice** is written,
  reposition to `front_zone_len + written_slices` (both terms exact: the front
  zone length is fixed by the Layout, the slice count by the cursor rows) and
  continue. **The absent seal marker does NOT confirm the tape is unsealed**
  — corrected 2026-09-21 (issue #277, ADR-0012's "the seal is RECORDED, not
  inferred"). `seal_marker_parses_at` returns false both when a position
  holds no marker and when the read *errors*, and `MismatchKind::SealUnreadable`
  is precisely what produces an `Inconclusive` confirm — so after one the seal
  file this session wrote is exactly the file the resume cannot read. Inferring
  "unsealed" there meant falling through to `reposition_for_resume` and `seal()`
  against a physically sealed cartridge, with ADR-0003's refusal bypassed.
  Resume now consults `volumes.sealed_at` (migration 018), written when this
  session's own `seal()` returned `Ok`: set → re-enter `confirm`, never
  reposition and never seal; NULL → the seal is genuinely still owed and the
  cursor rule above applies unchanged. The tape-side probes remain as defence
  in depth, not as the decision.
- **Resume across a process restart** (#25): the rule above is scoped "same
  session", meaning the `InterruptedSession` value `execute` returned. When the
  process itself is gone, that value must be rebuilt — and it is **rehydrated,
  never regenerated**. `build()` is not reproducible across a restart for two
  independent reasons: `BuildInputs::created_at` is `Utc::now()` at call time
  and is persisted nowhere, and `mam_loads` is the drive's load count, which
  increments on every cartridge load. Either one drifting changes the ID-thunk
  bytes, so a regenerated Layout would disagree with File 0 and **Confirm would
  quarantine a perfectly good tape** — silent corruption, not a loud failure.
  Hence "re-hash byte-identical" above means re-hash the *frozen files*, not
  re-generate them. The durable path back to them: `plan` records the session
  staging directory on every `writes` row it inserts (`writes.session_dir`,
  migration 006 — `plan` is the sole writer), and `build()` freezes the Layout
  itself to `session_dir/layout.json` (a staging-side sidecar; never a
  `LayoutEntry`, never on tape). `InterruptedSession::rehydrate` reads both,
  reconstructs the cursor map from `write_positions`, and adopts only
  `interrupted` rows — never `in_progress`. Liveness is the volume's lock,
  not the row (#376): `volume init`/`write`/`resume` hold
  `locks/volume-<id>.lock` from plan through confirm (and `volume verify` for
  its readback), and the kernel releases it however the process dies.
  `db::open`'s sweep moves an `in_progress` row to `interrupted` only when
  that lock is free, and `volume resume` takes the lock before it rehydrates,
  so a live writer in another process is refused on the lock itself. An
  `in_progress` row that resume sees while holding the lock belongs to a
  process that died after this command's open; the next open sweeps it.
  Revalidation is unchanged and still runs.
- **Confirm** (#23): a single forward pass from BOP (the index is at the front,
  not the tail — no seek-back). Read the seal marker and verify it binds File 3;
  diff the front index against the Layout (navigable tier); hash each file
  against the front index's `sha256_encrypted` (integrity tier). A write's
  confirm runs the navigable tier by default and the integrity tier only
  under `--full-confirm` (ADR-0012, amendment 2026-10-06 item 1, #387); a
  passing navigable confirm seals, and the volume's full readback is then
  owed to `volume verify` — `audit` and `report verify-status` name every
  sealed volume without one. The exact
  cryptographic chain is fixed in `volume-format-v2.md` §4–5. On tape that is
  one rewind, then Files 0, 1, 2, 3 and every content file in position order
  (#389): Files 0–2 are held in memory until File 3 says what they hash to,
  and judged in its order, so the verdict is the one a file-by-file walk
  gives. Where the seal marker is *read* depends on what is known of it
  (#397): straight after the session wrote it (or `volume resume` just parsed
  it), it is read last, at the end of the same forward pass — no locate out
  to it from BOT and back; on a tape whose seal nothing has just seen
  (`volume verify`, a resume whose recorded seal did not read) it is read
  first and alone, so an unsealed tape stops at one read. The navigable tier
  always reads File 3, then spaces forward to the seal: one rewind, two
  forward spaces. Wherever it is read, the seal is *judged* first (§2.5's
  precedence): a seal that is absent, unparseable or refused is the whole
  verdict and whatever was read after it is discarded, so both orders reach
  the same evidence. `TapeStore` tracks which file the head
  is at and only spaces forward to a file ahead of it, rewinding only for one
  behind it or after anything that leaves the position uncertain (an error, a
  write, a read that returns nothing, or st's own count disagreeing). Before
  1.0.5 every read rewound to BOT, which cost a full-tape confirm about five
  hours of rewinds on LTO-6. Since 1.0.6 (#390) each file's tape read runs
  on its own thread, up to the 256 MiB read queue ahead of the hash, so the
  drive keeps reading while the host hashes; the cursor follows how the
  tape read itself ended (a sink that fails mid-file stops the read there,
  so the position is unknown; one that fails after the read crossed the
  filemark leaves the head, truthfully, at the next file). Record a
  `verification_sessions` row stating **which tier** ran (ADR-0001). Match →
  mark `sealed`. Mismatch → **three outcomes, not two** (ADR-0012's 2026-09-18
  amendment, issues #260/#267): a mismatch that `MismatchKind::proves_medium_bad`
  rules TRUE quarantines the volume (`observed_condition`, never `status` —
  issue #242) and aborts the session; a mismatch it rules FALSE — a drive or
  transport error, which says nothing about the medium — is **`Inconclusive`**:
  do not seal (the readback did not succeed, so the durability claim is
  unproven), do not write `observed_condition` (nothing was learned about the
  medium), and leave the session resumable so confirm can be retried. Before
  that amendment confirm was `evidence.mismatches.is_empty()` and had only two
  outcomes, so one transient SCSI error inside an hours-long full-cartridge
  readback condemned a sound tape. Crash mid-confirm leaves `in_progress` → swept to
  Interrupted → resume revalidates and re-confirms (confirm is idempotent; no
  dedicated state needed). A full (integrity-tier) readback keeps a
  **checkpoint** per file as it goes (#410, `readback_checkpoints`, migration
  029): each file that hashed to its front-index claim is recorded with that
  claim and the hash of File 3's true bytes. A re-entered full confirm whose
  volume's latest `verification_sessions` row is a full one that never
  finished (`in_progress` or `aborted`) re-reads the seal marker and File 3 —
  always — and skips a recorded file only when File 3 hashes as it did then
  and the file's claim is unchanged, so the seal it judges binds the very
  claims the skipped files matched; §2.5's precedence is untouched (the seal
  is still judged first, a refused seal is still the whole verdict). Skipped
  files count toward `files_checked` and are recorded again under the new
  session — keeping the time they were actually read — so a chain of
  interruptions accumulates. A readback that finished, passed or failed, is
  never continued, nor one holding a file read at or before the volume's
  recorded write abort (the 2026-09-23 adoption rule wants a full readback
  wholly after it), and the quick tier neither records nor skips anything.
  `volume verify` takes part the same way: it records its session
  `in_progress` from the start and leaves it `aborted` when stopped, so
  re-running it continues; each continues the volume's latest readback,
  whichever command took it.
- **Snapshot lifecycle transitions happen only at Sealed**, inside the same
  transaction that records evidence, and are event-logged (#58).

## Store seam (child #71)

The session owns the state machine; the store executes entries and reports.
The trait surface the children build toward: `validate`-time capacity oracle;
`execute(entry) → Written | MediumEvent(EotReached)` (medium events are
*transition requests* — the session decides, the store never self-recovers; in
v2 `EotReached`'s only outcome is abort-to-unsealed, not salvage);
`confirm(layout) → Evidence`; `read(entry)` for verify/restore legs. `execute`
and `read` are **streaming** (they take/return a `Read` plus a known length, not
a whole `&[u8]` buffered in RAM) so peak memory tracks block size, not slice
size — the H9 fix (age's STREAM already gives constant-memory encryption;
`volume-format-v2.md` §7). Since 1.0.6 (#390) the write session's execute and
`TapeStore`'s reads overlap disk, hash and tape on separate threads through a
queue of reused tape-block buffers, so "tracks block size" is now a fixed
number of blocks: at most 256 MiB (`pipeline::QUEUE_BYTES`) per pipeline, one
pipeline at a time — still a constant, still never the slice size. The trait
did not change: the pipeline sits above `execute` (the session feeds it a
`Read`) and inside `TapeStore::read_file`, so `MemStore` and every test store
see the same calls they always did. TapeStore implements contact as drive I/O with
readback confirm; the anti-tape-ism test is that WarehouseStore's shapes
(execute=upload, confirm=deposit-receipt, restore-request before read) fit the
same signatures without violence (#72, phase 3).

## Out of scope here

Multi-volume spanning (not in the design), warehouse mechanics (phase 3,
ADR-0006), compaction's use of sessions (compaction writes are ordinary
sessions; `compact-finish`'s extra guards are unchanged), and exact Rust types
(children's work).

## Open items deliberately left to children

- #26 **shrinks dramatically** under ADR-0007 — from a three-layer
  truncate/sacrifice machine to a trivial abort-to-unsealed. mhvtl cannot even
  raise a real EOT reliably (the 2026-07-20 drill: it silently corrupts overflow
  rather than returning ENOSPC), which is *why* the pre-flight gate (#28) is the
  real defense and the abort is only a rare-miss backstop. #26 becomes: on write
  ENOSPC, stop, leave the tape unsealed, mark the session `aborted`.
- #28 decides where the capacity oracle reads MAM vs config when hardware is
  absent (mhvtl MAM answers are recorded by #8 for later hardware diffing). Its
  `reserve_bytes` is just the ENOSPC buffer now (no manifest reserve).
