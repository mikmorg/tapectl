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

**Handoff:** `docs/handoff.md` divides the remaining work between what an agent
finishes and what needs the operator's hands. It covers the 1.1.0 fresh start in order:
- the real-drive proof on home2, on the CTO's go-ahead;
- the `v1.1.0` tag;
- the operator's fresh start;
- `keepsake` on two cartridges, then the Heir Kit;
- the backlog.

The 1.0.0 real-drive rehearsal is done (2026-09-23,
`docs/runs/2026-09-23-real-drive-rehearsal.md`).

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
  removed; 40 checks since #355 added `empty_drive_refused`, GREEN 40/40 on 2026-09-29; 46 since #480 added `heir_where`, `heir_find` and `heir_slice_from_copy`, the last of which writes a second cartridge and retires its `MHVTLC2` volume; 52 since #420 added the five `heir_short_*` checks, GREEN 52/52 on 2026-10-20; 61 since #478 added the nine `span_*` checks of item 136, GREEN 61/61 on 2026-10-23).

Issues #22–#28 (closed) describe the *pre-v2* design; read them against the normative
set above, never as instructions.

**Production runs on home2** (ADR-0012, 2026-09-28): the drive's hostdev is detached from
this VM; `contrib/hosts/home2.profile` and `home2-prep.sh` are the host's install. home2
ran 1.0.0 (one tape, L6-0001, 2026-10-01), started fresh on 1.1.0 on 2026-10-14/15
(`keepsake` on L6-0001 and L6-0002), and upgrades in place to 1.5.0 (handoff (f)).

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
  `catalog rebuild` still never changes a status (ADR-0012, 2026-09-29 later amendment),
  with one exception from 1.7.0: it promotes a spanning snapshot it finds `staged` to
  `current`, upward only, once every slice is on a volume the catalog records `sealed`
  (ADR-0012 item 144); a volume's status it never changes.
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

**2026-10-06 and 2026-10-07: what 1.1.0 is.** ADR-0012's amendments of those dates
(items 1–24 and 1–35) are the rulings behind 1.1.0. `docs/releases/1.1.0.md` is the
operator's account of them: on-tape changes, migrations 027–035, exit codes and config
keys.

**2026-10-12: home2 starts fresh on 1.1.0** (ADR-0012, amendment "2026-10-12", items
36–48).
- **The fresh start.** There is no upgrade from 1.0.0. A new catalog comes from
  `first-run.sh` step 1, and the escrow identity is adopted, not minted (item 39). Every
  tenant key is new (item 41). The 1.0.0 home and staging are deleted (item 38).
  L6-0001, `keepsake`'s only tape copy, is erased and reused (item 37). `keepsake` has no
  tape copy until two new ones seal, and that window is accepted.
- **Pulled into 1.1.0 for it:**
  - a backup copy of every tenant envelope on tape (item 44; `volume-format-v2.md` §1.3;
    re-pin `57f061f`);
  - the on-tape `catalog.db` generation 3 (item 48; §1.4);
  - an adopt-keypair path (`init --operator-key`, `tenant add --adopt-key`, `ADOPT_KEYS_FROM`;
    item 42).
- **Policy rulings:**
  - the tape-only multiplier raises copy floors only, never the distinct-place count
    (item 46);
  - a tape-only shortfall stays an ADR-0008 Tier-2 consent gate (item 47);
  - #416's pass counters are test-only (item 45).
- **Status.** 1.1.0 is tagged `v1.1.0` (2026-10-13) after the real-drive proof on home2
  (step 12 50/50, db-loss 6/6, `cartridge erase` on the real drive, the #326/#366/#368
  measurements and #426 traced to host contention;
  `docs/runs/2026-10-12-1.1.0-real-drive-proof.md`). The operator's fresh start ran on
  2026-10-14/15.

  `docs/handoff.md` has the sequence. The 2026-10-07 on-tape authorization ended when
  v1.1.0 was installed (item 40). Open after it: #420, #418 R4, #388, #409 and the #364
  staging epic.

