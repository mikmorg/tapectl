# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tapectl is a Rust CLI tool for managing long-term archival storage across LTO tape and exportable encrypted directories (Blu-ray, USB). It wraps `dar` for archive creation/extraction, uses the `age` crate (from the rage project) for age encryption, and SQLite for catalog/inventory/policy/audit.

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

Milestones 0 through 7 are complete (Milestone 7's last piece, real LTO-6 validation,
was done on 2026-09-10 — see below).

**Milestone 0:** Full round-trip validated — dar → age encrypt → mhvtl tape → read → decrypt → extract. All criteria passed. Validation programs in `validation/`.

**Milestone 1:** Working commands: `init`, `tenant` (add/list/info/delete), `key` (generate/list/export/import), `unit` (init/init-bulk/list/status/tag/rename/discover). Full SQLite schema deployed.

**Milestone 2:** Working commands: `snapshot create/list`, `stage create`, `staging status/clean`. Full pipeline: directory walk → manifest → sha256 validation → dar archive → age multi-recipient encryption → checksums → stage report. dar wrapper with version check, XML catalog parsing, catalog isolation.

**Milestone 3:** Working commands: `volume init/write/verify/identify`. Full 10-file volume layout written to tape via mhvtl: ID thunk, system guide, RESTORE.sh, planning header, encrypted data slices, mini-index, tenant envelopes, dual operator envelopes. Tape ioctl module with fixed block I/O. Verify reads back and validates sha256.

**Milestone 4:** Working commands: `restore unit/file` (read from tape → decrypt → dar extract), `catalog ls/search/locate/stats`. Full round-trip verified: write → restore → diff -r identical.

**Milestone 5:** Working commands: `location add/list/info/rename`, `volume move/retire/read-slices`, `cartridge register/list/info/mark-erased`, `unit mark-tape-only`, `snapshot diff/delete`, `stage list/info`, `export`, `db backup/fsck`. Volume retire shows impact analysis. mark-tape-only enforces min_copies/min_locations. read-slices reads encrypted slices from a volume into staging for writing to another tape via `volume write`.

**Milestone 6:** Working commands: `archive-set create/edit/list/info/sync`, `audit` (compliance check with exit codes 0/1/2, --action-plan, --json), `snapshot mark-reclaimable` (enforced preconditions, tape-only 2x multiplier), `volume compact-read/compact-write/compact-finish/compact`, `report summary/fire-risk/copies/tape-only/dirty/pending/verify-status/health/capacity/age/events/compaction-candidates`. Policy resolver: dotfile > archive_set > defaults.

**Post-M6 completions:** All unassigned CLI commands from design doc implemented: `key rotate`, `tenant reassign`, `snapshot purge`, `unit check-integrity`, `quick-archive`, `db export/import/stats`, `config show/check`. Zero compiler warnings, 17 tests (5 unit + 12 integration), zero clippy errors. No StubCommands remain.

