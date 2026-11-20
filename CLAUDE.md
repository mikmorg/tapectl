# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**Coordinating the tapectl 2.0.0 build (Layout Version 3)? Start at
`docs/coordinator-handoff.md`**: what is ruled, the order of work, who settles what, and which
older texts no longer hold. **The go was given on 2026-11-17 (ADR-0012 item 292): the build is
under way**, stage by stage on each stage's "Starts when" line. The tasks
are in `docs/design/v3-implementation-plan.md`.

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

## Where the history is, and the rules it left

What each milestone, release and ruling did is recorded, not repeated here:
- `docs/coordinator-handoff.md`: the 2.0.0 build (what is ruled, the order of work, who
  settles what, which older texts no longer hold); the tasks are in
  `docs/design/v3-implementation-plan.md`.
- `docs/adr/0012-*.md`: every CTO ruling as a numbered item (a later item amends an earlier
  one). `docs/releases/<version>.md`: what each release changed, for the operator.
- `docs/runs/`: dated run records, never edited. `docs/audits/` and `docs/research/`: the
  reviews and measurements. `docs/handoff.md`: the operator's steps on home2.

Rules that outlived the history:
- **Production is home2** (`contrib/hosts/home2.profile`); its drive is not on this VM. The
  tapes L6-0001 and L6-0002 and home2's catalog are test articles (ADR-0012 items 235-238).
- **A byte pin** (`tests/on_tape_golden.rs`) that fails means the on-tape format changed: a
  CTO decision, never an agent's re-pin, and a pin moves only in a commit of its own.
- **Never argue a design choice from what `keepsake` holds** (item 281), and never bend the
  format to home2's older tools (item 273). Weigh a format choice by what one slice gives
  the stock tools alone, with no file table.
- **Prove a RESTORE.sh change** in the `tapectl-u26` container (`/scratch/tapectl-u26`) as
  uid 1000, and in a PR's CI: Ubuntu 26.04's uutils hang on `tail -c 1 <fifo` and enlarge
  pipes to 1 MiB where GNU coreutils do not (item 184), and root hides it.
- **Version rule** (item 201): the minor moves when generated on-tape bytes change or a
  command or flag spelling is removed, the patch for any other build installed on a
  production host; a change to scripts alone is no release. Each release is tagged
  `vX.Y.Z` on the commit it is built from.
