# v2 implementation plan — executor playbook

This is the build script for the Layout-v2 regear, written for an implementing
agent that was **not** part of the design sessions. Every task carries its
rationale compressed into guardrails. The design itself is settled — your job is
execution, not redesign.

## Read these first, in this order

1. `CLAUDE.md` (repo rules — build/test conventions)
2. `docs/design/volume-format-v2.md` (the byte format — normative)
3. `docs/design/layout-session.md` (the state machine — normative)
4. `docs/design/v2-open-questions.md` §§8–11 (microcosm, module design,
   chain walk, Library — the resolved decisions this plan implements)

**Authority order when texts disagree:** volume-format-v2.md → layout-session.md
→ the decision sheet → this plan. If two normative docs genuinely conflict, or a
step is ambiguous in a way that changes bytes-on-tape or schema: **STOP and ask
the operator.** Do not invent. Do not "pick the reasonable one" silently.

## Global rules (apply to every task)

- Work on branch **`feat/format-v2-regear`**. Never commit to master.
- `cargo check --all-targets` while iterating; debug builds only. **Never**
  `--release` (repo rule; the only exception is the gated perf suite, which
  this plan never runs).
- Before every commit: `cargo fmt`, `cargo clippy --all-targets` (must be
  **zero** warnings), `cargo test` (ungated suite must be green at **every**
  commit — exception noted in T9).
- All logic in the library crate (`src/`), never `src/main.rs`.
- After any clap/CLI change: `cargo run --example gen_man` and commit the
  regenerated `docs/man/*.1`.
- **No new dependencies** without operator approval. Where you need
  deterministic pseudo-randomness, derive bytes from `sha2` (already a dep) in
  counter mode — do not add `rand`.
- Do not touch `../homorg` or `../lcsas` (read-only pattern sources), do not
  file GitHub issues (R&D mode), do not edit the design docs except where a
  task explicitly says so.
- Never fix a red test by weakening its assertion. If a test contradicts the
  design docs, the docs win — say so in the commit message. If it contradicts
  your code, your code is wrong.
- Scope discipline: touch only what the task names. No drive-by refactors, no
  module reorganization, no "while I'm here."
- Commit style: imperative summary + a body explaining *why*, referencing the
  design doc section that mandates the change (e.g. "per volume-format-v2 §4").
  Small commits, one coherent step each.

## The three sacred invariants (violating any of these is a stop-the-line bug)

1. **The seal marker is written only inside the session lifecycle, ever.** An
   interrupted, aborted, or resumable tape must have NO seal marker — its
   absence IS the unsealed signal (`volume-format-v2.md` §4). No code path may
   write it except `seal()` after the last content file.
2. **`Layout::validate` full-hashes staged slices** (tri-layer L1,
   `layout-session.md` validation point 2). Never "optimize" it to size-only.
   Execute re-hashes inline (L2) and aborts on mismatch; confirm re-hashes from
   tape (L3). All three layers exist on purpose; none is redundant.
3. **No plaintext file carries tenant/unit names, filenames, `sha256_plain`, or
   key fingerprints** (`volume-format-v2.md` §2). On-tape sizes and ciphertext
   hashes are the only permitted plaintext facts. The leak-scan test (T9) is
   the enforcement — never weaken its needles.

## Verification palette

```bash
cargo check --all-targets            # fast compile gate
cargo test                           # ungated: unit + integration + isolation
cargo test --lib volume::            # focused module runs
cargo clippy --all-targets           # zero warnings required
cargo fmt --check                    # formatting gate
cargo run --example gen_man          # regen man pages after CLI changes
TAPECTL_MHVTL=1 cargo test --test mhvtl_e2e -- --ignored --nocapture   # needs /dev/nst0
scripts/mhvtl-verify-gate.sh         # 4-leg gate — only meaningful after T9
```

mhvtl gotcha: SCSI enumeration shuffles on reload; the gate discovers devices —
never hardcode `/dev/sgN`. The tape-device mutex in `tests/mhvtl_e2e.rs`
(`tape_lock()`, ~line 37) must guard every test that touches the drive.

---

## T0 — Rebase the branch onto master

**Goal:** the branch (`b89b8eb`..`9f75dc2`: primitives, seal-embed, 10G
default) predates several master docs commits. Rebase so the design docs you
read are the ones in your tree.

