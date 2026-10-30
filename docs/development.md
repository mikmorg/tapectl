# Development

How to build, test and change tapectl from a checkout. [CLAUDE.md](../CLAUDE.md) is
the maintained source for the exact invocations of the gated suites
([its "Testing" section](../CLAUDE.md#testing)); this page says what each suite is
for and what it needs, and links there rather than copying the command lines.

## Build and check

```bash
cargo check --all-targets       # fast compile verification
cargo build                     # debug binary at target/debug/tapectl
cargo test                      # about 4,600 tests; needs `dar` on PATH, no tape hardware
cargo clippy --all-targets      # must stay warning-clean
cargo fmt --check
scripts/check-docs.py           # `tapectl …` examples: subcommands and long flags must exist
```

The crate is a library with a thin binary: all logic lives in `src/lib.rs`'s modules,
and the integration tests import `tapectl::` directly.

`cargo test` is not hermetic: the staging pipeline, the dar wrapper and several
command tests run the real `dar`, found on `PATH`. Without it the run fails with
panics that do not name dar; `cargo test --test test_dependencies` says what is
missing.

After a CLI change, regenerate the two command references:

```bash
cargo run --example gen_man       # man pages, docs/man/*.1
cargo run --example gen_cli_md    # Markdown, docs/cli/*.md (tests/cli_md_fresh.rs fails while stale)
```

`scripts/check-docs.py` checks every `tapectl …` line in a shell block of the
user-facing documents against the binary (subcommands and long flags must exist),
runs `config check` on every whole `config.toml` example, and refuses a hard-coded
`--device /dev/nstN`. `--self-test` is its positive control. Run it with clippy and
fmt before committing a change to the docs or the CLI.

CI runs fmt, clippy, `cargo test` (with dar installed), the `docs/man` drift check and
a non-blocking `cargo audit`, on pushes and pull requests to `master` only. Its runner
is GitHub's `ubuntu-latest`, Ubuntu 26.04 as of 1.8.0, whose coreutils are uutils, so
a change to `RESTORE.sh` should pass
`tests/heir_restore_sh.rs` in an `ubuntu:26.04` container as an ordinary user (root
hides a pipe-size difference), or in a pull request's CI, before it reaches `master`.

**Byte pins.** `tests/on_tape_golden.rs` pins the bytes of `MANIFEST.toml` and
`RESTORE.sh` as tapectl writes them. If one fails, the on-tape format changed: that is
a decision recorded in an ADR, never a pin refreshed to make a test pass.

**Budget suites** pin what a path costs (tape motions, catalog statements, sha256
passes), not what it returns. A pin moves only on purpose, with the reason in the
commit message; CLAUDE.md, "Testing", has the three commands.

## Suites that need a virtual tape library

The mhvtl suites and the verification gate need a virtual tape library: setting one up
is in the operator guide,
["Testing with a virtual tape library"](operator-guide.md#testing-with-a-virtual-tape-library).
They skip unless their variables are set, and the Rust ones are `#[ignore]`d, so they
run with `-- --ignored`.

**Find the drives at the start of every run.** `/dev/nstN` numbering is not stable: a
reboot can renumber the drives, and so can a restart of mhvtl's tape daemons (an
`apt upgrade` that restarts services does it). Never hard-code `/dev/nstN`. Resolve
it from `/dev/tape/by-id/`, where the `scsi-XYZZY_A*` links are mhvtl's, and pass the
`/dev/nstN` spelling the link resolves to (one of the gate's steps needs that
spelling in its config):

```bash
ls -l /dev/tape/by-id/ | grep XYZZY                    # the mhvtl drives; a real drive's link has its own serial
lsscsi                                                 # which emulate an LTO-8 (ULT3580-TD8) and which an LTO-6 (TD6)
readlink -f /dev/tape/by-id/scsi-XYZZY_A2-nst          # the /dev/nstN to pass
```

Their discovery refuses anything that is not mhvtl, so an unset or wrong variable
aborts rather than writes to a real drive, but do not rely on that.

| Suite | What it proves | Needs |
|---|---|---|
| `tests/mhvtl_e2e.rs` | the round trip on tape and on-tape tenant isolation | `TAPECTL_GATE_TAPE` (an LTO-8 emulation), `TAPECTL_MHVTL=1` |
| `tests/mhvtl_motions.rs` | the tape layer's command count against the kernel's | the same |
| `tests/mhvtl_lbp.rs` | Logical Block Protection through the tape layer | `TAPECTL_GATE_TAPE_TD6` (an LTO-6 emulation: Reed-Solomon LBP), `TAPECTL_MHVTL=1` |
| `scripts/mhvtl-verify-gate.sh` | the operator-level verification gate: the Rust legs above, and every heir leg with the real `age`, `dar`, `mt` and `tar` | `TAPECTL_GATE_TAPE` and `TAPECTL_GATE_TAPE_TD6` (two different drives), `TAPECTL_MHVTL=1` |
| `tests/performance.rs` | the performance baselines ([perf-baselines.md](perf-baselines.md)) | `TAPECTL_PERF_TESTS=1`, a release build |

The gate is the heir path's only test with the real tools: `tests/heir_restore_sh.rs`
runs `RESTORE.sh` with `mt`, `dd` and `age` faked. A release is not proven on the heir
path until the gate has run.

## The lifecycle suite

`scripts/lifecycle-suite.sh` simulates years of use in minutes
([lifecycle-suite.md](lifecycle-suite.md); `--list` names its scenarios). It takes an
mhvtl drive of any generation. On a drive that is not mhvtl it switches to a
real-drive mode that runs only with `--erase short`, `--single-cartridge` and
`--i-will-lose-the-cartridge SERIAL`, and erases that cartridge. It is what the
installer's step-12 rehearsal runs.

## The plaintext scan

Staging writes no plaintext to the staging device (ADR-0012). `cargo test` audits a
real stage with inotify. `scripts/plaintext-scan.sh <tapectl binary>` (needs sudo, no
tape) stages a marker tree onto a `sync,nodiscard` loop-mounted image and scans the
raw image for the marker, with a deleted canary as its positive control.

## The installer's re-entry acceptance

`scripts/first-run-reentry-mhvtl.sh` is issue #414's acceptance. It drives
`scripts/first-run.sh` itself (a throwaway home, `--auto`, a profile of its own)
through the step-12 rehearsal, kills the step-13 write part-way, and checks that
re-entering with `--from 13` resumes the write and finishes the verify, audit, shelf
move and Heir Kit. It takes an mhvtl drive by its by-id name and refuses any other:

```bash
TAPECTL_MHVTL=1 scripts/first-run-reentry-mhvtl.sh --device /dev/tape/by-id/scsi-XYZZY_A1-nst
```

## A map of the code

- `src/cli/`: the commands (clap derive), thin over the library modules.
- `src/db/`: the SQLite catalog and its forward-only migrations; `ontape_catalog.rs`
  is the operator envelope's `catalog.db`.
- `src/staging/`: `dar -c -` cut into slices, hashed and age-encrypted in memory.
- `src/volume/`: the v2 layout, the write session (`session.rs`), the format parsers,
  `MANIFEST.toml` and `RESTORE.sh`, verify, restore, and catalog rebuild.
- `src/tape/`: the st driver's ioctls, the cartridge's chip, Logical Block Protection.
- `src/policy/`: copies, locations and escrow coverage, and the audit.
- `src/collection/`: folder-per-unit sources.
- `src/store.rs`: the Store trait and the chain walk shared by the tape and the
  in-memory store.

Design records: [docs/adr/](adr/), [docs/design/](design/) (the on-tape format is
[volume-format-v2.md](design/volume-format-v2.md)), and [CONTEXT.md](../CONTEXT.md),
the vocabulary.