- **Units are never split by hand** (#12's differential shape is the answer). Back up the
  catalog at the end of every write session.
- **Capacity is decided once, at `volume init`,** from the detected medium, and no path
  reads it from config after (ADR-0010). `--device` has no default.
- Real LTO-6 validation is done (2026-09-10, `docs/lto6-session-journal-2026-09-10.md`);
  `scripts/lifecycle-suite.sh` is the permutation suite built from it.

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

**Budget suites (issue #416)** pin what a path COSTS, not what it returns — every
production slowdown passed a correctness suite. All ungated:

```bash
cargo test --lib cost_budget        # tape motions per read path, FakeTape at 150-slice scale
cargo test --test catalog_budget    # statements and full-table scans per catalog command
cargo test --test pass_budget       # sha256 bytes and staging reads per write / stage create
```

A budget pin moves only on purpose, with the reason in the commit message; a pin that
fails is a cost change to explain, never a number to refresh. `pass_budget` reads
process-wide counters (`src/pass_count.rs`, test-only by ADR-0012 2026-10-12 item 45),
so it stays its own test binary; production hashing goes through
`pass_count::Sha256`, never `sha2::Sha256` directly, or its passes drop out of the count
(the budgets' 0.95 floor catches that).

Four suites are gated (they skip at runtime unless the env var is set):

```bash
# mhvtl end-to-end round-trip + on-tape tenant isolation. Tests are #[ignore], so
# pass --ignored.
#
# DEVICE NUMBERING IS NOT STABLE: the real LTO-6 may be lent to this VM (production
# owns it from home2 since 2026-09-28; ADR-0012), and a reboot can hand it /dev/nst0.
# A restart of mhvtl's tape daemons renumbers the mhvtl drives too (needrestart did it
# after an apt upgrade on 2026-10-23). So never hard-code /dev/nstN: resolve it from
# /dev/tape/by-id at the start of every run, and always set TAPECTL_GATE_TAPE. Discovery
# fails closed on a non-mhvtl device, so an unset value aborts rather than writing to
# the real drive — but do not rely on that. scsi-HUJ808A5L4-nst is the REAL drive;
# scsi-XYZZY_A* are mhvtl. The gate and mhvtl_e2e want an LTO-8 emulation (ULT3580-TD8,
# `lsscsi`): A1 and A2 are TD8, A3 and A4 TD6, whose LTO-6 media the gate's LTO-8
# backend refuses. Pass the /dev/nstN spelling the link resolves to, not the by-id path:
# one of the gate's steps needs that spelling in its config.
TAPECTL_GATE_TAPE="$(readlink -f /dev/tape/by-id/scsi-XYZZY_A2-nst)" TAPECTL_MHVTL=1 \
    cargo test --test mhvtl_e2e -- --ignored --nocapture

# TapeStore's own command count held to the kernel's other_cnt (#416 item 12). Same
# gating; it writes the loaded cartridge. The gate runs it as its rust_motions leg.
TAPECTL_GATE_TAPE="$(readlink -f /dev/tape/by-id/scsi-XYZZY_A2-nst)" TAPECTL_MHVTL=1 \
    cargo test --test mhvtl_motions -- --ignored --nocapture

# Logical Block Protection through the tape layer (#388): arming and the probe, protected
# writes and reads, the corrupt-block hook and item 174's return to the file's start, the
# close's page clear, a leftover cleared at the next open. It needs a TD6 drive (A3 or A4:
# Reed-Solomon; the TD8s speak CRC32C and fall back) and runs the library at a 262,144-byte
# payload, since mhvtl cannot carry the 524,292-byte wire block in one command. Same gating
# on its own variable, resolved by-id; it writes that drive's cartridge and leaves it at
# 524288 with the page at method 0. The gate runs it as rust_lbp, and the gate itself now
# needs TAPECTL_GATE_TAPE_TD6 too (unless TAPECTL_GATE_SPAN_ONLY is set).
TAPECTL_GATE_TAPE_TD6="$(readlink -f /dev/tape/by-id/scsi-XYZZY_A3-nst)" TAPECTL_MHVTL=1 \
    cargo test --test mhvtl_lbp -- --ignored --nocapture

# Performance scenarios (100k-file unit, 2 GiB write, 184k-file rebuild); ~3 min. The one
# case a release build is expected.
TAPECTL_PERF_TESTS=1 cargo test --test performance --release -- \
    --ignored --nocapture --test-threads=1
```

**`first-run.sh --upgrade` (#505, ADR-0012 item 192) has its own mhvtl harness**,
`scripts/upgrade-mhvtl.sh`: the previous release tag's `first-run.sh` installs into a
throwaway home, then the candidate's `--upgrade` runs three times (refused at the rehearsal
on the production cartridge, resumed to a green rehearsal on a second cartridge, the tidy).
It works in throwaway clones (a candidate not newer than the tag is stamped X.Y.(Z+1) and
tagged in its clone only), builds both in debug, and stands in for the host's units: a
transient `tapectl-drive-poll.timer` in `/run/systemd/system`, an installed backup unit under
`TAPECTL_UNIT_DIR`, the binary at `TAPECTL_INSTALL_BIN`. It needs a committed HEAD and two
cartridges of the drive's generation. Not covered: tidy's removal (a typed word), step 11's
real backup service, a release build.

```bash
TAPECTL_MHVTL=1 bash scripts/upgrade-mhvtl.sh \
    --device "$(readlink -f /dev/tape/by-id/scsi-XYZZY_A1-nst)"
```

**The heir path's only test with real tools is the mhvtl gate** (ADR-0012 item 70,
#457). `tests/heir_restore_sh.rs` runs RESTORE.sh with `mt`, `dd` and `age` faked (CI
has no age CLI); only the gate's RESTORE.sh legs (`heir_*`) and home2's real-drive
proof run it with the real `age`, `dar`, `mt` and `tar`. A release is not proven on
the heir path until the gate has run; each run record names the age version it used
(`age --version`). Since 1.3.0 the gate's heir legs also run RESTORE.sh's index modes
(`heir_where`, `heir_find`), its slice replacement (`heir_slice_from_copy`) and its per-file
sha256 check (`heir_file_check`) with the real tools, and since 1.8.0 its one stream into dar
(`heir_slice_boundary_read`) and its note on damage dar never needed (`heir_late_damage`). The gate's Rust legs pass only when every test they name ran and
said `ok`, and a gate run turns an on-media skip into a failure (#471).

## Architecture

**Three-phase pipeline:** `snapshot create` (fast metadata) → `stage create` (dar + sha256 + encrypt) → `volume write` (tape I/O)

**Key subsystems:**
- **CLI layer** (`src/cli/`): clap derive-based subcommands (tenant, unit, snapshot, stage, volume, catalog, restore, audit, etc.)
- **Database** (`src/db/`): SQLite with WAL mode, forward-only numbered migrations, full audit trail; `ontape_catalog.rs` is the operator envelope's `catalog.db` — schema, generation probe, read and write in one place
- **Unit management** (`src/unit/`): archival entities tracked via `.tapectl-unit.toml` dotfiles in each directory
- **dar integration** (`src/dar/`): subprocess wrapper for create, restore and the version check; minimum dar 2.6.x
- **Staging** (`src/staging/`): sha256 validation beside dar's single read of the source, `dar -c -` cut into dar slices and age-encrypted in memory (no plaintext on the staging device, #370), ephemeral slices; `release.rs` is the policy gate in front of `clean_staging`, shared by `collection run`, `volume write` and `volume resume` (#440)
- **Volume management** (`src/volume/`): Layout-v2 self-describing layout (`volume-format-v2.md`), the typestate write session (`session.rs`), Layout build/materialize (`build.rs`), front-index/seal/ID-thunk parsers (`format.rs`), `MANIFEST.toml` in both directions (`manifest.rs`), envelope read-back (`envelope.rs`), RESTORE.sh from named awk fragments (`restore_script.rs`), catalog rebuild (`rebuild.rs`), raw dump (`raw.rs`), verify, read-slices
- **Tape I/O** (`src/tape/`): kernel st driver via ioctl, fixed 512KB block mode
- **Crypto** (`src/crypto/`): age multi-recipient encryption, per-tenant key isolation
- **Policy** (`src/policy/`): 3-level resolver (dotfile > archive_set > defaults), advisory audit; `coverage.rs` owns the copy/location SQL and every `volumes.status` predicate, `escrow.rs` owns escrow coverage (verdict AND query) — never inline either again. A copy floor is always `copy_floor(config, resolved.copy_floor_base(), status)`, never `min_copies` read directly (1.7.1)
- **Store trait** (`src/store.rs`): built (ADR-0006) — `capacity`/`execute`/`confirm`/`read_file`, streaming so RAM tracks block size not slice size. `TapeStore` and `MemStore` share one chain-walk implementation, so MemStore-based tests exercise the real confirm path. `WarehouseStore`/`ExportStore` stay ADR-0006 peers but are unbuilt and untracked: #72 (closed) rescoped warehouse copies to a documented rclone/aws-cli procedure over sealed volumes (`docs/design-errata.md`), #73 (closed) is the warehouse-location model, and no issue tracks an `ExportStore` — `export` does not go through the trait
- **Cross-volume index** (`src/volume/index.rs`, `policy::coverage::index_versions`): `index/units.tsv` and `index/paths.tsv` built in the home work dir, streamed into each envelope by `build()` after `catalogs/`; RESTORE.sh's `--where`/`--find` read it; `catalog rebuild` reads the operator map as a to-do list and never imports it (`volume-format-v2.md` §1.5)
- **Collection** (`src/collection/`): `[[collections]]` config → `collection sync|status|plan|run`; folder-per-unit registration, alphabetical next-fit batch selector, stage-once/write-N-copies/release

**Design principles:**
- Volumes are self-describing — full data restore without the database or tapectl, and catalog rebuild with the operator/escrow key; what that does and does not promise is defined in `docs/design/volume-format-v2.md` ("Self-describing — what that promises")
- Strict tenant isolation — zero content metadata in plaintext on tape; tenant envelopes use age trial-decryption
- Multi-tenant bin-packing on shared volumes
- Physical cartridges tracked separately from logical volumes (cartridges can be erased/reused)
- Policy audit is advisory, never blocking (exit codes: 0=clean, 1=warn, 2=violation, 70=error)

## External Dependencies

- `dar` ≥2.6 (recommended 2.7.20+) — archive creation/extraction
- `sg3-utils` — drive health pages (`sg_logs`), the cartridge's MAM (`sg_read_attr`), drive identity (`sg_inq`), and since 1.7.2 the Control Data Protection mode page that arms and clears Logical Block Protection (`sg_modes` reads it, `sg_wr_mode` writes it; `src/tape/lbp.rs`, #388)
- `mhvtl` — virtual tape library for development/testing
- `lsscsi`, `mt-st` — the binary never calls them (device discovery and debugging), but
  `scripts/first-run.sh` step 2 requires both, along with `acl` (`setfacl`) and `python3`
- the `age` and `tar` CLIs — not used by the binary (it uses the `age` and `tar`
  crates), but the on-tape RESTORE.sh (the heir path) refuses to run without them or `mt`,
  and names `gpg` up front when the dar it finds needs it (ADR-0012 2026-10-06 item 21)

## Key Rust Dependencies

- `age`/`rage` 0.11 is pinned: its pre-1.0 API is unstable

## Configuration

- System config: `~/.tapectl/config.toml` (dar path, backends, defaults incl. exclusions and copy policy, archive sets, staging, collections, host check); locations are catalog rows from `location add`, not config — a `[locations]` table is refused
- Database: `~/.tapectl/tapectl.db`
- Session logs: `~/.tapectl/logs/<UTC>-<command>-<pid>.log`, one per long operation
  (issue #386, `src/progress.rs`): phases, waits over 5 s, stalls, and the INFO tracing
  tee. Library code calls `progress::phase`/`wait`/`add_bytes` unconditionally — they
  are no-ops on a thread with no session; phase durations land in `phase_timings`
  (migration 028). Progress goes to stderr only; `--quiet` silences it.
- Per-unit config: `.tapectl-unit.toml` in each archival directory