**Milestone 7 (complete — its real-LTO-6 validation is below):** Phases 1–9 landed. Lib target (Phase 1), module unit tests (Phase 2), sg_logs health collection (Phase 3), full audit trail wiring (Phase 4), mhvtl-gated E2E round-trip (Phase 5), library failure-mode tests (Phase 6), multi-tenant isolation tests (Phase 7 — crypto cross-decrypt rejection, plaintext-leak scan on raw tape bytes, both-tenants self-restore, tape-device lock for parallel mhvtl tests), performance harness (Phase 8 — `tests/performance.rs` gated on `TAPECTL_PERF_TESTS=1`, baselines in `docs/perf-baselines.md`), docs + man pages (Phase 9 — README Testing/Documentation sections, `examples/gen_man.rs` + `docs/man/*.1` via clap_mangen, `docs/lto6-validation-checklist.md`, written as a procedure stub in Phase 9 but **no longer one** — it was fleshed out and dry-run annotated against mhvtl on 2026-07-20 (#8) and records the ENOSPC fidelity gap; read it as usable procedure).

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
- **The DR recipe is one command:** `init --operator <name> --escrow-public-key <original>`
  (#139). `--operator` is required under a system account such as the `tapectl`
  service user (uid below `UID_MIN`), where `init` refuses without it (#357).
  No command replaces a registered escrow identity (ADR-0005).
- **`audit` scopes per check** (`cli::audit::CHECKS`); tape-only units were
  invisible to every check from Milestone 6 until #138.
- **Byte pins:** `tests/on_tape_golden.rs` pins MANIFEST.toml and RESTORE.sh.
  If one fails, the on-tape format changed — a CTO decision, never an agent's re-pin.
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
what an agent finishes and what needs the operator's hands (the go-ahead for the
first production write, on hold by the CTO's word; the Heir Kit ceremony; cartridges
and locations). The real-drive rehearsal is done (2026-09-23,
`docs/runs/2026-09-23-real-drive-rehearsal.md`).

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
  removed; 40 checks since #355 added `empty_drive_refused`, GREEN 40/40 on 2026-09-29).

Issues #22–#28 (closed) describe the *pre-v2* design; read them against the normative
set above, never as instructions.

**Production runs on home2** (ADR-0012, 2026-09-28): the drive's hostdev is detached from
this VM; `contrib/hosts/home2.profile` and `home2-prep.sh` are the host's install.

**2026-09-29 — the documentation pass's rulings** (ADR-0012, "Amendment, 2026-09-29";
issues #345–#362, all landed — #360 last, as `volume::adopt_lost`, see below):
- `[defaults] min_copies_for_tape_only`/`min_locations_for_tape_only` are now
  `min_copies`/`min_locations`, same meaning; the old names are refused by name, and
  `first-run.sh` offers the rename in place before step 1.
- Migration 026 dropped the states nothing set (unit `retired`, snapshot
  `superseded`/`failed`, volume `blank`/`missing`); a row still carrying one fails the
  migration loudly, and `volumes.status` has no default.
- `volume verify` exits 0 passed / 2 medium proven bad (quarantined) / 3 inconclusive.
- Stage reports live in `<home>/stage-reports/` (an old `receipts/` is moved once);
  *Receipt* means only the recipient list (`stage_sets.key_fingerprints`).
- RESTORE.sh checks for `tar` up front; the golden pin moved under that ruling (#349).
- **#360, built (`volume::adopt_lost`):** `volume resume` adopts a volume the catalog lost
  mid-write (restored from a backup taken between `volume init` and `volume write`, then
  `catalog rebuild --from-volume`) only on a matching File 0 uuid, a seal marker binding
  the front index, a passing full verify recorded after the rebuild, and front-index slice
  hashes equal to the catalog's own staged hashes (`stage_sets.origin = 'staged'`);
  `catalog rebuild` still never changes a status (ADR-0012, 2026-09-29 later amendment).
  Still back up the catalog at the end of every write session.

**2026-09-30 — the structural review** (ADR-0012, "Amendment, 2026-09-30"): nothing in the
schema or the format had to change before the first tape; the code and catalog findings are
issues #373–#385, built after it. Before it, and pinned once (`tests/on_tape_golden.rs`, the
third re-pin): RESTORE.sh decrypts a unit into scratch space inside `--to` (or
`--scratch DIR`) after a space check, never into /tmp; names a full disk as one; streams
`--verify`; refuses a layout_version other than 2. The package version is `1.0.0` (it was
`0.1.0` on every commit before), tagged `v1.0.0`. Version rule (ruled 2026-09-30): the minor
moves when generated on-tape bytes change, the patch for any other build installed on a
production host; each such release is tagged `vX.Y.Z` on the commit it is built from. Units are never split by hand: if a large unit's
re-archiving ever costs too much, the answer is #12's differential-only shape, underneath
the unit. The interim rules that waited on #376/#377/#378 are lifted: a write session holds
a per-volume flock and the catalog has a busy policy (#376, #377), and the unit's own
`.tapectl-unit.toml` is not content (#378, ADR-0012 2026-10-07 item 24), so `unit tag`,
`unit rename` or a dotfile edit mints no version.

**Real LTO-6 hardware validation is DONE** (2026-09-10), no longer deferred: an HP
LTO-6 was passed through to this VM (`docs/lto6-drive-passthrough.md`) and was used
for a full validation session (`docs/lto6-session-journal-2026-09-10.md`). The §5
open hardware questions are answered there — block size 512 K vs 1 M is a wash, MAM
over-report is +2 MiB. `scripts/lifecycle-suite.sh` (16 scenarios, `--list` names
them, x an 11-method restore matrix) is the permutation suite built from it.

## Build Commands

Default to `check`/debug. Release builds (`--release`) are only for publishing or the
gated performance suite — don't produce release artifacts unless asked.

```bash
cargo check --all-targets     # fast compile verification (preferred while iterating)
cargo build                   # debug binary at target/debug/tapectl
cargo clippy --all-targets    # must stay warning-clean
cargo fmt --check
```

CI (`.github/workflows/ci.yml`) runs `fmt --check`, `clippy -D warnings`, `cargo test`
(with dar installed), a `docs/man` drift check and a non-blocking `cargo audit` — but only
on pushes and PRs to `master`. Run `clippy`, `fmt --check`, and `cargo test` locally
before committing; nothing checks a push to any other branch.

The crate is a **dual lib + bin target**: `src/main.rs` is a thin wrapper and all logic
lives in the `tapectl` library crate (`src/lib.rs`). Integration tests import `tapectl::`
directly, so keep command logic in library modules, not `main.rs`.

Regenerate both command references after any CLI (clap) change:

```bash
cargo run --example gen_man      # writes docs/man/*.1
cargo run --example gen_cli_md   # writes docs/cli/*.md (tests/cli_md_fresh.rs fails while stale)
```

User-facing docs are checked against the binary: `scripts/check-docs.py` (every
`tapectl …` line in a shell block must name real subcommands and flags; whole
`config.toml` examples must pass `config check`; `--self-test` is its positive
control). Run it with clippy/fmt before committing a docs or CLI change.

## Testing

Default `cargo test` runs unit + integration + tenant-isolation + failure-mode tests;
none need tape hardware or mhvtl.

**`dar` must be on `PATH` (issue #43).** The ungated suite is *not* hermetic: 49
tests need a working dar (counted 2026-09-29 by stubbing dar out) — the staging
pipeline, `stage create`/`quick-archive`/collection batches, the dar wrapper,
`config check`'s dar probe, the restore record, and three `cli_smoke` tests.
Without dar, plain `cargo test` runs the whole lib test binary first — dozens of
dar-dependent tests fail with cryptic panics — and then stops (cargo's fail-fast is
per test binary), never reaching the named check in `tests/test_dependencies.rs`; run
`cargo test --test test_dependencies` for its instructions. Its by-name list
(`DAR_DEPENDENT_TESTS`) names only 13 of the 49, so do not read it as complete. dar is
a hard *runtime* dependency anyway, so testing needs nothing extra. The ungated tests
that run dar resolve it as plain `"dar"` via `PATH` — never hardcode `/usr/bin/dar`,
which is wrong on any distro installing to `/usr/local/bin`. The two
`binary = "/usr/bin/dar"` fixtures in `tests/integration.rs` are inert only because
no test using them reaches dar; do not copy them.

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
# DEVICE NUMBERING IS NOT STABLE: the real LTO-6 may be lent to this VM (production
# owns it from home2 since 2026-09-28; ADR-0012), and a reboot can hand it /dev/nst0. Always set TAPECTL_GATE_TAPE. Discovery fails
# closed on a non-mhvtl device, so an unset value aborts rather than writing to the
# real drive — but do not rely on that. Check `ls -l /dev/tape/by-id/` after a boot:
# scsi-HUJ808A5L4-nst is the REAL drive; scsi-XYZZY_A* are mhvtl. The gate and
# mhvtl_e2e want an LTO-8 emulation (ULT3580-TD8, `lsscsi`) by its /dev/nstN
# spelling — since the 2026-09-26 reboot that is nst3/nst4 (A1/A2); nst1/nst2 are TD6
# and the gate's LTO-8 backend refuses their LTO-6 media.
TAPECTL_GATE_TAPE=/dev/nst3 TAPECTL_MHVTL=1 \
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
- **dar integration** (`src/dar/`): subprocess wrapper for create, restore and the version check; minimum dar 2.6.x
- **Staging** (`src/staging/`): sha256 validation before archiving, age multi-recipient encryption, ephemeral slices
- **Volume management** (`src/volume/`): Layout-v2 self-describing layout (`volume-format-v2.md`), the typestate write session (`session.rs`), Layout build/materialize (`build.rs`), front-index/seal/ID-thunk parsers (`format.rs`), `MANIFEST.toml` in both directions (`manifest.rs`), envelope read-back (`envelope.rs`), RESTORE.sh from named awk fragments (`restore_script.rs`), catalog rebuild (`rebuild.rs`), raw dump (`raw.rs`), verify, read-slices
- **Tape I/O** (`src/tape/`): kernel st driver via ioctl, fixed 512KB block mode
- **Crypto** (`src/crypto/`): age multi-recipient encryption, per-tenant key isolation
- **Policy** (`src/policy/`): 3-level resolver (dotfile > archive_set > defaults), advisory audit; `coverage.rs` owns the copy/location SQL and every `volumes.status` predicate, `escrow.rs` owns escrow coverage (verdict AND query) — never inline either again
- **Store trait** (`src/store.rs`): built (ADR-0006) — `capacity`/`execute`/`confirm`/`read_file`, streaming so RAM tracks block size not slice size. `TapeStore` and `MemStore` share one chain-walk implementation, so MemStore-based tests exercise the real confirm path. `WarehouseStore`/`ExportStore` stay ADR-0006 peers but are unbuilt and untracked: #72 (closed) rescoped warehouse copies to a documented rclone/aws-cli procedure over sealed volumes (`docs/design-errata.md`), #73 (closed) is the warehouse-location model, and no issue tracks an `ExportStore` — `export` does not go through the trait
- **Collection** (`src/collection/`): `[[collections]]` config → `collection sync|status|plan|run`; folder-per-unit registration, alphabetical first-fit batch selector, stage-once/write-N-copies/release

**Design principles:**
- Volumes are self-describing — full data restore without the database or tapectl, and catalog rebuild with the operator/escrow key; what that does and does not promise is defined in `docs/design/volume-format-v2.md` ("Self-describing — what that promises")
- Strict tenant isolation — zero content metadata in plaintext on tape; tenant envelopes use age trial-decryption
- Multi-tenant bin-packing on shared volumes
- Physical cartridges tracked separately from logical volumes (cartridges can be erased/reused)
- Policy audit is advisory, never blocking (exit codes: 0=clean, 1=warn, 2=violation)

## External Dependencies

- `dar` ≥2.6 (recommended 2.7.20+) — archive creation/extraction
- `sg3-utils` — drive health pages (`sg_logs`), the cartridge's MAM (`sg_read_attr`), drive identity (`sg_inq`)
- `mhvtl` — virtual tape library for development/testing
- `lsscsi`, `mt-st` — the binary never calls them (device discovery and debugging), but
  `scripts/first-run.sh` step 2 requires both, along with `acl` (`setfacl`) and `python3`
- the `age` and `tar` CLIs — not used by the binary (it uses the `age` and `tar`
  crates), but the on-tape RESTORE.sh (the heir path) refuses to run without them or `mt`

## Key Rust Dependencies

- `clap` 4.6 (derive), `rusqlite` 0.39 (bundled), `age`/`rage` 0.11 (pinned: pre-1.0 API unstable)
- `nix` 0.29 (ioctl/fs), `sha2` 0.10, `uuid` 1 (v4/v7)
- `thiserror` 2, `anyhow` 1, `chrono` 0.4, `walkdir` 2, `serde` 1

## Configuration

- System config: `~/.tapectl/config.toml` (dar path, backends, defaults incl. exclusions and copy policy, archive sets, staging, collections, host check); locations are catalog rows from `location add`, not config — a `[locations]` table is refused
- Database: `~/.tapectl/tapectl.db`
- Session logs: `~/.tapectl/logs/<UTC>-<command>-<pid>.log`, one per long operation
  (issue #386, `src/progress.rs`): phases, waits over 5 s, stalls, and the INFO tracing
  tee. Library code calls `progress::phase`/`wait`/`add_bytes` unconditionally — they
  are no-ops on a thread with no session; phase durations land in `phase_timings`
  (migration 028). Progress goes to stderr only; `--quiet` silences it.
- Per-unit config: `.tapectl-unit.toml` in each archival directory
