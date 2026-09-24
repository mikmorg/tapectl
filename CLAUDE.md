# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tapectl is a Rust CLI tool for managing long-term archival storage across LTO tape and exportable encrypted directories (Blu-ray, USB). It wraps `dar` for archive creation/extraction, uses the `rage` crate for age encryption, and SQLite for catalog/inventory/policy/audit.

**The on-tape format is Layout Version 2** (ADR-0007). For anything touching tape
bytes or the write path, the normative set is, in authority order:
`docs/design/volume-format-v2.md` (the byte format) → `docs/design/layout-session.md`
(session state machine) → `docs/design/v2-open-questions.md` (resolved decisions,
§§1–11) → `docs/design/v2-implementation-plan.md` (the T0–T11 build playbook).
`docs/adr/0001`–`0007` govern all of them.

`tapectl-design-v4_0.md` remains the reference for everything those do NOT cover,
read **together with `docs/design-errata.md`** (the complete list of
superseded/recast sections — note §2.6/§8.1–§8.8 are superseded wholesale by v2,
and §2.9/Appendix C end-of-tape salvage is rejected outright). `CONTEXT.md` is the
vocabulary.

## Current State

Milestones 0 through 5 are complete.

**Milestone 0:** Full round-trip validated — dar → age encrypt → mhvtl tape → read → decrypt → extract. All criteria passed. Validation programs in `validation/`.

**Milestone 1:** Working commands: `init`, `tenant` (add/list/info/delete), `key` (generate/list/export/import), `unit` (init/init-bulk/list/status/tag/rename/discover). Full SQLite schema deployed.

**Milestone 2:** Working commands: `snapshot create/list`, `stage create`, `staging status/clean`. Full pipeline: directory walk → manifest → sha256 validation → dar archive → age multi-recipient encryption → checksums → receipt. dar wrapper with version check, XML catalog parsing, catalog isolation.

**Milestone 3:** Working commands: `volume init/write/verify/identify`. Full 10-file volume layout written to tape via mhvtl: ID thunk, system guide, RESTORE.sh, planning header, encrypted data slices, mini-index, tenant envelopes, dual operator envelopes. Tape ioctl module with fixed block I/O. Verify reads back and validates sha256.

**Milestone 4:** Working commands: `restore unit/file` (read from tape → decrypt → dar extract), `catalog ls/search/locate/stats`. Full round-trip verified: write → restore → diff -r identical.

**Milestone 5:** Working commands: `location add/list/info/rename`, `volume move/retire/read-slices`, `cartridge register/list/info/mark-erased`, `unit mark-tape-only`, `snapshot diff/delete`, `stage list/info`, `export`, `db backup/fsck`. Volume retire shows impact analysis. mark-tape-only enforces min_copies/min_locations. read-slices reads encrypted slices from a volume into staging for writing to another tape via `volume write`.

**Milestone 6:** Working commands: `archive-set create/edit/list/info/sync`, `audit` (compliance check with exit codes 0/1/2, --action-plan, --json), `snapshot mark-reclaimable` (enforced preconditions, tape-only 2x multiplier), `volume compact-read/compact-write/compact-finish/compact`, `report summary/fire-risk/copies/tape-only/dirty/pending/verify-status/health/capacity/age/events/compaction-candidates`. Policy resolver: dotfile > archive_set > defaults.

**Post-M6 completions:** All unassigned CLI commands from design doc implemented: `key rotate`, `tenant reassign`, `snapshot purge`, `unit check-integrity`, `quick-archive`, `db export/import/stats`, `config show/check`. Zero compiler warnings, 17 tests (5 unit + 12 integration), zero clippy errors. No StubCommands remain.