`git rebase master` on `feat/format-v2-regear`. Expect **no conflicts** (branch
touched only `src/volume/layout.rs`, `src/volume/layout_model.rs`,
`src/config.rs`; master moved only docs). If a conflict appears anyway, master's
docs win verbatim.

**Verify:** `cargo test` green, `git log --oneline -5` shows branch commits atop
master. **Done when:** rebased, green, and `docs/design/*.md` in your tree
contain §§8–11.

## T1 — Migration 003 (schema for sealed/quarantined + escrow)

**Goal:** the DDL drafted in decision-sheet §3.6 becomes
`src/db/migrations/003_v2_lifecycle.sql`, registered in `migrations()`
(`src/db/mod.rs:40`).

Steps:
1. Copy the full `volumes` DDL from `001_initial.sql` (keep **every** column
   and default identical — the real CHECK includes `retired`,`missing`,
   `erased`; keep them) into `volumes_new` with the CHECK extended by
   `'sealed','quarantined'`. Copy-swap-rename per §3.6, then recreate exactly
   `idx_volumes_location` and `idx_volumes_status`.
2. `ALTER TABLE encryption_keys ADD COLUMN is_escrow INTEGER NOT NULL DEFAULT 0;`
3. **Build-time verify from §3.6:** confirm `rusqlite_migration` disables
   foreign keys around migrations (read its docs/source for the pinned
   version). If it does NOT, wrap the rebuild per SQLite's 12-step procedure
   manually and say so in the commit. Either way add a test asserting
   `PRAGMA foreign_key_check` returns empty after migrating a DB that already
   contains volumes with child rows (writes, verification_sessions).

Traps: do NOT drop or rename any existing column; do NOT touch
`verification_sessions` (tiers map onto the existing `verify_type`); do NOT
remove `'sacrificed'` from `write_positions` or the `eot_recovery` columns —
they are documented inert reserve.

**Verify:** new unit test migrating a fresh DB and a populated 002-level DB;
statuses `sealed`/`quarantined` insertable; legacy `full` row still readable.
**Done when:** tests green, `db fsck` (existing command) passes on a migrated DB.

## T2 — Escrow recipient wiring (ADR-0005 substance)

**Goal:** one permanent escrow identity participates in every encryption;
rotation refuses without it.

Steps:
1. CLI: `key generate --escrow` — generates an age identity, **prints the
   secret once** for paper transcription with an unmissable warning, stores
   ONLY the public key (`is_escrow=1`, `is_active=1`); refuses if an escrow row
   already exists. `key import --escrow <pubkey>` adopts an existing one under
   the same rules. `key list` badges it. Regen man pages.
2. Every recipient-list assembly appends the escrow public key. Find them all:
   `grep -rn "encrypt_data(" src/ --include="*.rs"` — staging slice encryption
   (`src/staging/mod.rs`, `encrypt_data` callers) and every envelope encryption
   in `src/volume/write.rs`. Factor a single
   `queries::recipient_list_with_escrow(conn, base)` helper so future call
   sites can't forget.
3. `key rotate` refuses (hard error, clear message) if no `is_escrow=1` row.
4. `KeyAvailability.escrow_recipient_present` (in
   `src/volume/layout_model.rs`) flips from `None` to
   `Some(row exists)` wherever callers assemble it.

Traps: the secret must NEVER be written to DB, config, logs, or tracing output.
`is_escrow` is exempt from rotation — rotate must not deactivate it.

**Verify:** new tests — every fresh ciphertext trial-decrypts with the escrow
identity (generate one in-test); rotate-without-escrow refuses; rotate leaves
the escrow row untouched. **Done when:** those tests green + clippy clean.

## T3 — Microcosm fixture generator (decision-sheet §8)

**Goal:** deterministic synthetic media library for every later test tier.

Create `tests/common/mod.rs` (Rust integration-test shared-module convention;
each consuming test file declares `mod common;`):

