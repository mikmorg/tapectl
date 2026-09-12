# lifecycle-suite

`scripts/lifecycle-suite.sh` simulates years of tapectl use — multi-volume
archives, key rotation, tenant reassignment, tape-only marking, reclamation,
compaction, retirement, cartridge reuse, database loss — and, at the end of
every scenario, runs a **restore matrix** that recovers the data every way
tapectl and its on-tape scripts allow.

## Why this exists, alongside `mhvtl-verify-gate.sh`

The gate proves one happy path (plus interrupt/resume) on one tape. It cannot
see defects that come from **command ordering across time** — issue #115
(staging before escrow registration seals a tape the escrow key can never
decrypt) is exactly that class, found on real LTO-6 hardware on 2026-09-10
(`docs/lto6-session-journal-2026-09-10.md`). This suite is the instrument for
that class.

## Invocation

```bash
# mhvtl virtual library (device discovery, generation-matched media, same as the gate)
scripts/lifecycle-suite.sh --scenario first-year
scripts/lifecycle-suite.sh --all --seed 3

# a real single-cartridge LTO-6 drive — every flag below is required
scripts/lifecycle-suite.sh --scenario retire-and-reuse \
    --device /dev/nst0 --erase short --single-cartridge \
    --i-will-lose-the-cartridge <MEDIUM_SERIAL_FROM_sg_read_attr>

# plan only — no build, no discovery, no lock, executes nothing
scripts/lifecycle-suite.sh --dry-run --all
scripts/lifecycle-suite.sh --list
```

On a device discovery can't resolve to an mhvtl drive, the script falls back
to real-drive mode automatically and *requires* `--device`, `--erase short`,
`--single-cartridge`, and `--i-will-lose-the-cartridge SERIAL` — the last
cross-checked against `sg_read_attr`'s own "Medium serial number" field
before anything destructive runs (the same anchored-match consent shape as
`scripts/lto6-measure.sh`). `--erase long` (`mt erase`) is instant on mhvtl
and takes HOURS on real LTO — never pass it on real hardware.

Every scenario gets its own `$HOME_DIR`/source tree/tape-slot tracking
(`--all` runs all 13 without leaking state between them), and workspace
output goes to `--out DIR` (default `/scratch/tapectl-lifecycle`) as
`run-<timestamp>/`, with `REPORT.md`, one `log-<check>.txt` per check,
`SKIPPED.txt`, and `commands.log` (every `tapectl`/device command, in order —
the journal of everything done).

## Scenarios

| Name | What it exercises |
|---|---|
| `first-year` | Baseline: escrow BEFORE staging, 2 tenants, 3 units, 1 volume, full restore matrix |
| `evolving-source` | Mutate every unit, dirty/diff/supersedable, v2 on a 2nd volume, both versions restorable |
| `key-rotation` | Rotate mid-archive; old (inactive), new, and escrow keys all still restore, on both volumes |
| `tenant-reassign` | Move units between tenants; DB ownership change doesn't affect who can decrypt what |
| `tape-only-and-reclaim` | mark-tape-only preconditions (copies/locations), reclaim a superseded snapshot |
| `compaction` | mhvtl-only. compact-finish's copy-elsewhere refusal, then success once satisfied |
| `retire-and-reuse` | Retire (refused sole-copy, then safe), cartridge reuse, ADR-0003 sealed-tape refusal |
| `db-loss` | db backup/import, DB-less raw-volume + `import`, the pure heir path, `catalog rebuild` from tape |
| `escrow-ordering` | issue #115 regression: stage-before-escrow refusal, then a working escrow restore |
| `restore-file-and-catalog` | catalog vs. `find`, single-file restore (unicode/empty/symlink), integrity checks |
| `quick-archive` | The one-shot create+stage+write flow |
| `collection` | Folder-per-unit sync/status/plan/run, rename resolved by dotfile uuid |
| `permute` | Seeded random walk over the whole command surface (`--seed`/`--steps`) |

## The restore matrix

Run at the end of most scenarios as `restore_matrix LABEL UNIT TENANT
SRC_DIR TAG [OTHER_TENANT]`, producing 10 checks named `TAG.<method>`:
`unit` and `file` (via `tapectl restore`), `restore_sh_dd` (dd the script off
tape, `--info`/`--verify`), `restore_sh_primary`/`restore_sh_backup` (both of
the tenant's keys), `operator_envelope` (a normal restore path — see
`src/volume/layout.rs:516`), `escrow` (the printed-once escrow secret),
`raw_volume` (DB-less dump, every file verified), `isolation` (a different
tenant's key must not decrypt this one's slice — SKIPs, visibly, if no other
tenant exists on the archive), and `verify` (full + quick chain walk).