**Milestone 7 (software-side complete):** Phases 1–9 landed. Lib target (Phase 1), module unit tests (Phase 2), sg_logs health collection (Phase 3), full audit trail wiring (Phase 4), mhvtl-gated E2E round-trip (Phase 5), library failure-mode tests (Phase 6), multi-tenant isolation tests (Phase 7 — crypto cross-decrypt rejection, plaintext-leak scan on raw tape bytes, both-tenants self-restore, tape-device lock for parallel mhvtl tests), performance harness (Phase 8 — `tests/performance.rs` gated on `TAPECTL_PERF_TESTS=1`, baselines in `docs/perf-baselines.md`), docs + man pages (Phase 9 — README Testing/Documentation sections, `examples/gen_man.rs` + `docs/man/*.1` via clap_mangen, `docs/lto6-validation-checklist.md`, written as a procedure stub in Phase 9 but **no longer one** — it was fleshed out and dry-run annotated against mhvtl on 2026-07-20 (#8) and records the ENOSPC fidelity gap; read it as usable procedure).

**2026-09-11 — disaster recovery and the architecture review (complete).** Three
review rounds on `catalog rebuild --from-volume` (#136) and what building it
exposed, then seven deepenings, all landed and real-drive-validated (four
45/45 passes on the HP LTO-6 that day and the next). What to know:
- **The catalog is reconstructible from tape** with the operator or escrow key
  (`volume::rebuild`, `volume::envelope`); tapes written after 2026-09-11 carry
  tenant ownership and each stage set's escrow receipt in the operator
  envelope's `catalog.db` (`db::ontape_catalog`, shape-probed, no stamp).
- **Escrow coverage has three answers** — covered / unknown / gap —
  through one predicate and one query (`policy::escrow`); a rebuilt row is
  `unknown` (`stage_sets.origin`), attestable by `catalog rebuild --key <escrow>`
  decrypting one slice header. `audit` names an escrow-identity mismatch once.
- **The DR recipe is one command:** `init --escrow-public-key <original>` (#139).
  No command replaces a registered escrow identity (ADR-0005).
- **`audit` scopes per check** (`cli::audit::CHECKS`); tape-only units were
  invisible to every check from Milestone 6 until #138.
- **Byte pins:** `tests/on_tape_golden.rs` pins MANIFEST.toml and RESTORE.sh.
  If one fails, the on-tape format changed — a CTO decision, never a re-pin.
  `MANIFEST.toml` is one type both directions (`volume::manifest`); RESTORE.sh
  is assembled from named awk fragments (`volume::restore_script`).
- The full account: `docs/runs/2026-09-11-unattended.md`,
  `docs/audits/2026-09-11-rebuild-findings-review.md`; the process rules that
  outlived the run: `.claude/skills/unattended-run/SKILL.md`.

**2026-09-13 — the media-generation redesign (complete).** The last redesign before
first production use, triggered by "how do I write both LTO-5 and LTO-6 from this
LTO-6 drive". The answer exposed that a property of the *cartridge* was being read
from the *drive's* config, and an audit for that same shape found more. Two ADRs
govern the result:
- **ADR-0010 — generation is a cartridge property.** A drive declares only the one
  generation it is (`[[backends.lto]].generation`); `media_type`/`nominal_capacity`
  are gone from config and a stale key is refused by name. The medium's generation is
  DETECTED at `volume init` (`src/media.rs` tables + `src/tape/media_detect.rs`: MAM
  medium density code → MAM format code → the st driver's density register), and
  capacity follows it, overridden by the cartridge row then by the drive's
  `capacity_override` (virtual drives only). It is decided ONCE at init and stored in
  `volumes.capacity_bytes`; **no path reads capacity from config after init**. A drive
  that cannot write the detected medium refuses, and `--force` never overrides it.
  `volume init` also BINDS the cartridge (`src/volume/binding.rs`) from the MAM medium
  serial, auto-registering one when nothing matches — which is why
  `cartridge_volumes`, written only by tests until now, is live.
- **ADR-0011 — a cartridge's place is a location, not a status.** `offsite` left the
  status CHECK (migration 012 rebuilds the table to four states); `cartridge move` and
  `volume move` share one mover so the shelf and the catalog cannot drift.
  `cartridge retire` writes `retired_permanent` under an ADR-0008 Tier-2 gate and
  retires the volumes on it; `volume retire` frees the cartridge it was the last live
  volume on.
- **Binding records a displacement, it never gates one** — when the serial proves which
  cartridge is in the drive. Re-initialising a cartridge closes the open mount, marks the
  displaced volume `erased`, and warns naming any unit left without a copy. The File 0
  check is the consent point (ADR-0003) and a second gate was deliberately rejected. The
  mhvtl gate proves it: four volumes initialised on one cartridge in one run, no
  `--force`, three left `erased`. The one carve-out (ADR-0012): with no readable serial,
  a blank tape plus a `--cartridge` bound to a live volume is refused.
- **2026-09-14 — ADR-0012, the pre-production rulings.** The review's questions were
  grilled and ratified in full: a *Copy* is identical content, counted per Version (a
  unit is as covered as its least-covered live version, and a version is minted only
  when content changed); a cartridge's identity is its chip serial, the barcode a
  relabelable sticker; the retire family's zero floor is absolute (ADR-0008 Tier 3, the
  code had it inverted); cartridge capacities are decimal, data sizes binary; unknown
  config keys are errors everywhere; `volume calibrate` is not built. The work queue is
  the GitHub label `review-2026-09-13`; nothing ships to a production tape until it is
  empty, including documentation.
- `--device` no longer defaults to `/dev/nst0` anywhere. Write paths resolve it
  strictly (`config::resolve_lto_backend`), read paths leniently
  (`config::resolve_device`) so DR still works with keys and no `backend add`.
- The audit that came with it is recorded in `docs/design-errata.md` (four design
  promises no code keeps) and issues #142-#152.

**Handoff:** `docs/handoff.md` is the current division of remaining work into
what an agent finishes and what needs the operator's hands (the Heir Kit
ceremony, the LTO-6 session, the first production write).

**Post-M7 hardening (complete):** Design gap audit identified 3 active bugs + 6 unacknowledged gaps. All 8 items fixed: clone-slices restructured to staging-only read-slices (self-describing invariant preserved), restore trial-decrypts with all tenant+operator keys (key rotation no longer breaks restore), compact-finish refuses retirement if live slices lack copies elsewhere, volume_verify records verification_sessions (audit feedback loop closed), staging cleanup reports actual bytes freed, compact-read errors on checksum mismatch, critical DB operations wrapped in transactions, export writes MANIFEST.toml + RECOVERY.md. RESTORE.sh fleshed out from stub to full emergency recovery script (--info, --find-envelope, --restore modes with sha256 verification and block-padding trimming). 106 tests (60 unit + 46 integration/lib/isolation/failure-mode), zero clippy warnings.

**Renovation (2026-07):** a full renovation stage, charted as a wayfinder map at
[issue #1](https://github.com/mikmorg/tapectl/issues/1) with a phased backlog in
issues #20–#73. The three audits under `docs/audits/` found the happy path solid but
the heir/emergency and unhappy paths broken. The milestone claims above describe
happy-path completeness only — treat them accordingly.

**Format v2 regear (complete, 2026-07-28).** Holistic R&D
(`docs/research/2026-07-21-ontape-format-and-write-design.md`) established that a
plan-first, write-once medium should not imitate a streaming format. The whole
write path was rebuilt to Layout v2 and landed as playbook tasks T0–T10:

- **On tape:** a plaintext **front index** (File 3) carrying every file's
  position/type/size + ciphertext sha256; **envelopes before slices**; a trailing
  plaintext **seal marker** binding the front index. End-of-tape salvage is gone —
  a real EOT is a clean abort to an unsealed tape, and the pre-flight capacity gate
  is the sole capacity defense.
- **In code:** a typestate **write session** (`src/volume/session.rs`:
  build → validate → plan → execute → seal → confirm), a **Store trait** with a
  shared chain walk (`src/store.rs`), Layout build/materialize (`src/volume/build.rs`),
  format parsers (`src/volume/format.rs`), and a **Collection** layer
  (`src/collection/`, `collection sync|status|plan|run`) for folder-per-unit
  archiving (CONTEXT.md: "Collection" is the source-root concept; "library" is
  reserved for the tape library / changer).
- **Verified:** 270 ungated tests; `tests/format_v2.rs` is a keyless synthetic-heir
  acceptance suite (proves the byte layout from recorded bytes alone); mhvtl e2e 9/9
  on real tape including Rust-vs-bash chain-walk parity on both a good and a
  corrupted tape; `scripts/mhvtl-verify-gate.sh` GREEN **against an empty
  EXPECTED_FAIL manifest** (26/26 on 2026-09-10 — H7 #33 / H8 #34 are fixed and
  removed; 39 checks as of #301).

**Next:** issues #22–#28 describe the *pre-v2* design and must be read against the
normative set above, not implemented literally.

**Real LTO-6 hardware validation is DONE** (2026-09-10), no longer deferred: an HP
LTO-6 is passed through to this VM (`docs/lto6-drive-passthrough.md`) and was used
for a full validation session (`docs/lto6-session-journal-2026-09-10.md`). The §5
open hardware questions are answered there — block size 512 K vs 1 M is a wash, MAM
over-report is +2 MiB. `scripts/lifecycle-suite.sh` (13 scenarios x a 10-method
restore matrix) is the permutation suite built from it.

## Build Commands

Default to `check`/debug. Release builds (`--release`) are only for publishing or the
gated performance suite — don't produce release artifacts unless asked.

```bash
cargo check --all-targets     # fast compile verification (preferred while iterating)
cargo build                   # debug binary at target/debug/tapectl
cargo clippy --all-targets    # must stay warning-clean
cargo fmt --check
```

There is no CI; run `clippy`, `fmt --check`, and `cargo test` locally before committing.

The crate is a **dual lib + bin target**: `src/main.rs` is a thin wrapper and all logic
lives in the `tapectl` library crate (`src/lib.rs`). Integration tests import `tapectl::`
directly, so keep command logic in library modules, not `main.rs`.

Regenerate man pages after any CLI (clap) change:

```bash
cargo run --example gen_man   # writes docs/man/*.1
```

## Testing

Default `cargo test` runs unit + integration + tenant-isolation + failure-mode tests;
none need tape hardware or mhvtl.

**`dar` must be on `PATH` (issue #43).** The ungated suite is *not* hermetic: 13
tests shell out to a real dar — the staging-pipeline regression guards plus the
restore-collision test. `tests/test_dependencies.rs` asserts this once, by name,
so a missing dar fails with instructions instead of a dozen cryptic `dar -c
failed` panics. It is a hard *runtime* dependency anyway, so testing needs
nothing extra. Fixtures resolve it as plain `"dar"` via `PATH` — never hardcode
`/usr/bin/dar`, which is wrong on any distro installing to `/usr/local/bin`.

```bash
cargo test                              # everything ungated
cargo test --lib                        # unit tests only (in-module)
cargo test --test integration           # one integration file
cargo test test_volume_write_positions  # a single test by name (substring match)
```

Two suites are gated (they skip at runtime unless the env var is set):

```bash
# mhvtl end-to-end round-trip + on-tape tenant isolation. Tests are #[ignore], so
# pass --ignored.
#
# DEVICE NUMBERING IS NOT STABLE: this VM also has a real LTO-6 passed through, and
# a reboot can hand it /dev/nst0. Always set TAPECTL_GATE_TAPE. Discovery fails
# closed on a non-mhvtl device, so an unset value aborts rather than writing to the
# real drive — but do not rely on that. Check `ls -l /dev/tape/by-id/` after a boot:
# scsi-HUJ808A5L4-nst is the REAL drive; scsi-XYZZY_A* are mhvtl.
TAPECTL_GATE_TAPE=/dev/nst1 TAPECTL_MHVTL=1 \
    cargo test --test mhvtl_e2e -- --ignored --nocapture

# Performance scenarios (thousands of files, large archives); ~2 min. This is the one
# case a release build is expected.
TAPECTL_PERF_TESTS=1 cargo test --test performance --release -- \
    --ignored --nocapture --test-threads=1
```

## Architecture

**Three-phase pipeline:** `snapshot create` (fast metadata) → `stage create` (dar + sha256 + encrypt) → `volume write` (tape I/O)

**Key subsystems:**
- **CLI layer** (`src/cli/`): clap derive-based subcommands (tenant, unit, snapshot, stage, volume, catalog, restore, audit, etc.)
- **Database** (`src/db/`): SQLite with WAL mode, forward-only numbered migrations, full audit trail; `ontape_catalog.rs` is the operator envelope's `catalog.db` — schema, generation probe, read and write in one place
- **Unit management** (`src/unit/`): archival entities tracked via `.tapectl-unit.toml` dotfiles in each directory
- **dar integration** (`src/dar/`): subprocess wrapper; minimum dar 2.6.x; XML catalog parsing via quick-xml
- **Staging** (`src/staging/`): sha256 validation before archiving, age multi-recipient encryption, ephemeral slices
- **Volume management** (`src/volume/`): Layout-v2 self-describing layout (`volume-format-v2.md`), the typestate write session (`session.rs`), Layout build/materialize (`build.rs`), front-index/seal/ID-thunk parsers (`format.rs`), `MANIFEST.toml` in both directions (`manifest.rs`), envelope read-back (`envelope.rs`), RESTORE.sh from named awk fragments (`restore_script.rs`), catalog rebuild (`rebuild.rs`), raw dump (`raw.rs`), verify, read-slices
- **Tape I/O** (`src/tape/`): kernel st driver via ioctl, fixed 512KB block mode
- **Crypto** (`src/crypto/`): age multi-recipient encryption, per-tenant key isolation
- **Policy** (`src/policy/`): 3-level resolver (dotfile > archive_set > defaults), advisory audit; `coverage.rs` owns the copy/location SQL and every `volumes.status` predicate, `escrow.rs` owns escrow coverage (verdict AND query) — never inline either again
- **Store trait** (`src/store.rs`): built (ADR-0006) — `capacity`/`execute`/`confirm`/`read_file`, streaming so RAM tracks block size not slice size. `TapeStore` and `MemStore` share one chain-walk implementation, so MemStore-based tests exercise the real confirm path. `WarehouseStore`/`ExportStore` are the remaining peers (#72/#73)
- **Collection** (`src/collection/`): `[[collections]]` config → `collection sync|status|plan|run`; folder-per-unit registration, alphabetical first-fit batch selector, stage-once/write-N-copies/release

**Design principles:**
- Volumes are self-describing — full data restore without the database or tapectl, and catalog rebuild with the operator/escrow key; what that does and does not promise is defined in `docs/design/volume-format-v2.md` ("Self-describing — what that promises")
- Strict tenant isolation — zero content metadata in plaintext on tape; tenant envelopes use age trial-decryption
- Multi-tenant bin-packing on shared volumes
- Physical cartridges tracked separately from logical volumes (cartridges can be erased/reused)
- Policy audit is advisory, never blocking (exit codes: 0=clean, 1=warn, 2=violation)

## External Dependencies

- `dar` ≥2.6 (recommended 2.7.20+) — archive creation/extraction
- `sg3-utils` — drive health diagnostics
- `mhvtl` — virtual tape library for development/testing
- `lsscsi`, `mt-st` — optional device discovery and debugging

## Key Rust Dependencies

- `clap` 4.6 (derive), `rusqlite` 0.39 (bundled), `age`/`rage` 0.11 (pinned: pre-1.0 API unstable)
- `quick-xml` 0.39, `nix` 0.29 (ioctl/fs), `sha2` 0.10, `uuid` 1 (v4/v7)
- `thiserror` 2, `anyhow` 1, `chrono` 0.4, `walkdir` 2, `serde` 1

## Configuration

- System config: `~/.tapectl/config.toml` (dar path, backends, locations, defaults, exclusions, policy)
- Database: `~/.tapectl/tapectl.db`
- Per-unit config: `.tapectl-unit.toml` in each archival directory