```rust
pub const MICRO_BLOCK: u64 = 524_288;          // NEVER scaled (format constant)
pub const MICRO_TAPE_NOMINAL: &str = "2400M";  // 1/1024 of 2400G
pub const MICRO_SLICE: &str = "10M";
pub const MICRO_ENOSPC: &str = "8M";           // a few blocks; NOT linear-scaled
pub struct MicroSpec { pub n_units: usize, pub seed: u64 }  // sizes drawn 2–15M
pub fn generate_library(root: &Path, spec: &MicroSpec) -> Vec<UnitFixture>;
```

Determinism without new deps: unit i's size = 2M + (u64 from
`sha256(seed‖i)`) % 13M; content = `sha256(seed‖i‖block_no)` repeated to
length. Folder shape per §8: one dominant file (~90%, e.g. `movie.mkv`) + small
sidecars (`cover.jpg`, `movie.srt`). Do NOT emit names matching the default
excludes (`*.nfo`, `*.tmp`, `Thumbs.db`, `.DS_Store`) — they would silently
vanish from snapshots and break exact-diff verification.

**Verify:** self-test — same seed ⇒ byte-identical tree (hash the tree twice);
different seed ⇒ different. **Done when:** generator + self-test green.

## T4 — Store trait v2 (streaming + confirm surface)

**Goal:** grow `src/store.rs` to the §9 trait. This changes an existing pub
trait — update both impls and all callers in the same commit.

```rust
pub enum Tier { Navigable, Integrity }
pub struct Evidence { pub tier: Tier, pub files_checked: u32, pub mismatches: Vec<Mismatch> }
pub trait Store {
    fn capacity(&mut self) -> Result<CapacityReport>;
    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64>;
    fn confirm(&mut self, layout: &Layout, tier: Tier) -> Result<Evidence>;
    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64>;
}
```

Steps:
1. Streaming `execute`: read `src` in `block_size` chunks, pad the final block
   with zeros to the block boundary, write via the existing
   `TapeDevice::write_file_with_mark` / `write_file_with_sync_mark`
   (`src/tape/ioctl.rs:204/211`) — or, if those take whole buffers, add
   block-wise write methods to `TapeDevice` and leave the old ones for the v1
   read paths. Peak memory must be O(block_size), never O(len). Return bytes
   committed including padding.
2. `read_file(position)`: rewind + `forward_space_file(position)`
   (`ioctl.rs:107/129`) + read blocks to filemark, streaming into `sink`.
