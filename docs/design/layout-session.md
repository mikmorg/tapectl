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
`tenant_envelope_backup{tenant_id}` (`volume-format-v2.md` §1.3), `seal_marker` — the v1 planning header is folded into the operator envelope as
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
   config). Until 1.6.0 this pre-flight gate was the sole capacity defense
   (ADR-0007). Since 1.6.0 an early warning ends the tape with a short seal
   ("The short ending" below), and a plan may reach past the fill ceiling
   (ADR-0012 item 113): units stay in the caller's order; the prefix that
   fits the fill budget is gated as before; further whole units are appended
   while the plan's end stays at or below 100% of the cartridge's nominal
   capacity, and only when the drive declares `post_warning_bytes`. Without
   that key the plan stops at the fill ceiling as before. Since 1.7.0 a
   spannable set is planned past that bound, to its end, and the plan order
   puts owed runs first and spannable sets last ("Spanning" below).
2. Every staged slice exists on disk at its recorded encrypted size, and has a
   recorded sha256 (since 1.7.0, every staged slice the Layout writes: a
   spannable set's slice that this tape does not plan is listed only in the
   manifest, and needs no file, `volume-format-v2.md` §4.4). Under
   `--prewrite-hash` it must also match that sha256 (a
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

The schema carried the vocabulary when v2 was designed, and two `writes`
states have been added since: **`cut`** (migration 038, 1.6.0, ADR-0012 item
112), for the short ending, and **`range`** (migration 039, 1.7.0, ADR-0012
item 121), for spanning. Mapping (one session = the set of `writes` rows
sharing a volume + started_at, driven as a unit; `write_positions` rows are
the cursor):

| State | `writes.status` | Meaning / entry condition |
|---|---|---|
| Planned | `planned` | Layout validated, rows inserted, nothing on tape. |
| Executing | `in_progress` | Store is executing entries; `write_positions` advances `pending → writing → written`. |
| Interrupted | `interrupted` | SIGINT (clean mark) **or** startup sweep found an orphaned `in_progress` row whose volume lock is free (crash, #376). Resumable while the Layout revalidates. |
| Sealed | `completed` | Confirm readback passed. Terminal. On a short-sealed volume, only a stage set whose every slice is at or before `ended_after`. (1.7.0) For a write that may span, only when every slice of the stage set, 1 to N, is on this volume at or before `ended_after` ("Confirm of a short ending"). |
| Cut | `cut` | (1.6.0) Confirm readback passed on a **short-sealed** volume, and this stage set has a slice planned after `ended_after`: the one the early warning interrupted, and every one planned after it. Terminal. **Never a copy.** Its `write_positions` rows stay as written: slices that reached the tape stay recorded (spanning, #478, uses them). It does not block releasing staging once the version has a `completed` write elsewhere. Exists only on a sealed volume with `ended_after` set; an aborted session is never `cut`. (1.7.0) A write that may span is `cut` only when none of its set's slices is on the tape. |
| Range | `range` | (1.7.0) Confirm readback passed, the write may span (`writes.may_span`), and this volume holds some but not all of the stage set's slices at or before `ended_after`, on a full seal or a short one. The range is the row's `written` positions, with `pending` past the cut (ADR-0012 item 121). Terminal. Counts towards a copy slice by slice, never as a copy on its own (item 127). Never holds staging as an unfinished write would, and its slices count where a `cut` row's do not (item 129). Exists only on a sealed volume. |
| Aborted | `aborted` | Operator explicitly abandoned an interrupted session; resume revalidation failed unrecoverably; **or** the tape ended where no seal can follow: the hard end inside a slice or a seal (more than 16 refusals of one block in a row), a refused filemark, or an early warning before the first data slice (`volume-format-v2.md` §4.3 W6; clean abort, no salvage). An early warning anywhere else seals short ("The short ending" below). Terminal; the tape is not a copy. |
| Failed | `failed` | Store error other than EOT/interrupt (device gone, I/O error) with no transition available. Terminal unless operator retries → new validation → resume semantics. |

Volume status: migration 003 added **`sealed`** and **`quarantined`** to
`volumes.status`; **migration 017 removed `quarantined` again** — see the note
below. Lifecycle as the code actually walks it: `volume init`
inserts `initialized`, and the row stays there for the whole write session —
**no code makes an `initialized → active` transition.** Session progress lives
entirely in `writes.status` (the table above), never in `volumes.status`. From
`initialized` the row moves `→ sealed` (confirm passed, or `volume resume`
adopted a write the catalog lost — #360, below; either way ADR-0003: never
written again) or `→ erased` when a re-initialised cartridge displaces it
(ADR-0010).

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

`active` is a read-only holdover: it is written only by `tapectl import`,
describing a tape written elsewhere, and this write path never produces it.
`blank` and `missing` (migration 026) and `full` (migration 032, ADR-0012
amendment 2026-10-07 item 16) left the schema: none ever had a writer.

**`volumes.ended_after`** (migration 038, 1.6.0, ADR-0012 item 112) is the
`ended_after` of a volume's **short** seal: set only on a short-sealed volume,
NULL on every other (unsealed, sealed before 1.6.0, or sealed full by 1.6.0,
whose `ended_after` is the Layout's M-1 and is not stored). It is written in
the same transaction as `sealed_at`, from the seal the session wrote or, on a
resume or a rebuild, from the seal it located (`volume-format-v2.md` §1.2 rule
6). Invariants `audit` checks: a `cut` row exists only on a volume with
`ended_after` set, and every `completed` row there has all its slices at
positions at or before it.

**`writes.may_span`** (migration 039, 1.7.0, ADR-0012 item 121) says whether
the write may span: frozen when the write is planned, and read from the row
by everything after ("Spanning" below). A `range` row has `may_span` set (a
CHECK in migration 039). Invariants `audit` checks for a `range` row: it has
at least one `written` position, its `written` positions are at or before the
volume's ending, and they are one unbroken run in slice order.

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

**The one exception: a write the catalog lost** (ADR-0012 amendment
2026-09-29 (later), issue #360, `volume::adopt_lost`). A catalog restored from
a backup taken between `volume init` and `volume write`, then given the tape's
contents by `catalog rebuild --from-volume`, holds the volume as `initialized`
with no `sealed_at`, condition `ok`, and nothing but `completed` `writes` rows
(`adopt_lost::is_lost_write`; `cut` rows too since 1.6.0, and `range` rows
since 1.7.0). `volume resume` ADOPTS that row instead of
refusing it, on all of the following, refusing by the first unmet one:

1. the tape's File 0 uuid equals the catalog row's, and a valid seal marker
   binds the front index (File 3, at the position File 0 records);
2. a passing **full** `volume verify` of the volume is recorded, STARTED
   strictly after the latest `catalog_rebuild` event for it — a recorded row,
   asked before the tape moves; resume never runs the verify itself;
3. every slice hash in the front index equals the hash this catalog recorded
   when it staged that slice, in a stage set with `origin = 'staged'` (a set
   the rebuild made from the tape never satisfies it — that would be the tape
   vouching for itself); the catalog records no slice of the volume at a
   position the front index does not list; and every stage set on the volume
   is whole on it. On a tape that ended early (1.6.0), the seal is located by
   `volume-format-v2.md` §1.2 rule 6, the hashes are compared only at or
   before its `ended_after`, and a stage set with a slice after it is not
   whole but cut: adoption records it `cut` and records `ended_after`, as a
   passing short confirm does ("The short ending" below). Since 1.7.0 a set
   whose block states `may_span = true` is split as confirm splits it,
   `completed`, `range` or `cut`, and a `range` row is adopted (ADR-0012
   item 130).

Adoption writes what a passing confirm writes — `status = 'sealed'`,
`sealed_at` from the seal marker's own time, the write bookkeeping, the
snapshot promotions and a `write_completed` event — in one transaction.
`catalog rebuild` itself still never changes a status, and `volume write`
still refuses the same row as `VolumeHasRecordedWrite`.

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
            the hard end inside a slice or a seal,              QUARANTINED,
            a refused filemark, or an early warning             session
            before the first data slice                         Aborted
  Executing ───────────────► ABORT. No overwrite, no sacrifice.
                             Volume stays UNSEALED (no seal marker).
                             Operator reloads a fresh cartridge and
                             re-plans. Since 1.6.0 an early warning in a
                             data slice is not an abort: it seals (below).
```

The ending after an early warning (1.6.0, #420; the rules are "The short
ending" below, the bytes `volume-format-v2.md` §4.1–§4.3):

```
               early warning first seen in data slice K;
               slice K finished (st's refusals retried)
  Executing ─────────────────────────────────────────► write_early_warning
                                                        event, committed
                                                             │
                              K = M-1 ┌──────────────────────┴───────┐ K ≤ M-2
                                      ▼                              ▼
                              full seal at M               Sealing short: the short
                              (ended_after = M-1)          seal at K+1 (ended_after = K)
                                      │                              │
                                      └──────────────┬───────────────┘
                                                     ▼
                                     Confirming: entry K and the seal are read
                                     back under every tier (item 114); a short
                                     seal is confirmed over positions 0..K only
                                        pass │                 │ mismatch: as above
                                             ▼                 ▼ (quarantine or
                                     Sealed. Short: each stage    inconclusive)
                                     set wholly at or before K
                                     → completed, every other →
                                     cut; volumes.ended_after = K.
                                     Full: as before.
```

Since 1.7.0 a write that may span takes a third outcome, `range`, on either
ending ("Confirm of a short ending", "Spanning").

Rules that hold in every path:
- **Metadata is generated from the Layout, never from what happened to get
  written.** The front index, seal marker, planning header, and envelope
  manifests all come from the Layout (#24). The short seal (1.6.0) is no
  exception in source: it is the Layout's entries 0..K plus its own, and its
  one fact from the session is `ended_after = K`. File 0, File 3 and the
  envelopes stay the plan. (v2 has no transition that rewrites the Layout
  mid-session — a hard end aborts, and an early warning ends the session at a
  position of the unchanged Layout — so the only path that revalidates is
  resume, against the unchanged Layout.)
- **Interrupt/abort skips the seal marker entirely** — an interrupted or
  aborted tape has no seal marker and is self-evidently not sealed; that is
  what Unsealed means. A short seal is not an abort: it is a seal, and the
  tape it ends is Sealed. It is never recorded `completed`, and snapshots are not
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
  continue. **The medium bounds the cursor** (#403, ADR-0012 2026-10-07 item
  23): the catalog can record a slice `written` that never reached the tape
  (power lost after the row, before the drive flushed), so resume continues
  from **min(catalog cursor, files on the medium)** and rewrites from there. **The absent seal marker does NOT confirm the tape is unsealed**
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
  in depth, not as the decision. Since #460 they no longer read a failed read
  as an absent seal: a seal position that errors counts as absent only when
  the medium shows END OF DATA at or before it (`Store::eod_at_or_before`) or
  is blank at BOT. Any other failure is `ContactOutcome::SealUnreadable`: a
  fresh write refuses it, `--force` included, and a resume re-enters `confirm`
  if `sealed_at` is recorded and otherwise refuses, writing nothing. A resume
  that finds its own tape sealed records `sealed_at` from the seal marker's
  time.
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
  forward spaces. After an early warning (1.6.0) it also reads the entry at
  `ended_after` between them (item 114, "The short ending"): still one rewind
  and two forward spaces, because the seal is the file right after that
  entry. Wherever it is read, the seal is *judged* first (§2.5's
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
  wholly after it), nor one whose oldest read is older than the volume's
  resolved `verify_interval_days` — the tightest among the units written to it,
  each unit's being its archive set's, else `[defaults]`'s, which is 365 days
  when the file does not say (ADR-0012 2026-10-07 item 1; 2026-10-15 item 66;
  2026-10-16 item 75); no limit only on a volume no unit was written to — and the quick tier neither records nor skips anything. A continued
  readback that concludes records as its `started_at` the oldest `checked_at`
  among the files it skipped, so freshness never overstates what was read
  when. `volume verify` takes part the same way: it records its session
  `in_progress` from the start and leaves it `aborted` when stopped, so
  re-running it continues; each continues the volume's latest readback,
  whichever command took it.
- **Snapshot lifecycle transitions happen only at Sealed**, inside the same
  transaction that records evidence, and are event-logged (#58).

## The short ending (#420, tapectl 1.6.0)

Ruled 2026-10-20 (ADR-0012 items 9, 99, 110-115). What reaches the tape, and
the writer's obligations at the early warning (W1-W9), are
`volume-format-v2.md` §4.1-§4.3; this section is the session's side.

**Deciding the ending.** After each entry's `Written` verdict, the session asks
the store for `early_warning()` (Store seam, below). The first time it is
`Some` after a data slice at position K:
- K ≤ M-2: the session writes no further data entry and moves to **sealing
  short**: the short seal at K+1, `ended_after = K`.
- K = M-1 (the last data slice): the full seal at M, as planned.
- Before the first data slice the store's warning ends the session as an
  abort (`aborted` rows, `write_aborted` naming the warning, the tape
  unsealed). So does the hard end, or a refused filemark, anywhere: the store
  returns its medium-full error, and the session never sees `Written`.
- A warning first seen in the seal's own blocks changes nothing: the session
  writes no event and seals as it was going to, and its session log names the
  warning.

Sealing short is not a `writes` status: the rows stay `in_progress` until
confirm, and what makes the ending durable is the event.

**The `write_early_warning` event.** One row in `events`, committed after
entry K's cursor row is `written` and before the seal's first block goes to the
store, for both endings that follow a warning (K ≤ M-2 and K = M-1):

| Column | Value |
|---|---|
| `entity_type` | `'volume'` |
| `entity_id`, `entity_label` | the volume's id and label |
| `action` | `'write_early_warning'` |
| `field` | `'ended_after'` |
| `old_value` | NULL |
| `new_value` | K, as decimal text: the `ended_after` the session will seal at |
| `details` | one JSON object: `{"bytes_at": B, "refusals": R, "post_warning_bytes": P, "seal": "short"}`, with `"seal": "full"` when K = M-1 |

- `bytes_at` counts on-tape bytes from the beginning of the tape to the first
  refused block: the Layout's block-padded sizes of every file before the
  first entry this process wrote, plus the store's own `bytes_at`. Filemarks
  add nothing.
- `refusals` is the store's count of refused blocks, every one retried, up to
  the end of entry K.
- `post_warning_bytes` is the drive's configured key in bytes, or JSON `null`
  when the drive declares none.
- The reader that decides anything reads only `new_value`. The rest is the
  wear record of ADR-0012 item 98.

**Resume after a crash past the warning.** The rehydrated session also reads
`volumes.ended_after` and the latest `write_early_warning` event for the volume
whose `timestamp` is at or after the session's `started_at`. Then, in order:
1. `sealed_at` is recorded: re-enter confirm (the rule in "Transitions"), with
   the ending from `volumes.ended_after` (NULL: full).
2. Otherwise locate the seal (`volume-format-v2.md` §1.2 rule 6). If it parses
   (§4.1 S1-S6) and binds this session's File 3, the tape is sealed: record
   `sealed_at` from the seal's own time and, for a short seal, `ended_after`
   from it; re-enter confirm. Never write.
3. Otherwise, with the event: the ending is decided. Count the files on the
   medium by filemarks (`reposition_at_most(K+1)`, which never moves past
   K+1): N. The seal goes at P = min(K+1, N), with `ended_after = P-1`:
   - P-1 before the first data slice: abort.
   - P-1 = M-1 (only when the event's K was M-1): the full seal at M.
   - Otherwise the short seal at P.

   Resume writes no data entry. When P-1 is below the event's K (the medium
   lost files the catalog had recorded written, #403), the seal and
   `volumes.ended_after` state P-1 and the session log names both numbers; no
   second event is written. A seal torn by the crash may have been closed by
   st's filemark on close and so counted at K+1. Writing the seal at P replaces
   it, because a write at a position ends the recorded data there.
4. Otherwise (no event): the cursor rule in "Transitions", as before. The
   medium is past the warning, so the drive reports it again on the first
   write there, and the session takes the transitions above afresh.

**Confirm of a short ending.** The confirm plan carries the ending (`ended_after`
and the kind). The byte checks are `volume-format-v2.md` §5: the seal's
`ended_after` must equal the session's, positions after it are not read, and
entry K and the seal are read back under every tier (item 114). A pass commits,
in one transaction:
- the `verification_sessions` row, stating the tier;
- `volumes.status = 'sealed'` (with `sealed_at` and `ended_after`, recorded
  when `seal()` returned);
- each stage set on the volume whose every slice is at or before `ended_after`:
  `writes.status = 'completed'`, and its snapshot promoted `current` as today;
- every other stage set of the session: `writes.status = 'cut'`. That covers
  the one the warning interrupted and every one planned after it, whether or
  not any of its slices reached the tape;
- a `write_completed` event naming the short ending and the cut units.

A mismatch takes the three outcomes in "Transitions" (quarantine, inconclusive,
pass). An `ended_after` disagreement (`ended_after_disagrees`) is inconclusive.

**A write that may span** (1.7.0, ADR-0012 items 121 and 128). For a row
whose `writes.may_span` is set, confirm splits three ways, and on a full seal
as on a short one: a continuation's range can end a tape that seals full, and
a set can be whole on a short tape. `ended_after` is M-1 on a full seal.
- `completed` when every slice of the stage set, 1 to N, is `written` on this
  volume at or before `ended_after`: the volume holds the set whole and is a
  copy of it (item 93's middle tape);
- `range` when at least one is: the range is the row's `written` positions,
  and the row's `write_positions` past the cut stay `pending` (item 121);
- `cut` when none is.

A row whose `may_span` is not set keeps the rule above. The snapshot of a
`completed` set is promoted `current` as today, and that of a `range` set
when the promotion rule holds ("Spanning" below), this volume counted. The
`write_completed` event names the range units, with their slice runs, as it
names the cut units. `catalog rebuild` and the adoption of a lost write make
the same three-way split from the tape (`volume-format-v2.md` §4.4). That is
how item 130's "the row is `range` when the block says `may_span`" is read
for a block whose every slice, or none, is on the tape: so a rebuilt tape
gets the row its confirm gave it.

**After a short seal.** The command exits 0 (a short seal is a success), names
the cut units and says what writes them: an ordinary `volume write` of the same
staged sets onto another cartridge. The staging release (#440,
`staging::release`) and `staging clean` treat a `cut` row as no write: it
neither counts as a copy nor holds the staging as an unfinished write would.
So a cut unit stays staged until its version has a `completed` write, and from
then on it is released by the same rule as any other (ADR-0012 item 112).

From 1.7.0 the command also names each unit the tape ended in as a range, with
the slices it holds, and says that an ordinary `volume write` onto another
cartridge of the same generation continues it ("Spanning" below).

**Per-slice release** (1.7.0, ADR-0012 items 85 and 129). Staging is released
slice by slice. After a write, each slice whose copies (item 127) meet its
set's copy floor is released:
in one transaction its `stage_slices.staging_path` is cleared and
`stage_slices.released_at` (migration 039) is set, and then its file is
removed. The release of the set's last slice moves the set to `cleaned`. The
#402 checks, which refuse a staged slice missing on disk, accept a missing file
only when `released_at` is set. The release gate's refusal names the slices
still short of copies (a `ReleaseBlocker` variant of its own). A `range` row is
terminal: like a `completed` row it never holds staging as an unfinished write
would, and unlike a `cut` row its slices count. A set with no `range` row has
every slice on the same volumes, so for it this is the whole-set release of
1.6.0. `--keep-staging` keeps
every slice. `volume read-slices` that stages a released slice again clears
its `released_at`.

Per-slice release is safe only with item 128's floor ("Spanning" below). At a
copy target of 1, slices 1..K are released after tape 1 while the version is
not yet current; without the floor, retiring tape 1 would leave 1..K nowhere.
Releasing the whole set only once its least-covered slice meets the floor was
considered and not chosen: staging would hold the unit for the whole span,
against item 85.

**Later contacts.** `volume verify` reads `ended_after` from
`volumes.ended_after` and walks positions 0 to it (`volume-format-v2.md` §5).
`catalog rebuild` and the adoption of a lost write locate the seal, record
`ended_after` from a short one, and split the rows `completed`/`cut` by the same
rule as confirm. `volume rewrite` (#483) reads only `completed` writes; when its
destination seals short, the rewrite stays open, the cut slices stay pinned in
staging, and the command names `volume write` for them.

From 1.7.0 (ADR-0012 items 127, 130, 133):
- `catalog rebuild` and adoption split a row that may span three ways, as
  confirm does (above). A rebuild records all N slice rows of the set from the
  manifest, and `write_positions` only for the slices this tape's plan holds:
  `written` at or before `ended_after`, `pending` after. It promotes a version
  only by the rule in "Spanning", so a rebuild of one tape of a span leaves the
  version `staged`, reading 0 copies until its other tapes are rebuilt.
- `volume verify` sets `writes.write_verified` on a `range` row it read back in
  full, as on a `completed` one.
- `volume rewrite` refuses a source volume that holds a range, naming the
  range (item 133), and plans every write it makes with `may_span` unset (item
  142). Compaction (`volume compact-read`, `volume compact-finish`, `volume
  compact`) refuses a volume that holds a range in the same way.

## Spanning (#478, tapectl 1.7.0)

Ruled 2026-10-16 to 2026-10-21 (ADR-0012 items 82-95 and 121-143). What
reaches the tape, and how a reader reads a range, is `volume-format-v2.md`
§1.2 and §4.4; this section is the session's and the catalog's side.
CONTEXT.md defines Spanning, Range and Cut. A session still writes one tape:
each tape of a span is its own write session.

**The frozen flag** (items 82 and 121). A unit allows spanning through the
policy key `allow_spanning`, resolved like the other policy keys (the unit's
dotfile, then its archive set, then `[defaults]`) and off by default. Whether
a stage set's write may span is decided when the write is planned and stored on
its row as `writes.may_span`, because the dotfile is read at use and may change
while a span is in progress. Everything after the plan reads the row: the build
(File 0's and the seal placeholder's `requires`, `volume-format-v2.md` §1.2),
the seal, confirm and resume. A rehydrated session reads it from the rows and
never resolves the policy again. A write of a set that already has a `range`
row on some volume is planned with `may_span` set even when the key has since
been turned off, so a span in progress can always finish. `volume rewrite`
plans every write with `may_span` unset: a spannable unit it carries is planned
and recorded whole, and is cut if the early warning comes, as in 1.6.0 (item
142), because a rewrite never plans past the fill ceiling.

**What the plan works from** (items 122 and 127). The catalog records no copy
or span identity: no table says which tapes make up which copy. The planner
works from per-slice shortfalls. A slice's copies are the distinct eligible
volumes holding a `written` position of it in a `completed` or `range` row. A
slice is **owed** while its copies are below its set's copy floor. A spannable
set's Layout entries are only the slices this tape plans; its manifest block
lists every slice (`volume-format-v2.md` §4.4).

**The plan order** (items 83, 94, 95, 99, 113, 122 and 123). On each tape, in
this order:
1. **Owed runs.** A spannable set with a `range` row on a sealed volume and
   slices still owed is a span in progress. Its owed slices are laid out first,
   as one unbroken run in slice order, from its lowest owed slice to its
   highest, inclusive.
2. **Whole units,** those that may not span, as in 1.6.0: committed up to the
   fill ceiling, and with no spannable set queued, planned past it within item
   113's bound (items 99 and 113). Whole units planned after a span's tail keep
   item 113's bound.
3. **Spannable sets** with no range yet, last (items 83 and 95), so that
   whatever ends the tape cuts a unit that allows it. The first one that
   crosses item 113's bound is planned to its end, and the early warning, not
   the plan, ends its range (items 84 and 123). No set is planned after it.
   Several queued sets go in the order the write takes them.

A volume holds at most one unbroken range of a stage set (item 122). With a
copy target of 2 this order gives item 93's layout. Tape 1 ends in the set's
range 1..K. On tape 2 the owed run is the whole set (slices 1..K owe one more
copy, K+1..N two), so tape 2 begins with it and holds it whole. Tape 3 begins
with the owed run K+1..N. The rows are `range`, `completed` and `range`, and
every slice is on two volumes.

**A spannable set larger than one tape** (item 94) may span, and does not
wait behind whole units. It starts as early on its first tape as it needs to
touch the fewest tapes, by an estimate of where the early warning comes taken
at the cartridge's nominal capacity. The whole units it, or an owed run, leaves
no room for are deferred to a later tape, not refused, so they fill in after
its tail on its last tape and tape fill is unchanged. Item 93's whole-tape copy
cannot exist for such a set; its copies combine slice by slice. A spannable set
that fits on one tape keeps item 83's place (item 95).

**Refusals.**
- **No `post_warning_bytes`** (item 123): on a drive that does not declare
  the key, a spannable set that would pass the fill ceiling is refused, and
  the refusal names the key.
- **One generation per copy's span** (item 86): a run that does not start at
  slice 1 is planned only on a cartridge of the generation of the volume
  whose range ends just before it. Otherwise the write is refused.
- **A released owed slice:** an owed slice whose staged file was released is
  refused, naming `volume read-slices --from` to stage it again (#402's shape).
- **At stage time** (items 17, 82, 85 and 94): a spannable unit is staged
  whole before its first tape is written. One larger than the staging area
  (the filesystem's total size, not its free space) is refused by name. A unit
  that does not allow spanning is still refused above one tape, and the
  refusal names the key.

**Promotion and the retire floor** (item 128). A spanning version becomes
`current` once every slice of one of its stage sets is on a sealed volume:
each has a `written` position in a `completed` or `range` row on a volume
whose status is `sealed`, the volume being sealed counted. Confirm, the
adoption of a lost write and `catalog rebuild` promote by this rule, in the
transaction that records the seal or the rebuild. Until then the version stays
`staged`, and the retire floor protects it: `versions_at_stake` holds a version
that has a `range` row on an eligible volume at stake when that volume is the
only holder of any of its slices (`sole_slices`, widened to match). `audit`
names a span still in progress and the slices it is missing. Promoting at the
first range's seal (the unit would read 0 copies for the whole span) was
considered and not chosen.

**Copies, places and deposits** (items 127, 134 and 138). A version's copies
are, for each of its stage sets, the copies of the set's least-covered slice,
summed over the sets; slices of two stage sets never combine. Each tape,
deposit and place is credited to one stage set only, the lowest-numbered
stage set of the version that it holds (item 138). Locations use the same
minimum, and a deposit of a range volume counts for that range. A stage set
with no slice rows counts its whole writes. The planner does not know where a
tape will be kept, so item 93's "one location" is met after the plan: the
write names the tapes that make up each whole copy of a spanning version
("keep these together"), and `audit`'s per-slice location count checks where
they went (item 134).

**Wear** (item 98). A tape that ends in a range records its real end in the
`write_early_warning` event, as every tape that saw the warning does ("The
short ending").

## Store seam (child #71)

The session owns the state machine; the store executes entries and reports.
The trait surface the children build toward: `validate`-time capacity oracle;
`execute(entry) → Written | MediumEvent(EotReached)` (medium events are
*transition requests* — the session decides, the store never self-recovers; in
v2 `EotReached`'s only outcome is abort-to-unsealed, not salvage);
`confirm(layout) → Evidence`; `read(entry)` for verify/restore legs.

**`early_warning() → Option<EarlyWarning>`** (1.6.0, #420) is a sticky fact,
not a medium event. It is `None` until the store's first refused data block;
from then on it is `Some(EarlyWarning { bytes_at, refusals })` for the rest of
the store's life: `bytes_at` is the bytes the store had accepted since it was
opened, before the first refusal, and `refusals` counts every refusal it has
retried. The store retries refusals itself (`volume-format-v2.md` §4.3 W2) and
returns `Written` for an entry whose blocks and filemark were all accepted. It
returns `EotReached` (the medium-full error) only when it could not finish: the
hard end, or a refused filemark. In v2 that still has one outcome, the abort;
the early warning is never `EotReached`. The session asks after every
`Written`. The trait's default is `None`, for a store with no early warning (the
`eod_at_or_before` pattern); `TapeStore` (through one retrying block-write
function in `tape::ioctl`), `MemStore` and `FakeTape` implement it, the doubles
from a configured warning point and hard end.

**`store::locate_seal`** (1.6.0) is the one implementation of
`volume-format-v2.md` §1.2 rule 6 over the trait, shared by every Rust reader
that needs the seal: the chain walk, the contact check (`probe_seal`), resume,
`catalog rebuild` and the adoption of a lost write. It reads the planned
position; when that read fails and the store shows END OF DATA at or before it
(`eod_at_or_before`), it reads the last file, numbered from the store's own
count of the files on the medium (the count `reposition_at_most` already takes
from st). It never asks for a block address.

`execute` and `read` are **streaming** (they take/return a `Read` plus a known length, not
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

Warehouse mechanics (phase 3,
ADR-0006), compaction's use of sessions (compaction writes are ordinary
sessions; `compact-finish`'s extra guards are unchanged), and exact Rust types
(children's work).

## Open items deliberately left to children

- #26 **shrinks dramatically** under ADR-0007 — from a three-layer
  truncate/sacrifice machine to a trivial abort-to-unsealed. mhvtl cannot even
  raise a real EOT reliably (the 2026-07-20 drill: it silently corrupts overflow
  rather than returning ENOSPC), which is *why* the pre-flight gate (#28) is the
  real defense and the abort is only a rare-miss backstop. #26 becomes: on write
  ENOSPC, stop, leave the tape unsealed, mark the session `aborted`. *Superseded
  in 1.6.0 (#420):* ENOSPC is first the early warning, which the store retries
  and the session ends with a seal ("The short ending"); only the hard end, a
  refused filemark, or a warning before the first data slice aborts.
- #28 decides where the capacity oracle reads MAM vs config when hardware is
  absent (mhvtl MAM answers are recorded by #8 for later hardware diffing). Its
  `reserve_bytes` is just the ENOSPC buffer now (no manifest reserve).
