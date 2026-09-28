# tapectl documentation

Start with the [project README](../README.md) for what tapectl is. This page lists
every document, grouped by what you are trying to do.

## Using tapectl

| Document | What it covers |
|---|---|
| [Walkthrough](walkthrough.md) | One complete session with real output: init, tenants, units, two tapes in two places, audit, restore, the Heir Kit, a disaster-recovery rehearsal. **Start here.** |
| [Concepts](concepts.md) | The model: tenants, units, collections, snapshots and versions, stage sets, volumes and cartridges, locations, copies and policy, what is on a tape. |
| [Install](install.md) | The installer (`scripts/first-run.sh`) step by step, host profiles, the service user, timers, reinstalling, moving hosts, uninstalling. |
| [Operator guide](operator-guide.md) | Day-to-day operation: archiving, second copies, restores, tape-only units, retiring and compacting volumes, cartridge tracking, warehouse copies, the maintenance cadence. |
| [Configuration](configuration.md) | Every `config.toml` key with its default, the per-unit `.tapectl-unit.toml`, archive sets and policy resolution, environment variables. |
| [Keys and recovery](keys-and-recovery.md) | Primary, backup and escrow keys; who can decrypt what; the Heir Kit; restoring with tapectl, with only a key and a tape, and rebuilding a lost catalog. |
| [Troubleshooting](troubleshooting.md) | Exit codes, how confirmation works, and every refusal you are likely to meet — what it means and what to do. |
| [Command reference](cli/README.md) | Every command, subcommand and flag, generated from the binary's own definitions. |
| [Man pages](man/README.md) | The same reference as `man` pages (`man -l docs/man/tapectl.1`). |

## The tape format and the design

| Document | What it covers |
|---|---|
| [On-tape format v2](design/volume-format-v2.md) | The byte layout of a tape — normative. Includes what "self-describing" does and does not promise. |
| [Write session](design/layout-session.md) | The state machine of a tape write: build → validate → plan → execute → seal → confirm. |
| [Architecture decisions](adr/) | ADR-0001 … ADR-0013: the rules and why (escrow, consent tiers, cartridge identity, generations, …). |
| [Vocabulary](../CONTEXT.md) | The project's defined terms. |
| [Design errata](design-errata.md) | Where the original design document (`tapectl-design-v4_0.md`) is superseded. |

## Testing and hardware validation

| Document | What it covers |
|---|---|
| [Lifecycle suite](lifecycle-suite.md) | Simulating years of use in minutes on a virtual library (or, carefully, a real drive). |
| [LTO-6 validation checklist](lto6-validation-checklist.md) | The procedure for validating a real drive. |
| [LTO-6 passthrough](lto6-drive-passthrough.md) | Lending a physical drive to a VM (libvirt SCSI hostdev). |
| [Performance baselines](perf-baselines.md) | Regression baselines for the performance suite. |

## Records

Dated records are kept as history, not instructions: hardware session journals
(`lto6-session-journal-*.md`), run reports ([runs/](runs/)), review audits
([audits/](audits/)), research notes ([research/](research/)), and the maintainer
handoff ([handoff.md](handoff.md)).

## Keeping the docs honest

Every `tapectl …` line in a shell code block of the user-facing documents is checked
against the binary by `scripts/check-docs.py` (subcommands and flags must exist;
full `config.toml` examples must pass `config check`), and `docs/cli/` is regenerated
from the command definitions (`cargo run --example gen_cli_md`) with a test that fails
while it is stale. If you find an example that does not work, that is a bug — please
open an issue.
