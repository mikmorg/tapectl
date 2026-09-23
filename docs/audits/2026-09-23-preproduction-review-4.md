# Fourth pre-production adversarial review — 2026-09-23

**Range:** `bf6f57d..cf4a371` (80 commits, about 23.7k insertions, most of them sg_logs fixtures). Everything
landed since the third review was recorded: the ADR-0013 forensics work (#293-#298, #312-#322),
resume adoption of aborted sessions (#280), quarantine atomicity (#324), the real-drive harness
hardening (#321), the #182/#323 measurements, and #325/#327. It ran when the label was down to
#323 alone (parked, needs:cto), before the CTO's real-drive rehearsal.

**Method:** a 20-agent workflow. Eight dimension finders, each drawn from this project's recent
misses (forensic read-once, contact/drive identity, consent/ADR-0003, operator text naming commands,
tests that cannot fail, the harness, schema/migrations, restore/read paths). One adversarial
verifier per finding, told to REFUTE and to default to refuted when unsure. A completeness critic.
All agents were read-only: no builds, no tape, no scripts executed, no device nodes.

**Result: 11 raw findings from the finders, 9 survived, 2 refuted; the critic added 2.** Filed as
**#328-#337** (4 medium, 6 low). One critic finding was fixed directly instead (below). Reconciled:
9 + 2 = 11 = 10 filed + 1 fixed. Nothing was dropped.

## Filed

| # | Sev | Finding |
|---|-----|---------|
| [#328](https://github.com/mikmorg/tapectl/issues/328) | medium | log page sweep: sg_logs runs without --maxlen, so every page read (0x2E included) is TWO LOG SENSE commands — ADR-0013's read-once rule is broken at the device |
| [#329](https://github.com/mikmorg/tapectl/issues/329) | medium | a contact's drive is identified from device_tape (sysfs VPD) but its MAM and log pages are read from device_sg, and nothing checks both nodes are the same drive |
| [#330](https://github.com/mikmorg/tapectl/issues/330) | medium | a fresh `volume write` can overwrite a volume whose seal is RECORDED (sealed_at) once its session is aborted and the seal marker reads badly — no --force (ADR-0003) |
| [#331](https://github.com/mikmorg/tapectl/issues/331) | low | volume abort's consent/done texts promise `volume verify` then `volume resume` (and 'stays SEALED with every byte') from sealed_at alone; resume refuses a retired/erased volume |
| [#332](https://github.com/mikmorg/tapectl/issues/332) | low | test the_operation_vocabulary_is_what_code_actually_writes cannot fail for Operation::VolumeCompact (substring scan is satisfied by VolumeCompactRead, test modules and comments) |
| [#333](https://github.com/mikmorg/tapectl/issues/333) | low | lifecycle-suite permute: the copy-count check's warehouse-deposit guard reads a JSON key `catalog locate --json` never emits, so it can never fire |
| [#334](https://github.com/mikmorg/tapectl/issues/334) | medium | lifecycle-suite's real-drive branch takes basename of --device as given, so the by-id path CLAUDE.md prescribes for the real drive is refused ('cannot find ... in lsscsi') |
| [#335](https://github.com/mikmorg/tapectl/issues/335) | low | catalog rebuild's contact row keeps cartridge_id NULL and 'medium serial matches no registered cartridge' after the same rebuild registers the cartridge or learns its serial |
| [#336](https://github.com/mikmorg/tapectl/issues/336) | low | report verify-status lists erased/blank/initialized/retired volumes as 'never verified' and sorts them first (#293 regression) |
| [#337](https://github.com/mikmorg/tapectl/issues/337) | low | collection sync exits 0 when an UNREGISTERED unit directory's dotfile fails to parse — the unit is never registered, against ADR-0012's 'refused and non-zero' ruling |

Severity notes, where the coordinator departed from a verifier:
- **#330** was rated low by its verifier for its preconditions. It is filed medium because every step is
  a path the range's own texts sanction, and the outcome is ADR-0003's forbidden one (#208's class).
- **#334** is raised to medium because the CTO rehearsal depends on it. It fails closed, but
  it forces the real drive to be named `/dev/nstN`, which is the numbering hazard.
- **#328** is medium, not high: the evidence loss needs the drive to clear TapeAlert on the 4-byte
  header fetch, which is plausible but not observed. It is the structural ADR-0013 rule, though,
  and the fix is one argument, so it lands before the rehearsal.

## Fixed directly: a claim the coordinator made

The critic found that `docs/runs/2026-09-23-lto6-capacity-measurement.md` (as corrected in
`cf4a371`) said the tape-streaming loop does no hashing. It does: `session.rs` `execute` wraps every staged
file in `util::HashingReader` on the same thread as the tape write. This is tri-layer L2
(`v2-open-questions.md` §2.4), whose premise, "hash is free once streaming lands", does not hold on
this CPU (no SHA-NI). The coordinator had told the CTO the opposite in reply to a direct question.
Corrected in `c4f0c6d`; the design question is on #326. **Lesson:** answer a "does the code do X"
question by tracing the caller chain, not the innermost loop. `write_stream` really does no hashing;
its caller hands it a hashing reader.

## Refuted

- **The adoption predicate's 'verify started strictly after the abort' compares against a `started_at` that `volume verify` stamps at completion** (consent-adr0003). The mechanics check out. volume_verify inserts its verification_sessions row after store.confirm returns (src/volume/write.rs:3446 then 3460-3465) and never supplies started_at, so the column default puts started_at at completion time. aborted_adoption compares against started_at (src/volume/session.rs:733-741). But this is not a defect against the ratified design. The ADR-0012 2026-09-23 amendment, condition 2 (docs/adr/0012...md:802-806), requires "a passing full verify of this volume is recorded **after** the session was aborted", and says "the evidence must be a recorded row". It never req
- **stage create's INTERRUPTED refusal tells the operator to reload the cartridge and `volume resume L` even when L is retired/erased and resume refuses** (operator-text). The path can happen. Displacement (binding.rs:910-915), volume retire and mark-erased all leave 'interrupted' writes rows alone, and release_blocker (clean.rs:178-182) does not check the volume's status. But what the operator ends up with is not a wrong or harmful outcome.

(1) The refusal at src/cli/stage.rs:402-418 gives two ways forward in the same sentence: "Reload the same cartridge and resume it, OR write these slices to another volume". Both of the other ways it names work for an erased or retired volume:
- Writing the slices to another volume is allowed. The unfinished-session gate in

## Critic

**Areas it said no dimension reached:**
- src/db/mod.rs (+1004): only the migration SQL files were within the schema-migrations dimension. I did not read the Rust-side changes to db::open, the crash sweep or the queries.
- src/main.rs exit-code plumbing for the new Result<i32> collection arms: not read line by line
- src/cli/report.rs beyond the verify-status finding already reported (807 lines changed, including temperature-history and journal reports)
- scripts/first-run.sh, scripts/lto6-measure.sh, scripts/mhvtl-device.sh: left to the harness-scripts dimension, not re-read here
- tests/man_matches_help.rs and tests/cli_smoke.rs: not checked for vacuous assertions
- .claude/commands/autopilot.md: process doc, out of scope

**Verdict:** The range is trustworthy enough for the CTO's real-drive rehearsal, but not yet for a first production write. The five medium/low code findings already on record should be fixed first:
- sg_logs runs without --maxlen, so each page read, 0x2E included, costs two LOG SENSE commands.
- A contact row's drive identity comes from the tape node, while its MAM and log pages come from the sg node, and nothing checks the two are the same drive.
- A sealed-then-aborted tape can be overwritten without --force.
- Abort's text promises a verify-and-resume path that resume will refuse.
- `report verify-status` now lists erased, blank, initialized and retired volumes as 'never verified' and sorts them first.

The two in-range /dev/sg findings (the doubled LOG SENSE and the identity split) touch ADR-0013's read-once and spine rules directly, and the rehearsal is the first place either can be seen.

The areas no dimension reached are sound. That covers the collection per-unit refusal plumbing, the version-scoped staging clean with its release-blocker wording, the widened Tier-3 floor in policy/coverage, which does not inflate the copy count, the cartridge MAM journal CLI, and the multi-key RESTORE.sh. They add only one low finding: an unregistered unit with a bad dotfile exits 0 from `collection sync`.

The main thing the dimensions could not see is the last commit. It edits the capacity-measurement run journal to rule hashing out of the 56 MB/s write rate. The code hashes every streamed byte inline, in the same thread as the tape write (session.rs:1780 → store.rs:749 → util.rs:44). That document shapes the rehearsal's throughput expectations and the full-cartridge time budget, so correct it before the rehearsal.

No finding in any of the areas I reviewed threatens bytes on tape. Staging-release scope, the refusals and consent-ordering on the write path, and the heir restore script all held up.

**Checked and clean, stated positively:** Areas the eight dimensions did not plausibly reach. I read each diff and found them clean.
- **src/collection/batch.rs (#284):** release_if_covered is extracted and still called from execute_batch. The scope stays CleanScope::Units of this batch only. Its source-scan test bounds its own extraction.
- **src/collection/fingerprint.rs, plan.rs, status.rs, sync.rs (#285):**
  - walk_fingerprint's only fallible step is effective_compiled. compile() cannot fail, so every refusal really is a dotfile fault. classify_from_walk still hard-fails on DB errors.
  - collection run's batches never contain a refused unit.
  - The refused list shows in both JSON and plain text for all four commands, including a collection whose entire pending set was refused.
- **src/unit/dotfile.rs:** errors now carry the file path.
- **src/staging/clean.rs + src/cli/staging.rs (#278 --version):**
  - UnitVersion binds its two parameters in the right order and does not narrow the 'failed' sweep.
  - With --version, the else-branch (under-copied) cannot widen the scope, because unit_scope is exactly one id.
  - --version with zero or several --unit values is refused.
- **release_blocker (#279/#325):** ranking and labels are correct for all five states.
- **src/cli/stage.rs:** the live-slices refusal is correct for all four blocker states.
- **src/cli/audit.rs restage_action:** it emits a version-scoped clean, forced only when the unit is under-copied. Its tests run the emitted scope against the DB.
- **src/policy/coverage.rs:** holds_sealed_bytes and write_reaches_tape widen only versions_at_stake (the Tier-3 floor). copy_count_expr, and so audit and report copies, still see only the eligible, sealed volumes. A test pins that.
- **src/cli/cartridge.rs journal:** the barcode selector joins through cartridge_contacts or the known serial. --raw is refused for a row outside the selection.
- **src/volume/layout.rs RESTORE.sh (#288/#291/#218):**
  - Every key file is checked before the first tape read.
  - Keys are tried independently for the envelope and for each slice, starting with the key that opened the previous slice.
  - One die_no_envelope removes the '\\'' quoting garbage.
  - The #127 skip-to-next-position logic is kept.
  - MiB is correct.

Cross-cutting checks:
- **Contact bookkeeping:** it is infallible and inserted outside any transaction, so it cannot refuse a tape command.
- **Health on a failed write:** volume_write collects health after finish_session even when execution fails. A store error (ENOSPC or I/O) becomes an abort outcome, not an early `?`, so 0x2E is still swept on the failure contact.
- **Early refusals:** refusals before the MAM read open no contact.
- **Health sweep:** one per write contact, attached via contact.id().