Comparison is always `diff -r --no-dereference` **plus** a content+symlink-
target tree checksum (`tree_checksum` — journal Phase 4's method); plain
`diff -r` false-passes when a symlink is flattened to a regular file.

`isolation` targets the unit's **newest** stage set. Ordering by slice number
alone could pick a slice staged under a previous owner (before a `tenant
reassign`), which that owner's key legitimately still decrypts — sealed media
cannot be retroactively re-encrypted. Isolation here means *the current owner's
data is not readable by another tenant*; what a former tenant should retain
across a reassignment is open in #131.

There is deliberately **no** way to skip a whole matrix. One existed briefly
while closing #128, on the premise that some matrices need more than one
cartridge; every such case turned out to be a real bug (a missing escrow step, a
missing `volume init`, and #131). If a matrix fails, diagnose it — do not
attribute it to the media. The cheap discriminator is to re-run the scenario
multi-cartridge (`--erase long`, no `--single-cartridge`): if it still fails,
the media is not the cause.

## The `permute` restore baseline

`restore-latest-and-diff` compares the tape against a copy of the source taken
when a unit is **staged**, not when the volume is written. A volume is written
from a stage set built earlier, so a mutation landing in between would otherwise
make the "pristine" copy disagree with the tape and fail the check on content
the tape was never meant to hold. The tape holds what was staged, so that is the
instant the baseline must capture.

`mutate_source` never touches `.tapectl-unit.toml`. It is tapectl's own control
file, not user content — mutating it does not model source drift, it corrupts
metadata, and `audit` then correctly reports `policy_unresolvable` for that unit
for the rest of the walk.

## Reproducing a `permute` failure

The op sequence is generated once from `--seed` via Python's
`random.Random` and printed verbatim to `REPORT.md` before the walk starts.
To reproduce: re-run with the **same** `--seed` and `--steps` — the sequence,
and therefore the failure, is identical.

## Decisions this suite made from reading the code (not asked)

- **Operator envelope** is a normal restore path, not a refusal — layout.rs's
  `envelope_positions()` lists it alongside tenant envelopes as an equal
  trial-decrypt candidate.
- **Touch-only is dirty** under the default `mtime_size` checksum_mode — it
  compares mtime AND size, so a same-content/new-mtime edit registers dirty;
  it's blind only to a same-size/same-mtime *content* change.
- **`cartridge mark-erased`'s gate is the cartridge's DB status**
  (`pending_erase`, set by `volume retire`), not whether a physical erase
  happened — the DB has no way to observe that.
- **Every check is expected to pass.** `db-loss`'s scenario (b) used to be a
  deliberate failure — top-level `import` inserts only a bare `volumes` row,
  so a follow-on `restore unit` had nothing to resolve against. The CTO
  settled [#136](https://github.com/mikmorg/tapectl/issues/136) the other
  way: `import` registers a cartridge and that is all it was for, and
  rebuilding the catalog is its own command. Arm (b) now asserts that
  refusal positively, and arm **(d)** drives
  `catalog rebuild --from-volume` end to end AND the disaster-recovery
  recipe: `init --no-escrow`, rebuild from the tape's envelopes,
  `catalog locate`, a real `restore unit` off tape through the rebuilt rows,
  `volume verify --full`, then `key import --escrow` the ORIGINAL escrow key
  and assert the rebuilt rows read `escrow: yes` (their receipts rode the
  tape) with no escrow findings in `audit`; then a second rebuild that must
  change nothing. The mistake is measured too, in a second home: a plain
  `init` mints a replacement escrow identity, so the rebuilt rows read `NO`
  and `audit` names `escrow_identity_mismatch` exactly once with the
  `key import --escrow` command. (`init --escrow-public-key`, #139, is the
  one-command form; the arm keeps the two-step on purpose.)
- Three former expected failures are now fixed:
  `restore-file-and-catalog`'s symlink case (`restore file` dereferenced via
  `fs::copy` while `restore unit` preserved the link — the two commands
  disagreed about the same archive entry) and `escrow-ordering`'s
  stage-before-escrow refusal (#115).

## Not covered here

- **Interrupt/resume** — that's `mhvtl-verify-gate.sh`'s leg 5 (parking a
  write mid-session and proving `volume resume` finishes it).
- **The ENOSPC drill** — mhvtl gives a false pass past capacity; it's a
  real-hardware-only, hours-long procedure in `docs/lto6-validation-checklist.md`.
- Warehouse deposits (`volume deposit add`) and the Heir Kit ceremony
  (`key escrow-kit`) — neither is in the task's scenario list.
