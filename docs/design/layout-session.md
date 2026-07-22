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
2. Every staged slice exists on disk and matches its recorded sha256 (a full
   streamed hash). This is the first layer of the **tri-layer integrity model**:
   *validate* full-hashes from disk (cheap read insurance against wasting a
   3.5 h tape write on a stale/rotted slice), *execute* re-hashes inline on the
   same streaming read that feeds the tape and cleanly aborts to unsealed on
   mismatch (closes the validate→write TOCTOU window at zero extra I/O), and
   *confirm* (#23) hashes the tape readback against the front index. Each layer
   catches a window the others cannot. Finding ③'s "no double read" applies to
   front-index generation only — it reuses the stage-time `sha256_encrypted`
   verbatim rather than re-reading slices a third time.
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
| Interrupted | `interrupted` | SIGINT (clean mark) **or** startup sweep found orphaned `in_progress` (crash). Resumable while the Layout revalidates. |
| Sealed | `completed` | Confirm readback passed. Terminal. |
| Aborted | `aborted` | Operator explicitly abandoned an interrupted session; resume revalidation failed unrecoverably; **or** a real EOT was hit mid-write (MAM over-reported capacity — clean abort, no salvage). Terminal; the tape is not a copy. |
| Failed | `failed` | Store error other than EOT/interrupt (device gone, I/O error) with no transition available. Terminal unless operator retries → new validation → resume semantics. |

Volume status: migration 003 adds **`sealed`** and **`quarantined`** to
`volumes.status`. Lifecycle: `blank → initialized → active` (unsealed, a
session has written bytes) `→ sealed` (confirm passed; ADR-0003: never written
again) or `→ quarantined` (divergence at contact, ADR-0001). Legacy `full` is
read as sealed-equivalent for pre-renovation test volumes; new code writes
`sealed`. Only `sealed` volumes contribute copies (ADR-0004).

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
  unchanged; frozen generated zones re-hash byte-identical), rewind, read
  file 0, require ID-thunk identity match (label + uuid) — mismatch =
  divergence = quarantine, not overwrite (#27). Then the **two-case cursor
  rule** (`write_positions.stage_slice_id` is NOT NULL, so only slices have
  cursor rows — metadata files never do): if **zero slices** are recorded
  `written`, restart from BOT — the front zone is pennies and regenerates
  byte-identical from the frozen staging files; if **≥1 slice** is written,
  reposition to `front_zone_len + written_slices` (both terms exact: the front
  zone length is fixed by the Layout, the slice count by the cursor rows) and
  continue. The absent seal marker confirms the tape is legitimately unsealed
  (safe to resume, not an append to a sealed volume).
- **Confirm** (#23): a single forward pass from BOP (the index is at the front,
  not the tail — no seek-back). Read the seal marker and verify it binds File 3;
  diff the front index against the Layout (navigable tier); hash each file
  against the front index's `sha256_encrypted` (integrity tier). The exact
  cryptographic chain is fixed in `volume-format-v2.md` §4–5. Record a
  `verification_sessions` row stating **which tier** ran (ADR-0001). Match →
  mark `sealed`. Mismatch → the tape lies about itself: quarantine the volume,
  abort the session. Crash mid-confirm leaves `in_progress` → swept to
  Interrupted → resume revalidates and re-confirms (confirm is idempotent; no
  dedicated state needed).
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
`volume-format-v2.md` §7). TapeStore implements contact as drive I/O with
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
