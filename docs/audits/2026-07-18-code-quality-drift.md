# Holistic Review: Code Quality & Design-Doc Drift

**Date:** 2026-07-18
**Ticket:** [Holistic review: code quality & design-doc drift](https://github.com/mikmorg/tapectl/issues/3) (wayfinder map [#1](https://github.com/mikmorg/tapectl/issues/1))
**Method:** four parallel auditors swept `src/` against `tapectl-design-v4_0.md`, one per subsystem group (pipeline core; physical layer; data/policy; shell/cross-cutting). Severity is weighted by the renovation intent statement: restore paths, data integrity, and self-describing/heir-recoverable properties heaviest; stranger-facing multi-tenant polish light.
**Verdict scope:** static review against the design doc. Findings are auditor-verified against source but not all are runtime-reproduced; triage should confirm each HIGH before remediation.

---

## Executive summary

The happy path is real: write → verify → restore → diff round-trips, the schema is a faithful copy of the design's SQL, the shell layer has clean error discipline, and the post-M7 fixes are genuinely in place. But the audit's central conclusion is uncomfortable:

> **The properties the intent statement weighs heaviest — heir-recoverable, self-describing, integrity-guaranteed — hold only on the happy path, and the happy path is the only path implemented.**

Totals: **13 unique HIGH** (14 raw; one duplicate independently found by two auditors), **~30 MED**, **~23 LOW**, plus four dead-seam inventories.

### Theme 1 — The emergency/heir path is broken today (5 HIGH)

- The on-tape mini-index is generated **before** envelope entries are pushed into the file index, so `RESTORE.sh --find-envelope/--restore` cannot trim block padding → age rejects every envelope → **emergency restore fails end-to-end** on every tape written so far.
- The tenant `RECOVERY.md` inside envelopes gives commands that don't work: no truncate-to-`encrypted_bytes` step, slice names that violate dar's `base.N.dar` convention.
- The export `RECOVERY.md`'s `sha256sum -c` recipe emits a format sha256sum rejects.
- `export` can interleave slices from multiple staged stage_sets into one directory with an arbitrary manifest version.
- The ID thunk's first instruction to a stranger (`dd bs=64k`) fails against 512KB-block tapes.

### Theme 2 — The unhappy path is unimplemented (4 HIGH)

Schema columns exist for all of it; code for none of it:

- **SIGINT mid-write is recorded as success** (both auditors independently): slice loop breaks, metadata still writes at positions computed for the full count, all writes marked `completed`, snapshots `current`, unwritten slices recorded at tape position 0. The designed `interrupted`/resume flow does not exist, and startup recovery converts any `interrupted` rows to `aborted` anyway.
- **No ENOSPC/end-of-tape recovery** — a full tape aborts mid-pipeline leaving a non-self-describing volume.
- **No wrong-tape check, no append** — `volume write` always rewinds and overwrites file 0; the DB keeps counting the destroyed copies as restorable.
- **Stage failure leaves permanent `staging` rows** + orphaned plaintext `.dar` slices; the `failed` status is never set.

### Theme 3 — Integrity promises not kept (2 HIGH)

- **Bitrot protection is not implemented at the commitment point**: staging never compares computed sha256 against the stored baseline, and `backfill_checksums` runs on *every* stage, overwriting the baseline with possibly-corrupt hashes — after which even `check-integrity` can't see the corruption.
- **Dirty detection is absent codebase-wide**: no `unit status --dirty`, no dirty guard on `mark-tape-only` (design requires `--force` past shown changes), `report dirty` admits in a comment it detects nothing.
- Related MEDs: `volume verify` records `full` while performing only the `quick` pass; `check-integrity` double-reports across snapshots and misses NEW files.

### Theme 4 — Works only at test scale (2 HIGH)

- Staging reads **entire dar slices into RAM** (plaintext + ciphertext simultaneously) with a shipped default slice size of **2400G**; validation buffers whole files to hash. Guaranteed OOM on real data.
- `list_slices` sorts dar slices **lexicographically**, so ≥10 slices mis-number (`.10.` before `.2.`) and restore rebuilds slices dar cannot assemble — while all per-slice checksums still pass. Prefix matching can also ingest leftover slices from failed stages of similarly-named units.
- Symlinks/FIFOs break staging outright (validation reads through links; FIFO hangs forever) — any real home directory fails.

### Theme 5 — The policy layer is stranded (0 HIGH, but systemic)

`stage create` bypasses `policy::resolve` entirely (defaults only); `units.archive_set_id` has no writer anywhere so resolver layer 2 is permanently NULL; per-unit exclusions are never passed to dar and never applied to the walk; ~10 config knobs parse and do nothing (`block_size`, `device_tape`, `large_file_warn_threshold`, `preserve_acls`, `units.encrypt`, …).

### Theme 6 — Destructive operations lack consent gates

`volume retire` prints "ZERO copies after retirement!" and retires anyway (silently in `--json`); global `--dry-run`/`--yes`/`--verbose` are parsed and ignored by every command; `db import` overwrites the live database unprompted; `mark-erased` skips lifecycle checks; a second `key rotate` strands a tenant with zero active keys, after which staging silently encrypts operator-only.

### Theme 7 — Doc and code have drifted in both directions

"8-file" overview labels vs the implemented (and elsewhere documented) 10-zone layout — the enumerated zone list is current, the "8" counts are stale. §2.29 specifies variable block mode; implementation standardized on fixed 512KB. The M2 claim "XML catalog parsing via quick-xml" describes a module (`dar/catalog_xml.rs`) that is entirely dead — the files catalog comes from tapectl's own walk. Missing commands vs the design tree: `volume append`, `volume calibrate`, `restore raw-volume`, `config set`; `db export` is a row-count stub.

### Consolidated HIGH list (deduplicated)

| # | Finding | Where | Subsystem |
|---|---------|-------|-----------|
| H1 | Mini-index omits envelope entries → RESTORE.sh emergency restore fails end-to-end | `volume/write.rs:288-291`, `volume/layout.rs:333-338` | physical |
| H2 | Tenant RECOVERY.md commands don't work (truncate step missing, wrong slice naming) | `volume/layout.rs:650-685` | physical |
| H3 | SIGINT mid-write → everything marked `completed`, positions poisoned, no resume | `volume/write.rs:243-246,334-370`; `signal.rs` | physical + shell |
| H4 | No ENOSPC/EOT recovery — full tape leaves non-self-describing volume | `volume/write.rs:267+`, `tape/ioctl.rs:148-172` | physical |
| H5 | `volume write` rewinds/overwrites file 0 — no wrong-tape check, no append, no status guard | `volume/write.rs:99-105,138-146` | physical |
| H6 | Bitrot comparison never happens; re-stage clobbers the sha256 baseline | `staging/validate.rs:39-67`, `staging/mod.rs:306-309,427-445` | pipeline |
| H7 | Symlinks/special files break or corrupt snapshot/stage | `staging/mod.rs:520-557`, `staging/validate.rs:42-56` | pipeline |
| H8 | Lexicographic slice sort mis-numbers ≥10 slices → unrestorable rebuilds; prefix-match ingests foreign slices | `dar/create.rs:99-109` | pipeline |
| H9 | Whole-slice/whole-file in-RAM with 2400G default → OOM at real scale | `staging/mod.rs:213-222`, `staging/validate.rs:48` | pipeline |
| H10 | Dirty detection absent; mark-tape-only greenlights deletion without the designed guard | `cli/unit.rs`, `cli/operations.rs:268-333` | pipeline + data |
| H11 | `export` interleaves slices from multiple staged stage_sets | `cli/operations.rs:346-407` | data |
| H12 | `volume retire` proceeds to zero copies with no gate (silent in `--json`) | `cli/operations.rs:229-259` | data |
| H13 | Second `key rotate` strands tenant keyless (non-transactional); staging then encrypts operator-only silently | `cli/key.rs:156-201`, `staging/mod.rs:191-194` | shell |

### Reading guide for triage

The full per-subsystem reports below carry every MED/LOW finding with file:line, design refs, and one-line remediations, plus dead-seam inventories (schema-only features, dead modules, decorative config). Known overlaps: H3 reported by two auditors; retire gate appears as data-HIGH + physical-LOW; `parse_size_to_bytes` silent-zero appears as shell-MED + pipeline-LOW; dirty detection spans pipeline-HIGH + two data-MEDs.

---

## Subsystem report: pipeline-core

### Assessment
The modules are clean, small, and well unit-tested at the parser/dotfile level, but they are shallow wrappers whose depth the design actually demands is missing: stage-time validation, policy resolution, and symlink/exclusion handling are stubs of what sections 2.12–2.15 specify. Two defects endanger the restore path outright (slice mis-numbering, whole-slice-in-RAM at a 2400G default), and the flagship integrity promise — sha256 bitrot detection at the archival commitment point — is not actually implemented. Test coverage validates the happy path only; every HIGH below is invisible under mhvtl-scale test data.

### Findings

#### [HIGH] Bitrot protection not implemented; re-stage clobbers the sha256 baseline
- Where: src/staging/validate.rs:39-67, src/staging/mod.rs:306-309,427-445
- Design ref: 2.13, 2.3
- What: `validate_source` checks only existence + size; the sha256 it computes is never compared against `manifest_entries.sha256` on re-stage, so same-size corruption is silently archived. Worse, `backfill_checksums` runs on every stage (design: first stage only), overwriting the baseline with the possibly-corrupt hashes so even `unit check-integrity` can't detect it afterward. New files added after snapshot are also undetected — dar archives them, diverging archive content from the files catalog and breaking the "two stage_sets of one snapshot are logically identical" invariant.
- Remediation: compare computed hashes to stored manifest sha256 when present (error on mismatch), gate backfill on first-stage, and diff walked file set against manifest for NEW/MISSING.

#### [HIGH] Symlinks and special files break or corrupt the snapshot/stage pipeline
- Where: src/staging/mod.rs:520-557, src/staging/validate.rs:42-56
- Design ref: 2.15
- What: `walk_directory` records symlinks as regular files with symlink-metadata size; `validate_source` then `fs::read`s through the link — so any symlink causes a size-mismatch staging failure (or hashes target content into the catalog), a broken symlink reports "source file missing", a symlinked dir errors, and a FIFO hangs staging forever. dar itself handles symlinks correctly per design, but tapectl's own manifest layer cannot stage any unit containing one.
- Remediation: detect symlinks in walk (store as symlink entries with target), skip/handle non-regular files in both walk and validation.

#### [HIGH] `list_slices` mis-numbers dar slices (lexicographic sort) and matches foreign archives by prefix
- Where: src/dar/create.rs:99-109
- Design ref: 2.3, 2.4 (stage_slice identity), restore path
- What: dar emits `base.1.dar … base.N.dar` unpadded; `slices.sort()` is lexicographic, so for ≥10 slices `base.10.dar` sorts before `base.2.dar` and `stage_slices.slice_number` diverges from dar's real slice numbers — restore rebuilds files as `restore.{slice_number}.dar` (src/volume/restore.rs:137), producing wrongly-numbered slices dar cannot extract, while per-slice checksums still verify. Also `starts_with(stem)` makes `X_v1` swallow leftover `X_v10.*.dar` slices from a failed stage.
- Remediation: match `^stem\.(\d+)\.dar$` exactly and sort/record by the parsed numeric slice index.

#### [HIGH] Whole-slice and whole-file in-RAM processing with a 2400G default slice size
- Where: src/staging/mod.rs:213-222 (fs::read slice + in-memory encrypt), src/staging/validate.rs:48 (fs::read whole file), src/config.rs:155 (default "2400G")
- Design ref: quality (2.12 scale)
- What: staging reads each dar slice fully into a Vec and holds plaintext + ciphertext simultaneously (~2x slice size RAM); with the shipped 2400G default that is a guaranteed OOM on any real unit, and validation likewise buffers entire (multi-GB media) files to hash them. Works only at mhvtl test scale.
- Remediation: stream both hashing (Read → Digest) and age encryption (reader→writer) instead of buffering.

#### [HIGH] Dirty detection absent codebase-wide; mark-tape-only's dirty guard unenforced
- Where: src/cli/unit.rs:54-58 (no `--dirty`), src/cli/operations.rs (no dirty check in `unit_mark_tape_only`), src/cli/report.rs:303 (report_dirty is snapshot-age only)
- Design ref: 2.13, 2.22
- What: `unit status --dirty` doesn't exist, `units.checksum_mode` is stored but never used for any scan, and `unit mark-tape-only` performs no dirty comparison — the design's guard ("if dirty, shows specific changes, requires --force") before greenlighting local-data deletion is missing, risking silent loss of un-archived changes.
- Remediation: implement checksum_mode-based manifest-vs-disk comparison and wire it into `unit status --dirty` and the mark-tape-only precondition.

#### [MED] Stage bypasses the 3-level policy resolver entirely
- Where: src/staging/mod.rs:124-125,157-161
- Design ref: 2.12, 2.19
- What: `stage_create` takes slice_size, compression, and preserve_* straight from `config.defaults`; `policy::resolve` (which correctly implements dotfile > archive_set > defaults, including dotfile slice_size) is only called by audit and mark-reclaimable — so per-unit/per-set slice size and compression are silently ignored at the one point they matter.
- Remediation: call `policy::resolve` in `stage_create` and use its slice_size/compression/preserve flags.

#### [MED] Per-unit exclusions never applied; exclusions absent from snapshot manifest
- Where: src/staging/mod.rs:157-158 (global only, `-P` always empty), src/staging/mod.rs:511-561 (walk has no excludes)
- Design ref: 2.14
- What: dotfile `[excludes] patterns` are written/read but never merged into the dar `-X` args, and the snapshot walk excludes nothing — so manifest/files rows include files dar won't archive: a changed `Thumbs.db` fails stage validation, and `catalog`/`restore file` can point at paths that don't exist on tape.
- Remediation: merge global + dotfile excludes, pass to dar, and apply the same filter in walk_directory and validate_source.

#### [MED] Designed re-staging flow is unreachable — `stage create` only accepts status='created' snapshots
- Where: src/cli/stage.rs:221-232
- Design ref: 2.3 (Re-staging)
- What: after `staging clean`, the snapshot's status is 'staged'/'current', so the query "status = 'created'" finds nothing and errors "run snapshot create first" — you cannot create a new stage_set to produce additional copies without minting a new snapshot version, contradicting the snapshot/stage_set model.
- Remediation: allow selecting an existing snapshot (e.g. `--version`) whose stage sets are cleaned and create a new stage_set for it.

#### [MED] Stage failure leaves stage_set stuck in 'staging', no transaction, orphaned .dar slices
- Where: src/staging/mod.rs:128-330
- Design ref: schema (stage_sets 'failed'), 2.3 failure modes
- What: the 'failed' status is never set anywhere; any error mid-pipeline (dar, encrypt, IO) returns early leaving a permanent 'staging' row, plaintext .dar slices on disk (which the prefix-matching bug above can later ingest), and partial slice rows — and none of the multi-statement DB work is wrapped in a transaction despite the post-M7 "critical DB ops in transactions" hardening claim.
- Remediation: wrap in a transaction or catch errors to set status='failed' and delete partial slice files.

#### [MED] `unit init --archive-set` never persisted or validated — archive_set policy layer unreachable
- Where: src/unit/mod.rs:60-91, src/db/queries.rs (insert_unit; no UPDATE of archive_set_id anywhere)
- Design ref: 2.2, 2.19
- What: the flag is written only to the dotfile; `units.archive_set_id` is never set by any code path and the set name isn't validated to exist, so resolver layer 2 is permanently NULL and audit/mark-reclaimable silently fall back to system defaults for every unit.
- Remediation: validate the archive set exists and store archive_set_id in insert_unit (and on discover).

#### [MED] dar flag drift: `-3 sha512` instead of `--hash sha256`, `--acl` absent, preserve_acls dead
- Where: src/dar/create.rs:44-55, src/staging/mod.rs:253-271
- Design ref: 6 (Archive Creation Flags), 2.13 rationale, 2.26
- What: code hashes slices with sha512 then staging deletes the `.sha512` files (wasted I/O, forecloses the design's "dar hash double-duty" optimization); the design's `--acl` flag is never passed and `DarCreateParams.preserve_acls` is `#[allow(dead_code)]` — the config knob is a silent no-op, with ACL preservation resting on an unverified comment that `-am` covers it (`-am` is dar's ordered-masks flag, not an EA switch).
- Remediation: switch to `--hash sha256` (or drop it), and either implement or remove preserve_acls with a verified EA/ACL flag set.

#### [MED] `dar -x` restore flags: `-O` mislabeled "overwrite", no overwrite policy, no non-root warning
- Where: src/dar/restore.rs:13-27
- Design ref: 2.26, restore path
- What: `-O` is dar's ignore-ownership option, not overwrite; with `-Q` and no `-w`, restoring into a non-empty directory can turn dar's overwrite question into a hard failure, and the design's "warns on non-root restore" is absent.
- Remediation: fix the comment, add an explicit overwrite policy flag, and emit the non-root ownership warning.

#### [MED] snapshot create omits designed guards: nesting check, large-file warning, empty-unit warning
- Where: src/staging/mod.rs:18-99, src/cli/snapshot.rs:99-129
- Design ref: 2.2, 2.3 Phase 1
- What: `snapshot_create` never calls `nesting::check_nesting` (design: "unit init and snapshot create check parent/child"), `large_file_warn_threshold` is parsed into config but referenced nowhere, and empty units produce no warning.
- Remediation: add the nesting check and the two warnings to snapshot_create.

#### [LOW] init-bulk: no collision suffixes, dead depth param
- Where: src/unit/mod.rs:97-129
- Design ref: 2.2
- What: name collisions are skipped as errors instead of suffixed as designed; `_depth` is accepted and ignored (hardcoded depth 1).
- Remediation: append a collision suffix on init-bulk; drop or implement depth.

#### [LOW] rename_unit silently swallows dotfile write failure
- Where: src/unit/mod.rs:156-164
- Design ref: 2.2 (dotfile self-registration)
- What: `let _ = write_dotfile(...)` — on failure the dotfile (which rides into every future archive for self-registering restore) keeps the old name with no operator signal.
- Remediation: at minimum warn on dotfile update failure.

#### [LOW] Partial clean marks stage_set 'cleaned' while leaving an unremovable orphan slice
- Where: src/staging/clean.rs:40-90
- Design ref: quality
- What: if one slice fails removal but a sibling succeeds, the set still becomes 'cleaned'; the failed slice keeps its staging_path but the re-clean query requires status='staged', so the file leaks permanently.
- Remediation: only mark a set 'cleaned' when all its slices were released.

#### [LOW] parse_size_to_bytes silently returns 0 on unparseable input
- Where: src/staging/mod.rs:493-508
- Design ref: quality
- What: a typo'd slice_size records 0 into stage_sets.slice_size and the policy resolver rather than erroring.
- Remediation: return Result and reject unparseable sizes.

#### [LOW] Filesystem nesting fallback only checks one level of children
- Where: src/unit/nesting.rs:39-50
- Design ref: 2.2
- What: the DB-out-of-sync fallback misses an existing unit two or more levels below the new path; comment asserts one level "is enough", which is only true for init-bulk-created layouts.
- Remediation: walk deeper (bounded) or note the DB check as authoritative.

#### [LOW] username/groupname never captured; has_xattrs/has_acls hardcoded 0
- Where: src/staging/mod.rs:86-87,555-556
- Design ref: 2.26, schema
- What: manifest_entries.username/groupname are always NULL and the has_xattrs/has_acls columns carry a comment "populated on stage" that no code fulfills; dar's own archive metadata is the only place names survive.
- Remediation: resolve uid/gid to names during walk or drop the columns/comment.

### Deferred/dead seams
- src/dar/catalog_xml.rs — entire module `#[allow(dead_code)]`, never called; files catalog is populated by walk_directory instead of dar's XML listing (M2 claim vs reality).
- `stage create --verify` (design 2.3 optional `dar -t`) — not implemented; `create::test_archive` / `restore::test` / `restore::extract_file` are uncalled.
- stage_sets 'failed' status — schema state never entered by any code path.
- config `large_file_warn_threshold` and `defaults.hash` — parsed, never read.
- `DarCreateParams.preserve_acls` — dead field behind `#[allow(dead_code)]`.
- Dotfile `[policy] slice_size` — resolver reads it via raw TOML, but UnitDotfile struct can't write it and staging never consults the resolver, so the design 2.12 level-1 source is doubly stranded.
- `units.encrypt` / policy `encrypt` — staging hardcodes `encrypted=1` and always encrypts (safe direction, but the knob is decorative).
- snapshots.snapshot_type ('differential'/'incremental') and base_snapshot_id — schema present, only 'full' ever created.
- units.archive_set_id — no writer exists anywhere; the archive-set-to-unit link is unpopulatable.
- init_bulk `depth` parameter — dead.

---

## Subsystem report: physical-layer

### Assessment
The happy-path write→verify→restore round trip is real, tested, and DB-consistent, and the post-M7 fixes (verification sessions, compact-read checksum error, compact-finish guard, restore trial-decryption) are genuinely in place. But the physical layer implements only the happy path: end-of-tape recovery, append, wrong-tape detection, interrupt/resume, and the entire MAM/capacity model exist solely as schema columns and design prose, and the two heaviest-weighted stranger-facing artifacts — the on-tape mini-index and the tenant RECOVERY.md — each have defects that break the no-database emergency restore path end-to-end. On the "8-file vs 10-file" question: design §1/§2.6 says "8-file/8-zone" while §2.21 says "10-file"; the §2.6/§8 *enumerated* zone list (ID thunk, guide, RESTORE.sh, planning header, data slices, mini-index, tenant envelopes, operator envelope, operator backup) is what the code implements exactly, so the enumeration is current and the "8" counts are stale doc labels.

### Findings

#### [HIGH] RESTORE.sh emergency restore broken: mini-index omits envelope entries
- Where: src/volume/write.rs:288-291 (and dead pushes at 312, 324, 329); consumer at src/volume/layout.rs:333-338
- Design ref: 8.6, 8.3
- What: `generate_mini_index` is called before the mini-index/tenant-envelope/operator-envelope entries are pushed into `file_index`, so the on-tape mini-index lists only files 0..N (the later pushes go nowhere). RESTORE.sh's `--find-envelope`/`--restore` look up envelope `size_bytes` in that list to truncate 512KB block padding; the lookup misses, the padded ciphertext is fed to `age`, which rejects trailing data (pipefail), so every envelope "fails" and emergency restore dies with "no envelope matched the provided key". `--info`'s file map is also incomplete.
- Remediation: Encrypt all envelopes first, then generate the mini-index covering every file (including itself and envelopes) before writing it; add a round-trip test that runs RESTORE.sh against a written tape image.

#### [HIGH] No end-of-tape ENOSPC recovery — full tape leaves a non-self-describing volume
- Where: src/volume/write.rs:267, 290, 311-328; src/tape/ioctl.rs:148-172
- Design ref: 2.9, Appendix C (R1-R3)
- What: Every tape write uses `?` propagation; ENOSPC during a slice or during metadata aborts the whole pipeline, so the tape ends with no mini-index or envelopes and writes stuck `in_progress`. The layered recovery (stop slices → write metadata; overwrite incomplete slice; sacrifice last slice) is entirely absent — `writes.eot_recovery`, `sacrificed_slice_id`, and the `sacrificed` position status exist in schema only.
- Remediation: Catch ENOSPC in the slice loop and metadata writes and implement the three-layer recovery from Appendix C, recording `eot_recovery`/`sacrificed_slice_id`.

#### [HIGH] volume write always rewinds and overwrites from file 0 — no append, no wrong-tape check, no status guard
- Where: src/volume/write.rs:99-105, 138-146
- Design ref: 2.6 (append), Appendix C steps 2/5, Appendix D
- What: `volume_write` looks up the label in the DB, opens the device, rewinds, and writes File 0 — it never reads the loaded tape's ID thunk to confirm it is the right cartridge, never checks volume status, and never seeks EOM to append. Writing to an already-active volume (or with the wrong tape loaded) silently destroys existing data while the DB still records those write_positions as restorable copies.
- Remediation: Before writing, read File 0 and verify label match; refuse (or implement Appendix D append via MTEOM) when the volume already has completed writes.

#### [HIGH] SIGINT mid-write marks everything completed and poisons envelopes with position 0
- Where: src/volume/write.rs:243-246, 334-370, 1052, 1082
- Design ref: 2.24, Appendix C (I1-I4, C.3)
- What: On interrupt the slice loop breaks, then the code still writes envelopes and marks ALL writes `completed` and snapshots `current` — including units whose slices were never written; `build_manifest_units` records `tape_position` 0 (`unwrap_or((0,0))`) for unwritten slices, so on-tape manifests point at the ID thunk. The specified `interrupted` status and resume flow don't exist, and `UNIQUE(stage_set_id, volume_id)` makes a retry to the same volume fail with a raw constraint error.
- Remediation: On interrupt, exclude unwritten slices from manifests, mark those writes `interrupted` (never `completed`), and implement the C.3 resume path (or at least a clean retry).

#### [HIGH] Tenant RECOVERY.md instructions do not work as written
- Where: src/volume/layout.rs:650-685
- Design ref: 8.7
- What: The literal commands omit the truncate-to-`encrypted_bytes` step (age rejects block-padded input — the system guide itself warns about this), name slices `slice_N.dar` which violates dar's required `base.N.dar` convention so `dar -x` can't assemble them, and use an `ARCHIVE_BASE` placeholder; the spec's summary table, sha256 verification commands, and troubleshooting section are absent. This is the heir-facing document inside the envelope.
- Remediation: Rewrite generate_recovery_md to emit truncate+sha256sum steps and dar-conventional slice names (`restore.N.dar`, `dar -x restore`), plus the required sections.

#### [MED] Capacity model and MAM integration entirely absent
- Where: src/volume/write.rs:67, 166-185; src/cli/volume.rs:261-272; src/cli/cartridge.rs:60-79
- Design ref: 2.8, 2.5, Appendix C steps 3-4
- What: `sg_read_attr` is never called anywhere; ID thunk always writes `mam_capacity_bytes = 0` and empty `[media]` fields; `volumes.mam_capacity_bytes`/`mam_remaining_at_start` are never populated; cartridge register has no MAM auto-read; there is no pre-write capacity check, no `manifest_reserve`/`enospc_buffer` in `volume plan`, and no `volume calibrate` command.
- Remediation: Add an sg_read_attr wrapper beside health.rs, populate MAM fields at write/register time, and gate volume_write on the §2.8 availability formula.

#### [MED] Planning header emits invalid TOML for more than one unit
- Where: src/volume/layout.rs:548-574
- Design ref: 8.4
- What: `[[units]]` is written once before the loop; each subsequent unit's `name/uuid/...` keys land in the same table, producing duplicate-key invalid TOML for any multi-unit write. The unit test only does `contains()` checks so it can't catch this.
- Remediation: Emit `[[units]]` per unit inside the loop and add a `toml::Value` parse test with two units.

#### [MED] Verification records 'full' but never decrypts; no quick/full selection
- Where: src/volume/write.rs:444-449, 456-475; src/cli/volume.rs:148-166
- Design ref: 2.18
- What: `volume_verify` inserts `verify_type = 'full'` but only does read+checksum — that is the design's `quick` type; the read+decrypt+plaintext-checksum `full` type doesn't exist and the CLI offers no type flag, so the audit trail overstates verification strength.
- Remediation: Either record `'quick'` honestly or add a decrypt pass and a `--type full|quick` flag.

#### [MED] Envelopes missing dar catalogs and manifest fields; operator envelope missing catalog.db
- Where: src/volume/write.rs:1097-1123; src/volume/layout.rs:616-647
- Design ref: 8.7, 8.8
- What: Envelope tars contain only MANIFEST.toml + RECOVERY.md — no `catalogs/` dar catalogs (so selective restore without the DB is impossible), MANIFEST lacks `dar_command`, stage_set IDs, and the `units.files` listing; the operator envelope lacks the portable SQLite `catalog.db` subset entirely.
- Remediation: Add dar catalog files and the missing manifest fields to tenant envelopes, and the filtered catalog.db to operator envelopes.

#### [MED] cartridge_volumes is never populated — cartridge lifecycle is decorative
- Where: src/volume/write.rs:908-913; src/cli/cartridge.rs:84-96, 140-151
- Design ref: 2.5
- What: No code path ever INSERTs into `cartridge_volumes`, so cartridges are never bound to volumes: compact_finish's `pending_erase` update is always a no-op, cartridge list/info always show empty volume history, and the available→in_use→pending_erase lifecycle is never driven.
- Remediation: Bind cartridge↔volume at `volume init`/first write (barcode prompt or MAM serial) and transition cartridge status on write/retire.

#### [MED] System guide is a stub relative to the Appendix A template
- Where: src/volume/layout.rs:90-159
- Design ref: 8.2, Appendix A
- What: The doc provides a complete 11-section template (multi-tape recovery, key management, dar/age format details, layout version history); the implementation is a ~65-line condensed guide missing multi-tape recovery and most of the LLM-orchestration detail the design considers essential. The essentials that are present (tools, block padding, manual steps) are accurate.
- Remediation: Import the Appendix A template as the `const &str` the design specifies, substituting only tape-specific values.

#### [MED] ID-thunk read instructions (`dd bs=64k`) fail against 512KB-block tapes
- Where: src/volume/layout.rs:39-46
- Design ref: 8.1, 2.29
- What: The first command shown to a stranger (`dd if=/dev/nst0 bs=64k > GUIDE.md`) fails on this tape: EINVAL in fixed 512KB mode, ENOMEM in variable mode with 512KB blocks. Related drift: design §2.29 specifies variable block mode (`MTSETBLK 0`) while the implementation standardized on fixed 512KB (guide/RESTORE.sh reflect this; the thunk template does not, and the design doc is stale).
- Remediation: Change thunk instructions to `mt setblk 524288` + `dd bs=512k` (or `bs=1M` in variable mode), and reconcile §2.29 in the doc.

#### [MED] Hardware compression never disabled
- Where: src/tape/ioctl.rs (absent); src/volume/write.rs:138
- Design ref: 2.8, Appendix C step 1
- What: Design says hardware compression MUST be disabled for encrypted (incompressible) data; no MTCOMPRESSION/mode-page call exists anywhere in the open/write path.
- Remediation: Issue MTCOMPRESSION 0 (or sg mode select) in `TapeDevice::open`.

#### [LOW] Tenant envelopes written in tenant_id order, not shuffled
- Where: src/volume/write.rs:295
- Design ref: 2.6, 8.7
- What: `unique_tenants` is sorted/deduped, so envelope position correlates with tenant ID, contradicting the shuffled-random-order requirement (multi-tenant polish, weighted light).
- Remediation: Shuffle the envelope write order per tape.

#### [LOW] volume identify reads ID thunk only
- Where: src/volume/write.rs:539-546
- Design ref: CLI §5 ("reads ID + planning header")
- What: Identify never decrypts and shows the planning header, which is its stated purpose for operator quick identification.
- Remediation: Add optional planning-header decrypt with operator keys.

#### [LOW] Per-write receipt not generated
- Where: src/volume/write.rs:372-398 (absent); receipts exist only at stage time (src/staging/mod.rs:311-319)
- Design ref: 2.27, Appendix C step 10
- What: Design requires a receipt per write (`{date}_{write_id}.txt`); volume_write writes none.
- Remediation: Emit a write receipt after commit.

#### [LOW] Cartridge list interpolates status filter into SQL
- Where: src/cli/cartridge.rs:82-88
- Design ref: quality
- What: `format!("... WHERE c.status = '{st}'")` instead of a bound parameter — breaks on quotes and is inconsistent with the rest of the codebase's parameterized style.
- Remediation: Use `params![st]`.

#### [LOW] mark-erased skips lifecycle checks
- Where: src/cli/cartridge.rs:181-200
- Design ref: 2.5
- What: Any cartridge can be marked `available` regardless of prior status; the bound volume is not required to be retired and is never transitioned to `erased`.
- Remediation: Require `pending_erase` (or --force) and set the volume's status to `erased`.

#### [LOW] tape_alerts collected but never persisted
- Where: src/tape/health.rs:115-131
- Design ref: quality
- What: Page 0x2e alert counts are parsed into `HealthCounters.tape_alerts` but the `health_logs` INSERT drops the field.
- Remediation: Add a tape_alerts column/field to the insert.

#### [LOW] Restore temp dir inside destination can leak decrypted slices on failure
- Where: src/volume/restore.rs:54, 155-164
- Design ref: quality
- What: `.tapectl-restore-tmp` under the destination is only cleaned on success; a checksum/dar failure mid-restore leaves decrypted dar slices behind.
- Remediation: Use a guard/tempdir that cleans on drop.

#### [LOW] retire prints impact then retires unconditionally; json mode retires silently
- Where: src/cli/operations.rs:182-268 (dispatched from src/cli/volume.rs:182-184)
- Design ref: 2.23
- What: Impact analysis exists and flags zero-copy units, but retirement proceeds in the same invocation with no confirmation even when data drops to zero copies, and the promised remediation commands are only a generic sentence. Consistent with "never block the operator" but with no deliberate-consent step.
- Remediation: Require `--force` (or a prompt) when any unit drops to zero copies; print concrete remediation commands.

### Deferred/dead seams
- Backend trait (`src/backend/`) — deferred per CLAUDE.md; tape I/O is direct via src/tape/, acknowledged.
- `TapeDevice::seek_eom` / `get_position` / `TapePosition` (ioctl.rs:121-144) — no callers; the append/resume machinery they were built for was never implemented.
- `writes.eot_recovery`, `writes.sacrificed_slice_id`, `write_positions` status `'sacrificed'`, writes statuses `'interrupted'`/`'aborted'` — schema-only, never written by any code path.
- `volumes.mam_capacity_bytes` / `mam_remaining_at_start` (models.rs:152-153) — never populated; no sg_read_attr wrapper exists.
- Variable-block branch of `TapeDevice::write_data` (ioctl.rs:150-160) — unreachable; all callers pass fixed 512KB.
- `file_index` pushes after mini-index generation (write.rs:291, 312, 324, 329) — dead writes; the vec is never read again (this dead seam is the mechanism of the HIGH mini-index bug).
- `find_staged_data` per-set write_id pre-lookup (write.rs:991-997) — always overwritten by the caller at write.rs:126-128.
- `RestoreReport.success` — `#[allow(dead_code)]`, always true.
- `volume plan --copies` — naive multiplier; no real bin-packing across volumes exists anywhere.

---

## Subsystem report: data-policy

### Assessment
The deployed schema is a near-verbatim, faithful copy of the design's Section 4 SQL (plus a sensible FTS5 addition for catalog search), and the 3-level policy resolver and reclaimable-gating preconditions match spec closely with good unit-test coverage. The weak spots are in the operational layer: `export` and `volume retire` have genuine restore/data-integrity defects, the audit command implements only 4 of the 6 specified checks, and the "full audit trail" claim has holes (snapshot current/superseded transitions and several mutations are unlogged or logged without old/new values). Code quality is workmanlike but repetitive, with tuple-based queries duplicating hand-rolled row mappers and the `models.rs` module largely dead.

### Findings

#### [HIGH] export mixes slices from multiple staged stage_sets
- Where: src/cli/operations.rs:346-407
- Design ref: 2.4 (stage_set identity), 2.7
- What: The export query selects slices from ALL stage_sets with status 'staged' for the unit, ordered only by slice_number. If two versions (or a re-stage) are staged simultaneously, the export directory and MANIFEST.toml interleave slices from different dar runs with duplicate slice numbers, and `snapshot_version` is taken from `slices[0].4` arbitrarily. An heir following RECOVERY.md ("decrypt all *.dar.age, dar -x on the common prefix") gets a corrupted/ambiguous restore.
- Remediation: Select a single stage_set (latest staged for a chosen snapshot version) and record its id/version in the manifest.

#### [HIGH] volume retire proceeds unconditionally even when units drop to zero copies
- Where: src/cli/operations.rs:229-259
- Design ref: 2.23, "never lose data silently"
- What: Impact analysis prints "WARNING: N unit(s) will have ZERO copies after retirement!" and then immediately executes the retirement in the same call — no confirmation prompt, no `--force` gate, and in `--json` mode the retire happens with no acknowledgment at all. Retired volumes feed the cartridge-erase flow, so this is one silent step from real data loss. (Contrast: `compact-finish` at volume/write.rs:900 correctly refuses.)
- Remediation: Require confirmation or `--force` when any unit's other_copies == 0; also align cartridge status transition with compact-finish.

#### [MED] snapshot_delete 6-statement cascade is not transactional
- Where: src/cli/operations.rs:513-533
- Design ref: quality (contradicts post-M7 "fix 2.2" claim)
- What: stage_slices/stage_sets/manifest_entries/manifests/files/snapshots are deleted with six separate `conn.execute` calls; a mid-sequence failure leaves a half-deleted snapshot (e.g., snapshot row present with files gone). `snapshot_purge` (line 38) got the `unchecked_transaction` treatment; delete did not.
- Remediation: Wrap the cascade plus the event in one transaction.

#### [MED] audit implements 4 of 6 specified checks — encryption compliance and dirty status missing
- Where: src/cli/audit.rs:27-178
- Design ref: 2.20
- What: Spec lists "copy count, location presence, verification age, encryption compliance, dirty status, compaction candidates". Implemented: copy count, location presence, verify age, a no_archive check, and compaction candidates. No check that `unit.encrypt`/stage_sets match resolved `policy.encrypt`, and no dirty check.
- Remediation: Add encryption-compliance (unit/stage_set vs resolved policy) and dirty checks to the audit loop.

#### [MED] mark-tape-only missing the dirty check required by spec
- Where: src/cli/operations.rs:268-333 (dispatched from cli/unit.rs:319 with no extra checks)
- Design ref: 2.22
- What: Spec: "If dirty, shows specific changes, requires `--force`." Implementation enforces only min_copies/min_locations; a unit with un-snapshotted changes can be marked tape-only (and its source deleted) without any warning.
- Remediation: Run the dirty scan before marking and require `--force` if changes exist.

#### [MED] snapshot current/superseded transitions are not logged to events
- Where: src/volume/write.rs:336-368 (state change owned by data layer's audit-trail contract)
- Design ref: 2.25
- What: `volume write` completion flips snapshots to 'current' (and predecessor to 'superseded') inside the tx but logs only a volume-level `write_completed` event; the snapshot status changes — the single most important lifecycle transition — leave no per-entity old/new event. staged→ and purge/reclaimable transitions ARE logged, so the trail is inconsistent.
- Remediation: Emit `log_field_change` for each snapshot status transition inside the write-completion transaction.

#### [MED] RECOVERY.md checksum-verification recipe is broken
- Where: src/cli/operations.rs:429-434
- Design ref: restore-path (heir-facing) / quality
- What: The generated `sha256sum -c` pipeline uses `awk '{print $1, " ", $2}'`, producing hash + three spaces + filename; sha256sum requires exactly two spaces (or " *"), so the documented verification step fails for the very person the file exists to help. The grep/paste pairing is also order-fragile.
- Remediation: Emit a plain `SHA256SUMS` file (`<hash>  <filename>` lines) at export time and have RECOVERY.md say `sha256sum -c SHA256SUMS`.

#### [MED] check-integrity scans all snapshots un-deduplicated; NEW detection missing; whole-file reads
- Where: src/cli/operations.rs:89-147
- Design ref: 2.13
- What: The query pulls checksummed files across every snapshot in ('current','staged','created') and iterates the flat list, so a file legitimately changed between versions is reported both BITROT (old sha) and OK (new sha). The spec's "NEW" category (on-disk files absent from catalog) is unimplemented, and `fs::read` loads entire files into RAM — untenable for the design's 100G-file use case.
- Remediation: Restrict to the latest checksummed snapshot, add a disk-walk for NEW, and stream the hash.

#### [MED] catalog locate omits physical location and includes retired/erased volumes indistinguishably
- Where: src/cli/catalog.rs:177-209
- Design ref: Section 4 "Key Queries" ("Where are all copies of this unit?")
- What: The doc's worked query joins locations and returns `l.name`; the implementation drops location entirely and applies no volume-status filter or column — an heir is sent to tapes that may be retired/erased/missing with no way to tell, and no idea where any tape physically is.
- Remediation: Add `v.status` and `LEFT JOIN locations` columns to locate output (and flag non-active volumes).

#### [MED] startup recovery converts 'interrupted' writes to 'aborted', defeating resume
- Where: src/db/mod.rs:54-77
- Design ref: 2.24 vs Section 4 "Recovery" (design self-conflict)
- What: 2.24 specifies SIGINT marks a write `interrupted` and it resumes on next `volume write`; but every `db::open` blanket-updates all `in_progress`/`interrupted` writes to `aborted`, so an interrupted write can never survive to be resumed. The code follows Section 4's recovery wording, which contradicts 2.24.
- Remediation: Only abort truly orphaned `in_progress` writes; leave `interrupted` intact for resume (and surface the design conflict).

#### [MED] report dirty does no dirty detection and silently ignores --unit
- Where: src/cli/report.rs:303-337
- Design ref: 2.20 / quality
- What: The function admits in a comment it cannot detect dirtiness and just lists snapshot ages for all units; the `_unit_filter` parameter is accepted from the CLI but never applied, so `report dirty --unit X` returns everything.
- Remediation: Apply the unit filter and either implement scan-based dirtiness or rename/re-document the report.

#### [MED] archive-set edit/sync: non-transactional multi-update and thin event logging
- Where: src/cli/archive_set.rs:169-232, 407-425
- Design ref: 2.25
- What: Edit issues up to 8 separate UPDATEs (partial-apply on failure) followed by a single "edited" event with no field/old/new values; the sync update path overwrites all policy fields (including nulling DB-set values absent from config) and logs no event at all.
- Remediation: Wrap edit in a transaction and log per-field old/new; log sync updates.

#### [LOW] policy resolver silently swallows DB and dotfile errors
- Where: src/policy/mod.rs:49, 125-144
- Design ref: 2.19 / quality
- What: `if let Ok(...)` on the archive_set query and unchecked dotfile read/parse mean a DB error or corrupt dotfile silently degrades to weaker defaults with no warning; also `min_copies` default is seeded from `min_copies_for_tape_only`, conflating two distinct knobs.
- Remediation: Log a warning on resolution-layer read/parse failures; document or separate the min_copies default.

#### [LOW] migration UX drift: no backup prompt, stale meta.schema_version
- Where: src/db/mod.rs:47-51; src/db/migrations/001_initial.sql:8
- Design ref: Section 4 Versioning
- What: Design says "Prompts for DB backup before applying" migrations — `open()` migrates silently; and `meta.schema_version` is inserted as '1' but never bumped by migration 002 (rusqlite_migration tracks user_version separately), so the design's stated version key is stale.
- Remediation: Prompt/backup before to_latest and update meta.schema_version per migration.

#### [LOW] catalog polish: slice-of-sha panic risk, search semantics mislabeled
- Where: src/cli/catalog.rs:105, 126-134
- Design ref: quality
- What: `&s[..12]` panics if a stored sha256 is ever shorter than 12 chars; `catalog search` help says "substring match" but the FTS5 implementation is token-prefix match (mid-word substrings return nothing).
- Remediation: Use `s.get(..12)` and correct the help text.

#### [LOW] copy-count semantics inconsistent across commands
- Where: src/cli/operations.rs:283 (COUNT DISTINCT w.id) vs src/cli/audit.rs:32 (COUNT DISTINCT w.volume_id)
- Design ref: Section 4 Key Queries
- What: mark-tape-only counts distinct writes (matches the doc's worked SQL) while audit counts distinct volumes; two writes of different stage_sets on the same volume would count as 2 copies for tape-only enforcement but 1 for audit.
- Remediation: Standardize on distinct volume_id (the safer measure) everywhere.

#### [LOW] fire-risk/summary reports use coarse or over-broad aggregates
- Where: src/cli/report.rs:159-174, 120-124
- Design ref: quality
- What: fire-risk uses the global `min_copies_for_tape_only` instead of per-unit resolved policy (audit does it correctly), with a redundant `OR copies = 0`; summary's "total data on tape" sums bytes_written across all volumes including retired/erased ones.
- Remediation: Reuse `policy::resolve` in fire-risk; filter summary to active/full volumes.

#### [LOW] db fsck truncates integrity output and repairs without audit events
- Where: src/cli/operations.rs:806-849
- Design ref: 2.25 / quality
- What: `PRAGMA integrity_check` can return multiple rows but only the first is read; `--repair` deletes orphaned writes/stage_slices (records of what's on tape) with no transaction and no events row.
- Remediation: Collect all integrity rows and log repair deletions as events inside a transaction.

### Deferred/dead seams
- src/db/models.rs — module-wide `#[allow(dead_code)]`; Tag/StageSet/StageSlice/Volume/Write/VerificationSession/Event structs are mostly unused since CLI queries return ad-hoc tuples.
- src/db/queries.rs:23-111 — Tenant/EncryptionKey/Unit row mappers copy-pasted 3-4x each; no shared `from_row` helpers.
- archive_sets.preserve_xattrs/preserve_acls/preserve_fsa/dirty_on_metadata_change columns — resolver reads them (policy/mod.rs:64-67) but no CLI path (create/edit/sync) can ever set them; permanently NULL.
- report_dirty `_unit_filter` parameter — accepted from clap, never used (see MED finding).
- tags add/remove (queries.rs:428-444) — mutating, no event emission; minor 2.25 gap folded here.
- Backend trait (src/backend/) — deferred per CLAUDE.md; tape I/O direct, not touched by data layer.

---

## Subsystem report: shell-crosscutting

### Assessment
The shell layer is structurally clean — thiserror in the library, anyhow only at the main boundary, runtime unwrap discipline is good (the heavy unwrap counts are almost entirely inside `#[cfg(test)]` modules), and the config struct matches the design's section 7 spec key-for-key. However, two heavy-weight defects exist: the signal-handling spec (2.24) is only half-implemented and an interrupted write is recorded as `completed`, and `key rotate` is a one-shot command whose second invocation strands a tenant with zero active keys non-transactionally. The advertised global flags `--dry-run`/`--yes`/`--verbose` are parsed and then ignored everywhere.

### Findings

#### [HIGH] Interrupted write marked 'completed'; no resume, positions corrupted
- Where: src/volume/write.rs:243-246 and 333-347 (signal seam owned by src/signal.rs)
- Design ref: 2.24 Signal Handling
- What: On SIGINT the slice loop `break`s, then execution falls through and writes the mini-index/envelopes at positions precomputed for the *full* slice count (physical vs recorded positions diverge, breaking the self-describing invariant on that tape), then marks **all** writes `'completed'` and snapshots `'current'` inside the final transaction. Design requires: mark write `interrupted`, exit, resume on next `volume write` — none of that exists; `TapectlError::Interrupted` is never constructed.
- Remediation: On interrupt, skip index/envelope writing, mark unfinished writes `'interrupted'` (leave snapshots alone), and make `volume_write` resume from `write_positions`.

#### [HIGH] Second `key rotate` strands tenant with zero active keys; staging then silently drops tenant recipient
- Where: src/cli/key.rs:156-201 (with src/crypto/keys.rs:138-142, src/staging/mod.rs:191-194)
- Design ref: 2.16 Encryption, 2.1 Tenants
- What: Rotate deactivates all active keys (key.rs:159) *before* generating replacements with hardcoded aliases `rotated-primary`/`rotated-backup`; a second rotation hits `KeyAlreadyExists` (files from the first rotation exist) and errors out with the deactivation already committed — not in a transaction. Afterward `stage create` gathers tenant active keys (empty) + operator keys with no empty-check, so new slices are encrypted operator-only and the tenant cannot decrypt their own data.
- Remediation: Use timestamped/serial aliases, wrap deactivate+insert in one transaction, and make staging refuse to encrypt when a tenant has zero active keys.

#### [MED] Global --dry-run/--yes/--verbose parsed but never honored; destructive ops unprompted
- Where: src/cli/mod.rs:27-37; src/main.rs (never reads `cli.dry_run`/`cli.yes`/`cli.verbose`)
- Design ref: 5. CLI Interface (Global Flags)
- What: Grep confirms zero consumers of the three globals; the only dry-run is a local flag on `restore unit`, and the only prompt in the codebase is the compact-flow destination prompt (cli/volume.rs:332). `tenant delete`, `snapshot purge`, `volume retire`, `cartridge mark-erased`, `staging clean`, and `db import` (which overwrites the live database) all execute immediately with no confirmation, making `--yes` vacuous and `--dry-run` silently unsafe.
- Remediation: Thread a context struct with the globals into `cli::*::run` and gate destructive ops on confirm-or---yes, honoring --dry-run.

#### [MED] Missing commands vs design tree; `db export` is a stub
- Where: src/cli/volume.rs (enum), src/cli/restore.rs:11-49, src/cli/mod.rs:238-244, src/main.rs:200-220
- Design ref: 5. CLI Interface (Subcommands)
- What: Absent: `volume append`, `volume calibrate`, `restore raw-volume` (restore-path weight), and `config set/add/remove` (only show/check exist). `db export` prints seven table row-counts despite the "Export database as JSON" contract. Extra undocumented top-level `import`; `quick-archive` is non-interactive vs the doc's "interactive flow" and hardcodes 512KB blocks. (`restore dry-run` survives as a flag; `unit integrity`→`check-integrity` is a benign rename.)
- Remediation: Implement or explicitly de-scope append/calibrate/raw-volume/config-set in the doc; make `db export` dump real rows.

#### [MED] `parse_size_to_bytes` silently returns 0 on malformed input feeding capacity records
- Where: src/staging/mod.rs:493-505; consumed at src/volume/write.rs:46, src/main.rs:112, src/staging/mod.rs:131
- Design ref: 2.8 Capacity Model / quality
- What: `num_str.parse().unwrap_or(0.0)` means a typo'd `nominal_capacity` or `import --capacity` yields a 0-byte-capacity volume row that feeds capacity math and bin-packing with no error.
- Remediation: Return `Result` and reject unparseable sizes at config-load/CLI boundary.

#### [MED] Config keys parsed but ignored: block_size, device_tape/device_sg, manifest_reserve, enospc_buffer, hardware_compression
- Where: src/config.rs:76-106 vs src/cli/volume.rs:8 (`DEFAULT_BLOCK_SIZE: 512*1024` hardcoded), CLI `--device` defaults hardcoded to "/dev/nst0"
- Design ref: 7. Configuration, 2.29 LTO Drive Access
- What: Five backend keys deserialize and are never read; block size is a hardcoded 512KB constant (doc's config default is "1M", and 2.29 specifies variable block mode `MTSETBLK 0`), and tape device paths ignore `device_tape` in favor of clap defaults.
- Remediation: Wire backend config into device/block-size selection or delete the dead keys from the spec/struct.

#### [MED] `volume write` panics via unwrap when no LTO backend configured (the default after `init`)
- Where: src/volume/write.rs:165 (`config.backends.lto.first().unwrap()`)
- Design ref: quality
- What: `init` writes a default config with an empty `backends.lto` vec, so a fresh install running `volume write` panics; `volume_init` 120 lines earlier handles the same case correctly with `ok_or_else(... "no LTO backend configured")` (write.rs:40-44).
- Remediation: Replace the unwrap with the same `ok_or_else` guard.

#### [LOW] `key export` missing `--qr` paper-backup flag
- Where: src/cli/key.rs:37-41
- Design ref: 2.16
- What: Doc specifies `tapectl key export ALIAS [--qr]` for paper backup; only plain stdout public-key export exists.
- Remediation: Add `--qr` or drop it from the doc.

#### [LOW] `--config PATH` hijacks the entire home directory
- Where: src/main.rs:21-31
- Design ref: 5. Global Flags
- What: The flag derives home (DB, keys, staging receipts) from the config file's parent directory, so pointing at an alternate config file silently relocates the database and keys; undocumented coupling.
- Remediation: Separate `--config` from a `--home`/TAPECTL_HOME concept.

#### [LOW] error.rs blanket `#[allow(dead_code)]` hides dead variants and unused exit-code constants
- Where: src/error.rs:6-13, 87, 99-103
- Design ref: 5 (exit codes) / quality
- What: `Interrupted`, `NotInitialized`, `AlreadyInitialized` are never constructed (main.rs bails with ad-hoc anyhow strings instead); `EXIT_SUCCESS`/`EXIT_WARNING` are unused — audit uses literal codes. `exit_with_error` mapping every failure to 2 is spec-compliant.
- Remediation: Remove the blanket allow, delete or wire up dead variants, and have audit use the constants.

#### [LOW] Command bodies inlined in main.rs; repeated CLI boilerplate
- Where: src/main.rs:102-288; all files in src/cli/
- Design ref: 9. Rust Crate Structure / quality
- What: Import/QuickArchive/Db/Config (~190 lines including raw SQL) live in main.rs while every other command has a cli module; the json-vs-table print pattern and `query_row(...).map_err(TapectlError::Other("not found"))` lookup pattern are copy-pasted across all 17 cli files (location.rs's JSON list output also drops the description field its table shows).
- Remediation: Extract main.rs bodies into cli modules and add small output/lookup helpers.

### Deferred/dead seams
- src/backend/, src/cartridge/, src/verify/ — three empty directories, not declared in lib.rs (backend trait explicitly deferred per design 2.29 escape hatch; the other two are stale scaffolding)
- signal::clear_interrupted (src/signal.rs:20) — dead, `#[allow(dead_code)]`
- Config::load_or_default (src/config.rs:336) — dead, `#[allow(dead_code)]`
- tenant::require_tenant_by_id (src/tenant/mod.rs:75) — dead-allowed
- EXIT_SUCCESS / EXIT_WARNING (src/error.rs:7-9) — unused constants; audit exit codes are literals
- S3 backend config — intentionally commented-out in design section 7 and correctly absent from the Config struct
