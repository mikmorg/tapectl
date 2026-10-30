# tapectl documentation

Start with the [project README](../README.md) for what tapectl is. This page lists
every document in `docs/`, and the vocabulary and original design kept at the top of
the repository, grouped by what you are trying to do.

## Using tapectl

| Document | What it covers |
|---|---|
| [Walkthrough](walkthrough.md) | One complete session with real output: init, tenants, units, two tapes in two places, audit, restore, the Heir Kit, a disaster-recovery rehearsal. **Start here.** |
| [Concepts](concepts.md) | The model: tenants, units, collections, snapshots and versions, stage sets, volumes and cartridges, locations, copies and policy, what is on a tape. |
| [Install](install.md) | The installer (`scripts/first-run.sh`) step by step, host profiles, the service user, timers, reinstalling, moving hosts, [upgrading in place](install.md#10-upgrading-in-place), uninstalling. |
| [Operator guide](operator-guide.md) | Day-to-day operation: archiving, second copies, restores, tape-only units, retiring and compacting volumes, cartridge tracking, warehouse copies, the maintenance cadence. |
| [Configuration](configuration.md) | Every `config.toml` key with its default, the per-unit `.tapectl-unit.toml`, archive sets and policy resolution, environment variables. |
| [Keys and recovery](keys-and-recovery.md) | Primary, backup and escrow keys; who can decrypt what; the Heir Kit; restoring with tapectl, with only a key and a tape, and rebuilding a lost catalog. |
| [Troubleshooting](troubleshooting.md) | Exit codes, how confirmation works, and every refusal you are likely to meet — what it means and what to do. |
| [Command reference](cli/README.md) | Every command, subcommand and flag, generated from the binary's own definitions. |
| [Man pages](man/README.md) | The same reference as `man` pages (`man -l docs/man/tapectl.1`). |
| [Release notes](releases/) | What each release changes for an operator: on-tape format, catalog migrations, exit codes, config keys, known open items ([1.1.0](releases/1.1.0.md), [1.1.1](releases/1.1.1.md), [1.1.2](releases/1.1.2.md), [1.2.0](releases/1.2.0.md), [1.3.0](releases/1.3.0.md), [1.4.0](releases/1.4.0.md), [1.5.0](releases/1.5.0.md), [1.6.0](releases/1.6.0.md), [1.7.0](releases/1.7.0.md), [1.7.1](releases/1.7.1.md), [1.7.2](releases/1.7.2.md), never tagged on its own and contained in [1.8.0](releases/1.8.0.md)). Read them from your version to the target before [an upgrade](install.md#10-upgrading-in-place). |

## The tape format and the design

| Document | What it covers |
|---|---|
| [On-tape format v2](design/volume-format-v2.md) | The byte layout of a tape — normative. Includes what "self-describing" does and does not promise. |
| [Write session](design/layout-session.md) | The state machine of a tape write: build → validate → plan → execute → seal → confirm. |
| [v2 open questions](design/v2-open-questions.md) | The format v2 design questions and how each was resolved (§§1–11) — normative after the two above. |
| [v2 implementation plan](design/v2-implementation-plan.md) | The T0–T11 build playbook the v2 write path was built from — last in the same order of authority. |
| [Threat model and boundaries](design/threat-model.md) | Who the adversary is and is not; integrity versus authenticity (the seal is not a signature); the st/SG boundary and the synchronous seal filemark; the power baseline; how the format grows; one cartridge, one recovery unit. |
| [Architecture decisions](adr/) | ADR-0001 … ADR-0013: the rules and why (escrow, consent tiers, cartridge identity, generations, …). |
| [Vocabulary](../CONTEXT.md) | The project's defined terms. |
| [Original design document](../tapectl-design-v4_0.md) | Design v4.0 — still the reference for whatever the documents above do not cover; read it with the errata. |
| [Design errata](design-errata.md) | Where the original design document is superseded. |

## Testing and hardware validation

| Document | What it covers |
|---|---|
| [Development](development.md) | Building and testing from a checkout: the checks to run before a commit, the suites that need a virtual tape library and what each needs, the plaintext scan, the installer's re-entry acceptance, a map of the code. |
| [Lifecycle suite](lifecycle-suite.md) | Simulating years of use in minutes on a virtual library (or, carefully, a real drive). |
| [LTO-6 validation checklist](lto6-validation-checklist.md) | The procedure for validating a real drive. |
| [Profiling an LTO drive](lto-profiling.md) | Measuring a drive and a test cartridge with `scripts/lto-profile.sh`, and the `[[backends.lto]]` block it emits. |
| [LTO-6 passthrough](lto6-drive-passthrough.md) | Lending a physical drive to a VM (libvirt SCSI hostdev). |
| [Performance baselines](perf-baselines.md) | Regression baselines for the performance suite. |

## Records

Dated records are kept as history, not instructions: hardware session journals
(`lto6-session-journal-*.md`), raw drive captures from the virtual library
(`mhvtl-baseline-recordings.txt`), run reports ([runs/](runs/)), review audits
([audits/](audits/)), research notes ([research/](research/)), the questions
unattended runs deferred to the maintainer, all since answered
([decisions-pending.md](decisions-pending.md)), and the maintainer handoff
([handoff.md](handoff.md)).

## Keeping the docs honest

Every `tapectl …` line in a shell code block of the user-facing documents is checked
against the binary by `scripts/check-docs.py` (subcommands and long `--flags` must
exist — short flags, arguments and required options are not checked; full
`config.toml` examples must pass `config check`), and `docs/cli/` is regenerated
from the command definitions (`cargo run --example gen_cli_md`) with a test that fails
while it is stale. If you find an example that does not work, that is a bug — please
open an issue.