**2026-10-15: 1.1.1, a patch for home2** (ADR-0012 amendment "2026-10-15", items 57–64;
`docs/releases/1.1.1.md`). No on-tape change; migration 036 adds `cleaning_cartridges`. In
it: `drive status`/`eject`/`cleaned`, `device_sg` derived from `device_tape`, proven-blank
init without `--force`, `volume write` releasing staging (`--keep-staging`), the one-command
copy `volume write --init --full-confirm --move-to`, optional labels for reads, raised
priority for tape commands, the `/etc/tapectl/home` pointer, and a single source read
(dar's `fadvise` neutralised by a seccomp filter on `dar -c`, #441). home2 upgrades in
place (take `db backup` with 1.1.0 first).

**2026-10-16: 1.1.2, the two reviews' fixes** (`docs/releases/1.1.2.md`; ADR-0012 amendment
"2026-10-15 (later)", items 65–70). No on-tape change and no migration. The design review (`docs/audits/2026-10-15-design-review.md`)
and the first adversarial review of the 1.1 code (`docs/audits/2026-10-15-1.1-diff-review.md`,
one high: a rebuilt unsealed tape counted as a copy) filed #444–#475, worked in four waves.
Things to know: an unreadable seal position is not an absent seal unless the drive reports
END OF DATA (#460; the HP drive proves it at the next step-12 rehearsal); `stage create`
compares dar's catalogue with what it hashed (#464); `audit` has a per-volume full-readback
clock and `[defaults] verify_interval_days` (#453, #454; home2 sets 365); the gate's Rust
legs are judged by test name and its erase leg runs `cartridge erase` then a no-force init.

**2026-10-16: 1.2.0, the re-pin** (`docs/releases/1.2.0.md`; ADR-0012 amendment
"2026-10-16", items 71–75). The minor moved because generated on-tape text changed:
RESTORE.sh checks each mode's own tools from one table (`RESTORE_TOOLS` in `layout.rs`),
exports `LC_ALL=C`, says "a key matched" only when one did, describes its work directory
(the decrypted envelopes under `${TMPDIR:-/tmp}`) and warns before filling it, and after a
lone-unit restore says what else the key opened; RECOVERY.md names a dar catalogue only when
the envelope holds it; File 1 names the drives that read the tape, the SAS interface, one
unit's disk space and the tool list. The golden RESTORE.sh pin moved once, in its own commit;
MANIFEST.toml, File 0, the front index, the seal and the envelopes did not, `requires` stays
empty and the "needs tapectl" text stays "tapectl 1.1.0" (item 73), so 1.1.x and 1.2.0 read
each other's tapes. Also in it: `[defaults] verify_interval_days` is 365 when absent (#453),
and `[[collections]]` `snapshot_command`/`release_command` let `collection run` read every
unit from a read-only snapshot while every recorded path stays real (#455; `collection::hook`,
`staging::view`). No migration. home2 goes from 1.1.0 straight to 1.2.0 by handoff (f); the
2026-10-16 on-tape authorization ends when v1.2.0 is installed (item 72). Next: the spanning
and #418 R4 design (1.3.0), put to the CTO in rounds.

**2026-10-19: 1.3.0, the cross-volume index and restore repairs** (`docs/releases/1.3.0.md`;
ADR-0012 items 76-103, ruled 2026-10-16 to 10-19). Each envelope now carries an index of the
archive (`volume-format-v2.md` §1.5): a tenant's units and every path with its recorded sha256,
and the operator's map of which volumes hold which version; RESTORE.sh reads it (`--where`,
`--find`) and checks every restored file against it. Restore repairs (#480): a damaged slice is
read from another copy (tapectl asks for the cartridge; RESTORE.sh takes `--slices-from` or
`--swap-tapes`), damaged files are filled from another copy of the same version, and tapectl's
restore reads around a bad 64 KiB age chunk (`crypto::chunks`, reviewed: a slice whose ciphertext
hash mismatched is whole only if its plaintext proves it). Every Rust reader checks `requires`
(#476). File 1 was rewritten (#477). No migration; `requires` stays empty. Spanning is designed
and ruled (items 76-77, 82-100: opt-in per unit, packed last, cut at the drive's early warning
then the short seal) and waits for #420 (#478); the profiling toolkit is #479.

**2026-10-19: 1.4.0, File 1 again, in-place compaction, the escrow heir's restore**
(`docs/releases/1.4.0.md`; ADR-0012 items 105-107). File 1 was rewritten after two more fresh
reads (#481): RESTORE.sh's bytes did not move. `volume rewrite` compacts a tape on its own
cartridge, even when it holds the only copy, behind a typed consent and a full readback, with a
hold no release path can lift until the new volume reads back (#483; `src/volume/rewrite.rs`,
migration 037). `restore unit|file|volume` and `catalog rebuild` take `--key FILE`, so an heir with
only the escrow secret restores through tapectl (item 107). `scripts/lto-profile.sh` profiles a
drive and emits its `[[backends.lto]]` block; `post_warning_bytes` caps the slice size (#479).

**2026-10-19: 1.5.0, the fifth read's last findings and the kit's own tapectl**
(`docs/releases/1.5.0.md`; ADR-0012 item 109, #485). File 1 and RECOVERY.md text only; no golden
pin moved and there is no migration. `key escrow-kit` writes a fourth file, `tapectl-<version>`, a
copy of the running binary named on the cover sheet with its sha256 (`escrow_kit::generate`;
tests call `generate_with_program` with a small stand-in), and File 1's "tapectl itself" sends the
heir there first.

**2026-10-21: 1.6.0, the short seal** (`docs/releases/1.6.0.md`; ADR-0012 items 110-120, #420).
A write that reaches the drive's early warning finishes the slice in flight, seals the tape short
after it and exits 0: whole units count as copies, the units after the warning are `cut` (stay
staged; the next `volume write` writes them whole elsewhere), and no unit is ever split. Every
1.6.0 tape's File 0 states `requires = ["short-seal"]`, so a 1.5.0 reader refuses it by name
(item 111). The seal is the last file at END OF DATA (`volume-format-v2.md` §1.2 rule 6, §4); the
catalog records `volumes.ended_after` and `writes.status = 'cut'` (migration 038). Where things
live: the retry past the warning in `tape::ioctl::write_block_retrying`; the session's short
ending in `volume::session`; the chain walk's ending in `store::ConfirmPlan::with_ending`
(`ended_after_disagrees` is inconclusive, never a quarantine); planning past the fill ceiling to
100% of nominal only with `post_warning_bytes` (item 113, `CapacityBudget::split`); a warned write
always reads back the slice at `ended_after` (item 114). The golden pins moved under item 116 and
a third pins the short seal. The test hook `TAPECTL_TEST_EARLY_WARNING_AT` fakes st past a
warning; the gate's `heir_short_*` legs run RESTORE.sh on a short tape. #488's first build rides
it: ring's SHA-256 behind `pass_count::Sha256`, the HP page-0x34 repositions figure in
`write_feed_ratio` events, `TAPECTL_PIPELINE_TRACE=1`, and the `research` cargo feature's knobs.
The acceptance run on home2 (`scripts/short-seal-acceptance.sh`, item 117) passed all seven steps
on 2026-10-22, reading back the slice past the warning and the short seal on the real drive
(`docs/runs/2026-10-21-1.6.0-acceptance.md`); 1.6.0 is tagged `v1.6.0` on `c9c5877`. #488's
drive phase ran on 2026-10-22 with the research build (`docs/runs/2026-10-22-488-drive-phase.md`):
no load reproduced #426, ring is the one lever the figures support, and the excess that did
appear came with the drive's own page 0x02 corrections while the feed was steady.

**2026-10-23: 1.7.0, spanning** (`docs/releases/1.7.0.md`; ADR-0012 items 121-144 on the design of
items 82-95, #478). A unit whose policy sets `allow_spanning` (dotfile > archive set >
`[defaults]`, off when absent) may have its slices on several tapes: spannable units go last, the
drive's early warning ends each tape's *range* (the run of one stage set's slices on one volume,
`writes.status = 'range'`), and the next write lays the owed slices first. Whole units are still
never split. A tape whose plan holds a slice of a spannable set states `requires = ["short-seal",
"spanning"]` (a 1.6.0 reader refuses it); the MANIFEST block states `may_span = true` and lists
every slice, with `tape_position` only for this tape's (`volume-format-v2.md` §1.2, §4.4;
`layout-session.md` "Spanning"). Migration 039: `writes.may_span` (frozen at plan time; rebuild
reads the block), `stage_slices.released_at`, `archive_sets.allow_spanning`. Where things live:
the plan order in `CapacityBudget::place` (`Placement`; `split` is `place` of whole units), wired by
`volume::write::build_span_layout` (a probe build, `place`, the build in plan order, read back);
confirm's completed/range/cut split in `session::span_split`, shared with the adoption of a lost
write; copies per slice (a version is as covered as its least-covered slice, item 127; item 138's
credit) and item 128's promotion in `policy::coverage` (`slice_copies`,
`every_slice_on_sealed_volume_sql`); the per-slice staging release in
`staging::clean::release_covered_slices`; tapectl's restore across cartridges in
`volume::restore::span` over `slice_holders::holders_of_set`; RESTORE.sh's in `restore_across_tapes`
and the `span_*` functions (`span_accept` takes a cartridge only by hash under its seal, R8), with
the one prompt in `swap_loop` (stdin, no timeout, no device held); the rebuild's split in
`rebuild::tape_split` and item 144's promotion in `promote_spanning_version`. `volume rewrite` and
compaction refuse a range volume (`write::refuse_range_holder`, item 133). `audit` adds
`span_in_progress` and four range invariants under `short_seal`; `report failed-stages` is #409's
report (item 91). RESTORE.sh was re-pinned five times (three under item 135, once for the fresh
read of File 1, once under item 145 for its damage handling; `1cd5a405…`), a pin covers
the range block, and the 1.6.0 RESTORE.sh is a test fixture. The coordinator amended fork X12: a
whole unit a queued spannable unit pushes past the fill ceiling waits for the next tape, never
refused (for the CTO, in the release notes). The gate's span leg (item 136), the chain and a fresh
read of File 1 are done. Item 145 fixed RESTORE.sh's damage handling, which GitHub's Ubuntu 26 runner
exposed: a `sha256sum` that quotes names had hidden damaged files from the list. By item 146 the
real-drive proof (item 137, `scripts/span-acceptance.sh`, with a scratch build on home2) comes
before the `v1.7.0` tag, and handoff (h) after it. Item 147 (#492) aligns item 94's start with its
own text in a 1.7.x patch.

**1.7.1: per-location copy targets and auto-stage** (`docs/releases/1.7.1.md`; ADR-0012 items
147-160; #491, #492, #493). A patch: no byte on tape changes. A unit's *copy targets* are a map of
location to count, `copies_at`: the dotfile's `[policy.copies_at]`, an archive set's `copies_at`
(its `required_locations` folded in at one copy each) or `[defaults] copies_at`; the most specific
layer wins whole (`ResolvedPolicy.copies_at`). The dotfile layer is frozen into
`units.copies_at_frozen` at snapshot, stage and adoption (`policy::freeze_copies_at`) and read only
while the dotfile is absent. **The copy floor is `ResolvedPolicy::copy_floor_base`** (the larger
of `min_copies` and the map's sum) through `coverage::copy_floor`; each target is
`coverage::location_target`, and every per-location check asks `coverage::short_of_copies_at`.
Item 158's need at one location is `coverage::NeedAt` (`need_at`, `NeedAt::judge`); `report needs`
is `cli::report::needs_rows`. Auto-stage lives in `src/for_location.rs` (`plan`, `stage`, and
`staged_sets_for_location`, which `volume write --for-location` takes as
`volume::write::StagedScope::ForLocation`); `--keep-staging-for-locations` is
`staging::release::Hold::EveryLocation`. Item 159 as built binds an owed run to a location only
when it starts after slice 1. Item 147 (#492) is `CapacityBudget::place`'s start;
`CapacityBudget::span_last_run` and `collection::selector::after_the_tail` keep the whole units it
displaces in the collection batch (`after_tail`). #491's fix is `volume::write::read_back_span_plan`. Migration 040.
Item 26's shared-place warning judges where a moved tape goes (`--for-location X`, `--move-to Y`):
`volume::write::WriteScope` carries the destination beside the `StagedScope`, and
`coverage::copy_distinctness_at` takes it; `stage for-location`'s plan runs the same check.

**Real LTO-6 hardware validation is DONE** (2026-09-10), no longer deferred: an HP
LTO-6 was passed through to this VM (`docs/lto6-drive-passthrough.md`) and was used
for a full validation session (`docs/lto6-session-journal-2026-09-10.md`). The §5
open hardware questions are answered there — block size 512 K vs 1 M is a wash, MAM
over-report is +2 MiB. `scripts/lifecycle-suite.sh` (17 scenarios, `--list` names
them, x a 12-method restore matrix) is the permutation suite built from it.

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

Two suites are gated (they skip at runtime unless the env var is set):

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
# gating; it writes the loaded cartridge. The gate runs it as its last leg, rust_motions.
TAPECTL_GATE_TAPE="$(readlink -f /dev/tape/by-id/scsi-XYZZY_A2-nst)" TAPECTL_MHVTL=1 \
    cargo test --test mhvtl_motions -- --ignored --nocapture

# Performance scenarios (100k-file unit, 2 GiB write, 184k-file rebuild); ~3 min. The one
# case a release build is expected.
TAPECTL_PERF_TESTS=1 cargo test --test performance --release -- \
    --ignored --nocapture --test-threads=1
```

**The heir path's only test with real tools is the mhvtl gate** (ADR-0012 item 70,
#457). `tests/heir_restore_sh.rs` runs RESTORE.sh with `mt`, `dd` and `age` faked (CI
has no age CLI); only the gate's RESTORE.sh legs (`heir_*`) and home2's real-drive
proof run it with the real `age`, `dar`, `mt` and `tar`. A release is not proven on
the heir path until the gate has run; each run record names the age version it used
(`age --version`). Since 1.3.0 the gate's heir legs also run RESTORE.sh's index modes
(`heir_where`, `heir_find`), its slice replacement (`heir_slice_from_copy`) and its per-file
sha256 check (`heir_file_check`) with the real tools. The gate's Rust legs pass only when every test they name ran and
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
- `sg3-utils` — drive health pages (`sg_logs`), the cartridge's MAM (`sg_read_attr`), drive identity (`sg_inq`)
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