3. `confirm`: implement the chain walk exactly per `volume-format-v2.md` §5 —
   forward from BOP: seal marker present? → hash File 3 vs seal binding →
   parse + diff File 3 vs `layout` (positions/types/sizes) = Navigable; then
   (Integrity tier only) hash every file vs File 3's `sha256_encrypted`.
   Hash the TRUE bytes: read the padded tape file, truncate to the front
   index's `size_bytes` before hashing. Collect mismatches; do not stop at the
   first (it's a report, like validate).
4. `MemStore`: implement all four against its `files: Vec<Vec<u8>>` (store
   PADDED bytes to mirror tape semantics). This is what makes T7 test the real
   confirm code.
5. Add the **hashing tee reader** (small `Read` adapter computing sha256 of
   bytes as they pass) in `src/staging/` or a new `src/util.rs` — T6 uses it.

Traps: sync marks: v2 uses `sync=true` ONLY for the seal marker (the final
flush covers everything; v1's op-envelope syncs are dropped — decision-sheet
§9). Malformed seal marker in `confirm` ⇒ report "unsealed", NOT an error
(fail-safe precedence, sheet §2.5). Do not implement EOT salvage of any kind —
ENOSPC bubbles up as an error the session turns into a clean abort.

**Verify:** unit tests on MemStore for all four methods; a padded-write test
(len not block-aligned ⇒ padded, recorded size correct). **Done when:** green +
existing callers compile (v1 `volume_write` may temporarily adapt with a
whole-buffer `Cursor` wrapper — fine until T8 deletes it).

## T5 — Generators v2 + Layout build/materialize/validate

**Goal:** every byte the tape will hold is producible and frozen to disk before
contact.

### T5a — generators (`src/volume/layout.rs`)
1. **ID thunk v2** per sheet §2.3: new generator (replace
   `generate_id_thunk`'s 18-arg signature with a small struct). `[volume]`
   magic `"tapectl-volume-v2"`, `layout_version = 2`; `[layout]` carries ONLY
   `front_index = 3`, `seal_marker`, `total_files`. Delete the v1 position
   fields. Keep the human header; it must say the map is File 3.
2. **Guide v2** (`generate_system_guide`): rewrite the Quick Reference to the
   v2 zone order (envelopes at File 4, slices after, seal last); disclosure
   section gains the accepted size-inference line (`volume-format-v2.md` §2
   "Accepted disclosure") and the unit-boundary corollary; add the §3.3
   zero-strip procedure and the three-rung degradation ladder (sheet §3.4).
3. **RESTORE.sh v2** (`generate_restore_script`): modes per sheet §10 —
   `--info` (SEALED / UNSEALED / DAMAGED verdict via §2.5 precedence),
   **new `--verify`** (keyless chain walk), `--find-envelope`, `--restore`
   (front-index sizes/hashes cross-checked), rung-2 fallback (read map from
   the seal marker's embedded copy when File 3 fails to parse). Keep
   `require_uint` on every parsed integer; keep parsing line-oriented
   (grep/awk — the §3.1 grammar contract); tools only
   mt/dd/age/dar/sha256sum/head/truncate.
4. **PLAN.toml**: `generate_planning_header` content survives as an operator
   envelope tar member named `PLAN.toml` (fold ruling, format §8). Update
   `build_envelope_tar` callers (`src/volume/write.rs`) accordingly in T8.
5. Update the layout.rs unit tests: `restore_script_has_all_modes`
   (layout.rs:~1267) gains `--verify`; id-thunk test asserts v2 fields and the
   ABSENCE of v1 position fields.

### T5b — build + materialize + validate (`src/volume/layout_model.rs` + new code)
1. `ContentSource` → `Staged(PathBuf) | Materialized(PathBuf)` (no byte blobs).
2. `Layout::build(...)`: assemble entries in format order (0..3 front,
   permuted tenant envelopes from File 4, op env, op backup, slices
   unit-contiguous alphabetical, seal marker last); write every generated zone
   to the session staging dir; record exact sizes + sha256 for all entries
   (slices: verbatim from `stage_slices` — no re-read here); THEN generate the
   front index from the completed entry list (its own entry: size/hash None;
   seal marker entry: hash None) and materialize it; finally generate the seal
   marker with a **placeholder timestamp** for sizing only (§9 micro-decision —
   RFC 3339 UTC is fixed-width, so the sealed-at rewrite in T6 is
   byte-length-identical).
3. Envelope permutation (sheet §2.1): stable-sort tenant envelopes by
   hex(`sha256(volume_uuid_bytes ‖ 0x00 ‖ le64(tenant_id))`). Tenant envelopes
   only — operator envelope + backup keep fixed positions after them.
4. `validate`: keep the existing full-hash of staged slices (sacred invariant
   2); add: frozen-zone files exist and match recorded size+hash; capacity =
   Σ block-padded + enospc_buffer ≤ oracle (`Store::capacity`); keys + escrow
   per T2; generated zones parse (front index + seal marker TOML round-trip,
   RESTORE.sh `bash -n`).

Traps: generators run ONCE at build — resume must re-read the frozen files,
never regenerate (frozen-bytes rule; the ID thunk embeds `created_at`, so
regeneration would drift). `pad_to_blocks` stays the single padding function.
The front index NEVER contains its own size/hash; the seal marker's embedded
copy DOES contain File 3's size+hash and omits only its own (they are
different lists — see `generate_seal_marker`'s doc comment, layout.rs:~749).

**Verify:** `cargo test --lib volume::` — existing 25+ tests plus new ones:
permutation determinism (same uuid ⇒ same order, different uuid ⇒ different),
build-twice byte-identity of frozen zones, front-index exclusion rules.
**Done when:** green, clippy clean.

## T6 — The session (`src/volume/session.rs`, new)

**Goal:** the §9 typestate machine, exactly. Copy the flow block from
decision-sheet §9 into the module doc comment and implement it.

Steps:
1. Types: `BuiltLayout → ValidatedLayout → PlannedSession → ReadyToSeal →
   SessionEnd` with the §9 operations. Each phase's methods exist only on its
   type.
2. `plan`: insert `writes` 'planned' + `write_positions` 'pending' (slices
   only — `stage_slice_id` is NOT NULL by schema; metadata files get no rows).
3. `execute`: rewind; per entry — SIGINT check between entries
   (`crate::signal::is_interrupted()`, see current use in write.rs), stream
   the entry's file through the tee reader into `store.execute`; compare tee
   hash vs entry hash — mismatch ⇒ clean abort (unsealed, 'aborted', staging
   kept); ENOSPC from store ⇒ same abort. Slice entries update their cursor row
   ('written' + `sha256_on_volume`). All entries use `sync=false`.
4. `seal`: regenerate the seal marker with real `sealed_at`
   (assert `len == frozen placeholder len` — if that assert ever fires, the
   placeholder trick broke; stop), write with `sync=true`.
5. `confirm(tier)`: call `store.confirm`; record `verification_sessions`
   (`verify_type`: Integrity→'full', Navigable→'quick'); pass ⇒ ONE transaction
   flipping writes 'completed', snapshots 'current', volume 'sealed'; fail ⇒
   volume 'quarantined', session aborted, staging kept.
6. Resume: two-case cursor rule verbatim from `layout-session.md` (zero slices
   written ⇒ restart from BOT; else reposition to
   `front_zone_len + written_slices`); revalidate first; File-0 identity check
   (label+uuid) — mismatch ⇒ quarantine, never overwrite. Extend
   `recover_orphaned_sessions` (`src/db/mod.rs:54`) rather than duplicating
   the sweep.

Traps: default confirm tier is **Integrity** (`--quick` opts down — ratified);
seal is unreachable unless every entry executed (typestate should make this
unrepresentable); no code path outside `seal()` writes a `SealMarker` entry
(sacred invariant 1); staging GC must refuse to delete inputs of any
non-terminal session AND of sessions whose volume is not yet sealed across
**all planned copies** (sheet §3.5/§11 — touch `staging clean`'s guard).

**Verify:** unit tests over MemStore: full happy path ends Sealed with correct
DB rows; injected hash mismatch ⇒ AbortedUnsealed + no seal marker in
`MemStore.files`; ENOSPC (make MemStore fail at byte budget) ⇒ same; SIGINT
flag between entries ⇒ Interrupted + resumable. **Done when:** green.

## T7 — The synthetic-heir harness (sheet §4.1) — the acceptance suite

**Goal:** a keyless verifier proves the byte layout in `cargo test`, no tape.

New `tests/format_v2.rs` (declares `mod common;` for T3 fixtures): build a
microcosm batch (small N=6–10 units), run the full session against `MemStore`,
then from **the recorded bytes alone**:
- parse File 3; walk the full chain (seal → File 3 → every file), trimming
  padding via `size_bytes`;
- assert §2.5 self-consistency (positions strictly increasing from 0, exactly
  one `front_index` at 3, exactly one `seal_marker` last, `file_count`
  matches);
- assert the v2 order (envelopes precede slices; no `planning_header` type);
- permutation determinism across two builds; frozen-zone resume byte-identity;
- **fail-safe truncation**: drop the last recorded file ⇒ verdict UNSEALED
  (never sealed, never a crash);
- decrypt one tenant envelope with the fixture key, restore that unit's slices
  from recorded bytes, `assert` exact content match vs the generator (keyless
  path first, then keyed restore — both must work).

**Mutation smoke (do this once, manually):** flip one byte in one recorded
slice and confirm the harness FAILS with a hash mismatch; revert. If it stays
green, the harness is broken — fix the harness, not the code.

**Done when:** suite green, and the mutation smoke demonstrably fails.

## T8 — Flip `volume_write` + CLI

**Goal:** `volume_write` (`src/volume/write.rs:119`) becomes ~orchestration
only: gather staged batch (`find_staged_data`, ORDER BY u.name stays) → T5
build → validate → plan → execute → seal → confirm. Delete: `mini_index_tuples`,
the two-pass mini-index sizing, `generate_mini_index`, position arithmetic,
`ZoneKind::MiniIndex` + `ZoneKind::PlanningHeader` (and the layout-session zone
list already reflects this). MAM read + health collection stay (best-effort,
around the session). `volume verify` gains `--full|--quick` and becomes chain
walk consumer 2 (sheet §10) via the same `Store::confirm`. `volume_init` keeps
writing the provisional thunk (now v2). Update man pages.

Traps: keep `capacity_bytes`/MAM populate behavior; the old inline capacity
gate in write.rs is replaced by `validate`'s (do not leave both); config
`manifest_reserve` is removed in T10, not here — until then simply stop
reading it in the new path.

**Verify:** ungated `cargo test` green (unit + integration). The mhvtl e2e
suite is EXPECTED RED between T8 and T9 — this is the single allowed exception;
land T8 and T9 as consecutive commits without running the gate in between.
**Done when:** ungated green; `volume write` on mhvtl produces a v2 tape
(manual spot check: `mt rewind; dd | head` shows the v2 thunk).

## T9 — e2e updates (mhvtl legs to v2)

**Goal:** the gated suite proves the real tape matches the harness's model.

In `tests/mhvtl_e2e.rs` (helpers: `write_volume`:161, `read_tape_file_at`:278):
1. Round-trip legs: v2 positions (front index at 3, envelopes from 4, seal
   last); restore legs unchanged in intent.
2. **Leak scan v2** (`mhvtl_no_plaintext_tenant_metadata`:298): plaintext
   position set `{0,1,2,3,last}`; everything else must start with the age
   magic; needle set STRENGTHENED — sentinel tenant/unit names AND source
   filenames AND the expected `sha256_plain` hex digests must be absent from
   every plaintext file; `sha256_encrypted` values and sizes are permitted.
   Update the envelope-range sanity block for the v2 order.
3. **Parity leg** (sheet §10): write a good microcosm volume; run Rust
   `volume verify --full` AND extract File 2 and run `RESTORE.sh --verify`
   against the tape — both must PASS.
4. **Corruption leg:** hand-write a raw mini-volume via `TapeStore`
   (bypassing the session — the tri-layer would refuse) with one slice's bytes
   flipped relative to its front-index hash; Rust verify AND `RESTORE.sh
   --verify` must both FAIL on exactly that position.
5. Review `scripts/mhvtl-verify-gate.sh` EXPECTED_FAIL manifest — v2 changes
   none of the current 2 entries (H7 #33, H8 #34); if a gate leg parses v1
   layout positions, update it.

**Verify:** `TAPECTL_MHVTL=1 cargo test --test mhvtl_e2e -- --ignored
--nocapture` fully green; then `scripts/mhvtl-verify-gate.sh` green.
**Done when:** both green on the VM.

## T10 — Library + selector (sheet §11) + config cleanup

**Goal:** the §11 design, verbatim: `[[libraries]]` config block, `library
sync|status|plan` CLI (new `src/cli/library.rs` + `src/library/` module),
batch execution = stage once → session per copy → release staging per the GC
rule. Selector = alphabetical first-fit against `usable − enospc_buffer`
(no BFD — ruled out). Multi-tape drill test at microcosm scale (~600 units ⇒
2+ tapes; assert name-ordered spines + ≥99% fill net of padding).

Config cleanup (only now, when nothing reads them): remove `manifest_reserve`
(field, default fn, and the test fixtures that set it — serde ignores unknown
keys in existing user configs, so removal is safe); leave other decorative
keys for the R&D-exit sweep. Regen man pages.

**Done when:** drill test green; `library sync --dry-run` on a generated
microcosm library reports sensibly.

## T11 — Close out

1. Re-run everything: ungated suite, mhvtl suite, the gate.
2. Update `CLAUDE.md`'s current-state paragraph (v2 flip landed; Library
   exists; microcosm testing model) — nothing else in that file.
3. Report to the operator: what landed, test counts, any place you had to
   deviate from this plan and why. Do NOT merge to master, do NOT push, do NOT
   file issues — the operator decides R&D exit.

---

## Task DAG

```
T0 → T1 → T2 ─┐
      T3 ──────┼→ T4 → T5a → T5b → T6 → T7 → T8 → T9 → T10 → T11
              (T2 and T3 may interleave after T1)
```

If blocked >30 minutes on any single error, or if a design doc seems to demand
something impossible: stop, write down exactly what you tried, and ask the
operator. An honest stop beats an inventive workaround in this codebase —
wrong bytes on tape are forever.
