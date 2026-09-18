# Pre-production adversarial review #2 — 2026-09-18

**Range:** `c309509..HEAD` (105 commits, ~15,100 non-doc insertions across 56 files) — everything
landed since the 2026-09-17 review was recorded, which that review therefore never saw.

**Trigger:** Policy rule 7, re-run on CTO ruling after the `review-2026-09-13` label emptied a second time.

**Shape:** 13 agents — six dimension finders, one adversarial verifier per dimension (told to REFUTE,
default to not-a-defect), one completeness critic. 2.2M tokens, 703 tool calls, 0 agent errors.

**Result: 35 raw findings, 20 confirmed, 15 refuted (43% killed), 8 critic gaps.**

## Why these six dimensions

The 2026-09-17 review's own lesson was that the dimensions which paid were drawn from what the session
kept actually finding, not from generic categories. All six here came from real misses in the 24 hours
before the run:

| Dimension | The miss it came from |
|---|---|
| constraints-vs-history | migration 017 would have restored a status its own new CHECK forbids |
| stale-claims | `cp_release_staging`'s "No coverage gate is involved", false since #244, above the call that now refuses |
| tests-that-prove-nothing | six green-for-the-wrong-reason results on this project |
| single-derivation | #96's five drifted copies of one status list; #242 adding a second column every predicate must consult |
| refusal-ordering-and-consent | refusals that fire after a side effect make their own tests false passes |
| harness-product-drift | three correct changes reddened harness assertions that pinned a sentence, not a rule |

## Confirmed findings

### [MEDIUM] `collection run`'s new min_copies release gate is batch-scoped, but the release it guards is database-wide
`src/collection/batch.rs:205` — dimension: refusal-ordering-and-consent

`execute_batch` computes `under_copied_units(conn, config, batch)` over only the units in the batch it just wrote, then, when that list is empty, calls `clean_staging(conn, config, false)` — a function whose candidate SQL has no batch, unit or collection filter and sweeps every eligible `stage_sets` row in the database. The gate ADR-0012's #244 amendment relies on therefore protects a strictly narrower set than the act it authorises, so a `collection run` can release staged bytes belonging to units it never looked at.

**Failure scenario.** Unit `docs` resolves the default `min_copies = 2` (src/config.rs:346 `default_min_copies`). It is staged and written once; its stage set is `'staged'` with one `completed` write, deliberately retained so the operator can follow the printed recipe "swap in the next cartridge and run `tapectl volume write <label>`". A separate collection `media` is bound to an `[[archive_sets]]` entry with `min_copies = 1`. The operator runs `tapectl collection run --collection media --batch 0 --label MED1`. Every unit in that batch now has 1 copy, meeting its resolved `min_copies` of 1, so `under_copied` is empty and `clean_staging(force=false)` runs. `docs`'s stage set satisfies the unfiltered candidate SQL (one `writes` row, `completed`), so its `staging_path`s are NULLed and its `.dar.age` slices unlinked. The recipe's `tapectl volume write VOL2` then finds nothing (`find_staged_data` selects only `status = 'staged'`), and recovery is a full re-stage from source — impossible for a `mark-tape-only` unit. `tapectl staging clean` at that same moment would have refused, naming `docs: 1/2 copies`. A second reachable shape needs no heterogeneous policy: if every unit in the batch hits `execute_batch`'s documented no-op arms (`(false, "staged")` / `(false, "current")` at src/collection/batch.rs:161-176) nothing is written, `under_copied_units` is empty for want of a shortfall, and the same global sweep fires. `cmd_run --dry-run` cannot warn about either: src/cli/collection.rs:399-424 returns before `execute_batch` and never computes the release.

### [MEDIUM] `audit --action-plan` emits a bare `tapectl staging clean`, and its doc comment still guarantees that release which issue #244 made refusable
`src/cli/audit.rs:596` — dimension: stale-claims

`restage_action`'s doc comment asserts "so a bare `staging clean` (no `--force`) is guaranteed to reclaim it", and the function emits a bare `tapectl staging clean` as the first step of an `&&`-joined operator remedy. Issue #244 (commit 8e0b157, landed LATER in this same diff) made a bare `staging clean` refuse the whole command whenever any release candidate's unit is below its resolved `min_copies`. The comment was true when written for #209 and is the opposite of true now — the exact defect class #209 existed to close, reintroduced.

**Failure scenario.** Archive with `min_copies = 2` (the guide's default). Unit `photos` has an unencrypted or escrow-gapped current stage set still `'staged'` with one completed write, so `check_encryption`/`check_escrow_coverage` fires and `restage_action` is reached with `any_live == true`. `tapectl audit --action-plan` prints `tapectl staging clean && tapectl stage create photos --version 1 && tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>`. The operator pastes it; `staging clean` exits non-zero with "staging clean refused: ... photos: 1/2 copies ... Pass --force", `&&` short-circuits, and none of the remedy runs. The same happens when an unrelated unit elsewhere in the archive is the under-copied one.

### [MEDIUM] `cartridge unretire` restores `volumes.status` from `events.old_value` with no legal-set filter — migration 017's new CHECK can reject it
`src/cli/operations.rs:1600` — dimension: constraints-vs-history

Migration 017 removed `'quarantined'` from the `volumes.status` CHECK, but `cartridge_unretire` still reads a volume's pre-retirement status straight out of the `events` audit trail and writes it back into `status` unfiltered. That trail was written by pre-017 code that could legally record `old_value = 'quarantined'`, so the restore can attempt a value the new CHECK forbids. This is the exact defect review caught in 017 itself (whose header now filters its own subquery to the legal set) — 017 fixed one reader of that trail and missed the sibling reader.

**Failure scenario.** A volume is quarantined by a failed `volume verify` (pre-017 this wrote `volumes.status = 'quarantined'`). The operator retires its cartridge — which is the intended use of that state: `tier3_does_not_fire_for_a_quarantined_volume_on_the_cartridge` documents that the ADR-0008 Tier-3 floor deliberately stays out of the way because a quarantined volume "counts for nothing", so a quarantined tape is exactly the tape whose cartridge gets retired. `cartridge_retire` logs `events(entity_type='volume', action='retired', field='status', old_value='quarantined')` and sets the volume `retired`. Migration 017 then runs: it only rewrites rows whose status IS 'quarantined', so this row (now 'retired') is untouched and the event survives verbatim. The operator realises the retirement was a mistake and runs `tapectl cartridge unretire <barcode>` — the command every refusal text names as the way out (src/volume/binding.rs:534 "with `tapectl cartridge unretire {}`, which is the way back"; src/cli/operations.rs:1154 and :1256). The read at :1600 returns 'quarantined', the UPDATE at :1676 hits `CHECK constraint failed: volumes.status IN (...)`, the `?` propagates, `tx` drops unconnitted so the whole unretire rolls back, and the operator is left with a `retired_permanent` cartridge, retired volumes, and a raw SQLite error from the one command documented as the escape. The window is concrete and recent: `cartridge retire`/`unretire` landed with ADR-0011 on 2026-09-13 and 017 landed 2026-09-17, so any live catalog — or any `db backup` file from those four days, brought forward by `db import` — can carry the event. Fix shape: constrain the subquery to the legal set the way 017 does, and map a recovered 'quarantined' onto 017's own translation (`observed_condition = 'quarantined'`, `status` from an earlier legal event or the documented fallback) rather than dropping the fact.

### [MEDIUM] #244's staging-clean gate refuses the bare `staging clean` that `audit --action-plan` prints as its own remedy — and a test in the same diff pins the now-false claim
`src/cli/audit.rs:620` — dimension: harness-product-drift

`restage_action` (the shared remedy for the `escrow_coverage` and `encryption` audit checks) prefixes its action string with a bare `tapectl staging clean`, justified by a doc comment asserting that such a clean is "guaranteed to reclaim it". Commit 8e0b157 (#244) landed a gate in `cli::staging` that refuses a bare `staging clean` for exactly that state. Nobody enumerated this call site: the remedy the audit prints now dead-ends on its first command. This is the #244 shape the review brief names, living inside the product rather than inside scripts/.

**Failure scenario.** Default config (`min_copies_for_tape_only = 2`). Unit `photos` v1 is staged and written to one sealed volume; its stage set is still `'staged'`; the stage set is unencrypted (or lacks escrow coverage), so `check_encryption`/`check_escrow_coverage` fires. `tapectl audit --action-plan` prints `tapectl staging clean && tapectl stage create photos --version 1 && tapectl volume init <OTHER-LABEL> && tapectl volume write <OTHER-LABEL>`. The operator pastes it; the first command exits non-zero with "staging clean refused: ... photos: 1/2 copies ... Pass --force", the `&&` chain stops, and the violation the audit just told them how to fix is never fixed. The operator is back in issue #209's loop — the exact defect class this diff spent five commits closing.

### [MEDIUM] permute's end-of-walk restore matrix is always skipped: it looks up a JSON key `catalog locate` does not emit
`scripts/lifecycle-suite.sh:3140` — dimension: tests-that-prove-nothing

Predates c309509 (introduced in a30e284); reported because the review brief names the `permute` scenario and this is the same green-for-the-wrong-reason shape the project has hit six times. `scenario_permute` picks the volume to restore from by reading `label` out of `catalog locate --json`, but that command serialises the field as `volume`. The extraction therefore never matches any written label, `last` stays empty, and every unit's final 10-method restore matrix is replaced by a SKIP that reads "unit X never ended up on any volume this walk" — even after the walk wrote volumes carrying it.

**Failure scenario.** Run `scripts/lifecycle-suite.sh --scenario permute` on mhvtl. The walk draws `write-next-volume` and writes VOL-PM1 carrying photos/docs/big. At the end, `catalog locate photos --json` returns `[{"volume":"VOL-PM1",...}]`; `labels` becomes the line `{'volume': 'VOL-PM1', 'status': 'sealed', ...}`; `grep -qx VOL-PM1` fails; `last` stays empty; `pm-final-photos.unit` is recorded as SKIP "never ended up on any volume this walk". The randomised walk's only end-state restore verification never runs, for any unit, at any seed, and the run still reports green.

### [MEDIUM] pm_final_copy_count_is_honest compares audit's copy count against a structurally-zero expectation
`scripts/lifecycle-suite.sh:3056` — dimension: tests-that-prove-nothing

Predates c309509 (a30e284), same root cause as the finding above. The check that replaced the abandoned seed-dependent per-step copy_count assertion computes its expected count from the same wrong JSON key, so `expected` is 0 on every unit, every seed, every run. The check therefore has only two outcomes — a vacuous pass when audit raises no copy_count finding, or a false red when it raises one — and never performs the comparison its comment documents.

**Failure scenario.** Walk ends with every unit at 2 copies against min_copies=2: audit raises no copy_count finding, `claimed` is "none", the check passes having compared nothing — the branch it advertises ("does tapectl's copy count AGREE with the volumes this walk actually wrote?") is never taken. Walk ends with photos at 1 copy: audit reports "has 1 copies, needs 2", `claimed`=1, `expected`=0, and the check fails with "copy_count disagreement for photos: audit says 1, this walk wrote 0 volume(s) carrying it" — a false red blaming the catalog for the harness's own key error.

### [LOW] `volume list --status` help advertises `quarantined` as an accepted value; the command refuses it outright
`src/cli/volume.rs:289` — dimension: stale-claims

The clap help for `volume list --status` lists `quarantined` in its accepted-value set. Migration 017 removed `quarantined` from `volumes.status`'s CHECK (issue #242), and `validate_volume_status` now refuses that exact string with a bespoke error. The generated man page carries the same stale list. The sibling command got this right, which makes the omission concrete rather than debatable.

**Failure scenario.** Operator runs `tapectl volume list --help`, reads `quarantined` in the accepted list, runs `tapectl volume list --status quarantined` to inventory suspect media, and gets a usage error instead of a list. The help never points them at the CONDITION column that actually carries the fact.

### [LOW] `SealedPending::confirm`'s doc comment still says a failed confirm sets `volumes` 'quarantined'; the code writes `observed_condition`
`src/volume/session.rs:929` — dimension: stale-claims

The method doc directly above the confirm implementation states the failure branch as "fail => `volumes` 'quarantined'", in a sentence whose other three clauses (`writes` 'completed', `snapshots` 'current', `volumes` 'sealed') are all status values — so it reads as `volumes.status = 'quarantined'`. Migration 017 removed that status value entirely and the code 130 lines below writes `observed_condition`. The module-level flow block at the top of the same file WAS corrected; this one was missed.

**Failure scenario.** A maintainer adding a fourth quarantine writer, or writing a query for "which volumes did confirm condemn", reads this method's doc (the natural place to look, since it owns the transition) and filters `volumes.status = 'quarantined'`. That predicate matches nothing after migration 017, so the quarantine is silently invisible — the same coverage-misstatement class ADR-0012 item 1 names as load-bearing.

### [LOW] operator-guide's "stop, run `staging clean`" recipe no longer runs under issue #244's gate
`docs/operator-guide.md:242` — dimension: stale-claims

The guide's answer to "`volume write` is about to write more than you meant" is to stop and run `staging clean` to release what is already on tape. Since #244 a bare `staging clean` refuses the entire command — not just the offending set — whenever any release candidate's unit is below its resolved `min_copies`, which is precisely the state of a stage set that has been written to only one of two required cartridges. The guide mentions neither the gate, the refusal, nor `--force`, anywhere.

**Failure scenario.** Operator with the guide's own `min_copies_for_tape_only = 2`, one leftover stage set written to exactly one cartridge, runs `tapectl volume write`, sees the unwanted unit in the announcement, aborts, and follows the guide: `tapectl staging clean` exits non-zero refusing to release anything. The guide gives them no next step, so they are stuck between a write they do not want and a release the tool refuses.

### [LOW] `policy::coverage`'s module doc still lists `quarantined` among the values `volumes.status` moves to
`src/policy/coverage.rs:10` — dimension: stale-claims

The module header of the file that ADR-0012 declares sole owner of every `volumes.status`/`observed_condition` predicate still names `quarantined` as a `volumes.status` value, citing migration 003 for the list. Migration 017 removed it from that CHECK; every function body in this same file was updated to consult `observed_condition` instead, and the individual doc comments say so — only the header that a first-time reader reads first was left behind.

**Failure scenario.** A maintainer reads the module header to learn what the status column can hold, writes `status IN ('sealed','quarantined')` in a new derivation, and the `quarantined` arm silently matches zero rows — the coverage-misstatement class this module exists to prevent.

### [LOW] `db fsck`'s comment claims the schema-pending note is shown on a dry run; the human dry-run branch never prints it
`src/cli/db.rs:68` — dimension: stale-claims

The comment justifying the unconditional `schema_is_current` check states the note is "Meaningful on a dry run too" and that the tool must "say so here". The `--json` path does carry `schema_pending`, but the human-readable dry-run branch prints only the DRY RUN line and the issue list, and returns before ever reaching the `if schema_pending` block, which lives exclusively in the non-dry `else`.

**Failure scenario.** On an FK-broken database, `tapectl db fsck --repair --dry-run` reaches `db::open_for_repair` (src/main.rs:219-228 matches `Fsck { repair: true }` regardless of `--dry-run`). The preview prints its findings with no mention that the connection is behind head, so the operator plans the real repair believing the schema state is current — the exact confusion the comment says it is preventing.

### [LOW] README (and CLAUDE.md) still say the lifecycle suite has 13 scenarios; this diff brought it to 16
`README.md:225` — dimension: stale-claims

The README's Testing section states the lifecycle suite covers 13 scenarios. This same diff added three (`stale-catalog-sealed-tape`, `cartridge-displacement`, `collection-second-copy`), taking the registry to 16. The same README edit correctly updated "12 report types" to "13" twice, so the count was being maintained — this one was missed.

**Failure scenario.** Someone budgeting a pre-production mhvtl run, or checking that `--all` covered everything, counts 16 scenario names against the README's 13 and cannot tell whether three are new or three are undocumented leftovers. CLAUDE.md is the agent-facing pointer, so an agent inherits the same wrong figure.

### [LOW] The global `--dry-run` help promises a preview on ~27 leaves that now refuse the flag
`src/cli/mod.rs:32` — dimension: stale-claims

`--dry-run` is `#[arg(long, global = true)]` with the help "Show what would be done without making changes". Roughly 27 leaves now refuse it outright via `refuse_dry_run`, yet clap still renders the flag — with that promise — into every one of those commands' `--help` and into every generated man page's SYNOPSIS and OPTIONS. The project's own test file names this as the defect it is fixing, but only for the code path, not the help text.

**Failure scenario.** Operator runs `tapectl staging clean --help` (or `man tapectl-staging-clean`), sees `--dry-run` listed with "Show what would be done without making changes", and runs `tapectl staging clean --dry-run` expecting a preview of what would be released. The command exits non-zero. Harmless once, but the same shape reaches `volume write`, `catalog rebuild` and `archive-set sync`, where an operator scripting around a promised preview finds it does not exist.

### [LOW] `volume list --status` help still advertises `quarantined`, a value the command now refuses
`src/cli/volume.rs:288` — dimension: constraints-vs-history

Migration 017 removed `quarantined` from `volumes.status` and `validate_volume_status` now rejects it as a `--status` argument with a dedicated error, but the clap help text for the flag — and therefore the generated man page — still lists it among the accepted values.

**Failure scenario.** An operator runs `tapectl volume list --help` (or reads `man tapectl-volume-list`), sees `quarantined` listed as a legal value, types `tapectl volume list --status quarantined`, and gets an error instead of a result. The error text is good and explains where the fact moved, so the cost is a contradiction between the tool's own help and its behaviour rather than a wrong answer — but the man page is a build artifact of this help string, so regenerating docs propagates the stale value rather than fixing it.

### [LOW] `volume deposit add` consults status but not observed_condition, so a verify-quarantined tape now passes a refusal that used to catch it
`src/cli/volume.rs:1244` — dimension: single-derivation

The deposit refusal reads only `status` and tests `status != "sealed"`. Migration 017 moved quarantine off `status` onto `observed_condition`, so a volume a failed `volume verify` proved unreadable now reads `status = 'sealed'` and is accepted — recording a durable warehouse-copy claim for a medium tapectl has observed to be bad. This is a hand-written `volumes.status` predicate outside `policy/`, the exact thing CLAUDE.md and coverage.rs:75-78 forbid ("Never inlined outside this module").

**Failure scenario.** `tapectl volume verify L6-0007` fails with medium-proving mismatches → `quarantine_on_medium_evidence` sets `observed_condition = 'quarantined'` and leaves `status = 'sealed'`. The operator then runs `tapectl volume deposit add L6-0007 --to glacier`. Before migration 017 the row read `status = 'quarantined'` and the command refused. It now succeeds, inserting a `volume_deposits` row. `catalog locate` renders it in the Warehouse column from an unfiltered subquery (src/cli/catalog.rs:308-311), so the row reads `Serviceable = NO` beside `Warehouse = glacier` — a custody claim for a tape tapectl says cannot be read. (The copy count itself is safe: `scoped_deposits` gates the source volume on `eligible` at src/policy/coverage.rs:321-327, so the deposit never counts. Scope note: only verify-quarantines reach this; a write-path quarantine leaves `status = 'initialized'`, which this predicate still refuses.)

### [LOW] Migration 017's status-restore fallback contradicts ADR-0012 point 4, which was never amended
`src/db/migrations/017_volume_observed_condition.sql:46` — dimension: single-derivation

ADR-0012's ratified amendment says a migrating quarantined row's status is restored "to `sealed` where [the events row does] not [record it]". Migration 017 restores `'initialized'` instead and says so in its own header. The code's choice is the correct one — the ADR's stated justification is factually wrong — but the ADR text was not amended, so the ratified document and the shipped migration disagree.

**Failure scenario.** A future worker reads ADR-0012 point 4 as normative (the stated authority order puts docs/adr/ above the code), concludes migration 017 is buggy, and "fixes" the fallback to `'sealed'`. A volume whose write session was quarantined mid-flight — never sealed, no seal marker, no front index on tape — then claims `status = 'sealed'`, which ADR-0003 reads as "immutable, never written again" and CONTEXT.md:75-81 defines as the tape being self-describing and complete. The catalog would assert on-tape completeness for a tape that has none.

### [LOW] csc_fingerprint's "every set must still be LIVE" guard is inverted and can never fire
`scripts/lifecycle-suite.sh:3693` — dimension: tests-that-prove-nothing

The guard that the `collection-second-copy` scenario added specifically to stop `csc.same_staged_bytes` passing on two identical records of nothing is written as `grep -vq PATTERN || fail`, which fails only when EVERY line of the fingerprint matches ` cleaned id=`. The fingerprint always contains `info ...` and `  slice ...` lines that cannot match that pattern, so the guard's failure branch is unreachable in every state. The fully-vacuous case is caught only incidentally by the next line, and a partially-released state is not caught at all.

**Failure scenario.** A regression releases staging for 2 of the 3 `media/*` units after the first copy (exactly the pre-#238 defect this scenario guards). Both `csc.fingerprint.before` and `csc.fingerprint.after` then record those two sets as `cleaned id=`, with identical (stale) slice hashes. Line 3693 passes because `info`/`slice` lines do not match the pattern; line 3698 passes because the third set is still ` staged id=`; `diff -u` at 3714 is empty, so `csc.same_staged_bytes` reports PASS while the property it exists to prove — that the second copy consumed the same live staged bytes — is false for two of the three units. The comment records that a measured negative control once produced exactly this false pass; re-running it today would be stopped by line 3698 returning 1 with no diagnostic, making the inverted guard look as though it worked.

### [LOW] the_resolution_is_deterministic_across_the_whole_table asserts f(x) == f(x) on a pure function
`src/startup.rs:647` — dimension: tests-that-prove-nothing

The test calls `resolve_from` twice with identical arguments and asserts the two results are equal. `resolve_from` takes every environment input as an explicit parameter and performs no I/O, no clock read and no randomness, so the assertion is a tautology: no implementation that compiles can fail it. Its doc claims it proves main's two resolution call sites cannot diverge, which it does not check at all.

**Failure scenario.** Rewrite `resolve_from`'s body to return an arbitrary wrong home for every row — e.g. make `named_home` always `PathBuf::from("/wrong")` — and this test still passes: both calls return the same wrong value. The nine table rows it enumerates are never compared against any expected resolution, so the only thing that would ever turn it red is nondeterminism the function is structurally incapable of.

### [LOW] restore_unit dry-run test claims to check the slice count with an assertion any output satisfies
`tests/dry_run_global.rs:1359` — dimension: tests-that-prove-nothing

The assertion is `text.contains("would restore") && text.contains('1')` with the failure message "dry-run output does not name the preview or the slice count". The fixture's unit is named `u1` and its volume `L6-0001`, both of which appear in the same line and both of which contain the character '1', so the second conjunct is satisfied no matter what slice count is printed — or whether one is printed at all.

**Failure scenario.** Change the preview to `"would restore \"{}\" from {} to {}"` — dropping the slice count entirely, or printing it as 0 — and the test still passes, because `"would restore \"u1\" from L6-0001 to ..."` contains both `would restore` and '1'. The only property actually pinned is that the two literal words appear, not that the preview reports how many slices the real run would read back.

### [LOW] volume_abort test doc explains the behaviour via a clap field this same diff deleted
`tests/cli_smoke.rs:1795` — dimension: tests-that-prove-nothing

The doc for `volume_abort_proceeds_on_global_yes_alone` explains the pass as `Abort`'s local `yes` field sharing clap's arg id with the global `Cli::yes`, and states that `src/cli/volume.rs` was deliberately not changed. Commit e3e39ba (issue #240), later in this same diff, removed that local field entirely; the sibling test's doc was updated to say so and this one's was not. The assertion still checks the right thing, but the reader is told the test guards a mechanism that no longer exists.

**Failure scenario.** A maintainer investigating a future `--yes` regression reads this doc, looks for the local `Abort.yes` field it names as the load-bearing mechanism, does not find it, and either concludes the test is obsolete and deletes it or re-adds a local `yes` field to match the doc — reintroducing exactly the `-y`-after-the-subcommand parse failure that issue #240 removed.

## Completeness-critic gaps

What no dimension reached. The previous review's critic earned its place by finding four unexamined
subsystems including `scripts/`; this one was pointed at that lesson explicitly.

### [HIGH] tests/mhvtl_e2e.rs is untouched by all 105 commits — the only on-media producer of medium evidence never asserts the verify path's new destructive database effect
`tests/mhvtl_e2e.rs:1518`

This diff gave `volume verify` a new, irreversible side effect (issue #234/#242: a failed verify writes `volumes.observed_condition = 'quarantined'` plus an `events` row, and adds `VerifyReport::quarantine`). The one test in the repo that drives a genuinely corrupted tape through the production entry point was not updated and asserts nothing about any of it.

**Failure scenario.** An operator runs `volume verify` on a real LTO-6 cartridge with a marginal block. The verify reports a hash mismatch, `quarantine_on_medium_evidence` flips `observed_condition` to 'quarantined', and `policy::coverage::in_service`/`eligible` (src/policy/coverage.rs:80, 776) immediately stop counting that cartridge as a copy — silently taking a unit from 2 copies to 1, or 1 to 0, in `audit`, `report copies` and the Tier-3 retire floor. No command sets `observed_condition` back to 'ok' (`grep -rn "SET observed_condition" src/` shows only `= 'quarantined'`, never `= 'ok'`). The first time this whole sequence executes against real media will be on the operator's irreplaceable data, because the gate that was declared GREEN never ran it.

### [HIGH] Issue #239's "a dirty drive must not condemn a good tape" ruling was applied to `volume verify` only — `SealedPending::confirm` still quarantines on ContentUnreadable
`src/volume/session.rs:948`

`MismatchKind::proves_medium_bad()` was built so a read error or short read (`ContentUnreadable`) cannot quarantine a volume. Only `volume verify` consults it. The write path's confirm still quarantines on ANY mismatch, so one transient read fault while reading back a freshly written tape produces exactly the outcome ADR-0012's amendment exists to prevent, on the path where it is most expensive.

**Failure scenario.** The production write finishes, `seal()` has already written the seal marker to the physical tape, and confirm's chain walk hits one transient SCSI error or short read on one slice — a dirty head, a marginal cable, the exact events #239 enumerates. `passed` is false, `observed_condition` becomes 'quarantined', `writes` rows go 'aborted'. `volume write` and `volume resume` both now refuse (`coverage::is_write_target` consults the condition, src/volume/write.rs:1207), and `volume init --force` on the same cartridge is refused by `decide_fresh_write_contact`'s AlreadySealed arm with "--force cannot override this" (src/volume/write.rs:2064) because the tape IS physically sealed. A good tape holding good bytes and a dirty drive have together cost the operator the cartridge, recoverable only by bulk-erasing it. Nothing on media can exercise this — neither mhvtl nor the lifecycle suite can inject a read fault — so it is unproven in either direction.

### [MEDIUM] `Config::load` hard-fails on a condition computed from transient /dev state, with no equivalent of the `db fsck --repair` escape #233 built in this same diff
`src/config.rs:620`

This diff added four new load-time refusals to `Config::load`. One of them, `validate_backends`, decides whether two `[[backends.lto]]` entries collide by calling `std::fs::canonicalize` on device paths — so whether tapectl starts at all now depends on what `/dev` looks like at that moment. There is no bypass: config load precedes everything except `completions` and `init`.

**Failure scenario.** The operator's config names the real LTO-6 by its stable by-id path and an mhvtl drive as `/dev/nst1`. A reboot renumbers and the by-id symlink now resolves to `/dev/nst1`. `Config::load` refuses with "both resolve to device_tape", and EVERY tapectl command fails — `catalog locate`, `restore unit`, `report copies`, `db backup`, `audit`, `db fsck --repair` — at precisely the moment the operator is trying to find out what happened. The recovery is hand-editing config.toml, which is not a thing the refusal message says and not a thing an heir following RECOVERY.md would know.

### [MEDIUM] The new #244 `staging clean` gate is database-wide and all-or-nothing, and its only escape (`--force`) is strictly less safe than the gate itself
`src/cli/staging.rs:220`

One unit below its resolved `min_copies` refuses the entire `staging clean`, including for fully-covered units, for `'failed'` stage sets that hold no copy requirement at all, and for orphaned session dirs and lockfiles. `--force` is the only way past, and it releases everything — including the under-copied unit the gate exists to protect.

**Failure scenario.** The operator writes copy 1 of ten units, then the cartridge for copy 2 is lost or the second drive fails. One unit sits at 1/2 copies indefinitely. `staging clean` now refuses forever, so the encrypted slices of all ten units — plus every failed stage set and orphaned session dir — stay on the staging filesystem. Staging fills; `stage create` starts failing on ENOSPC; the archive cannot make progress. The only documented escape is `--force`, which discards the staged bytes of the under-copied unit too — i.e. the operator's way out of the gate is the exact act the gate calls "discarding the only cheap route to the copy the operator's own policy requires".

### [MEDIUM] ADR-0012's `deny_unknown_fields` ruling was applied to the unit dotfile's `[policy]` table only; `[unit]`, `[excludes]` and the top level still swallow typos
`src/unit/dotfile.rs:60`

Issue #211 put `#[serde(deny_unknown_fields)]` on `PolicySection`. The three other structs that make up a `.tapectl-unit.toml` did not get it, so a misspelled section name or key outside `[policy]` is still silently ignored — the same failure #211 exists to close, one level up.

**Failure scenario.** An operator hand-edits a unit dotfile and writes `[excludes]\npattern = ["*.iso", "scratch/"]` (singular) instead of `patterns`. `read_dotfile` parses it cleanly, `exclude_patterns` comes back empty, and `stage create` archives the excluded material into the encrypted slices and onto the tape — permanently, on write-once media, with no indication anything was wrong. The `[policies]` variant of the same typo silently discards `warehouse_copies` and `slice_size`, so the unit quietly reverts to defaults for every copy decision made about it thereafter.

### [MEDIUM] Migration 017 rebuilds the `volumes` table but did not get the rebuild-verification standard issue #227 established for migration 012 four commits earlier in this same diff
`src/db/migrations/017_volume_observed_condition.sql:105`

Commit 20d06a2 closed #227 by pinning migration 012's table rebuild (no column changed, every index recreated, the foreign key preserved). Migration 017 — the only OTHER create/copy/drop/rename in the tree, added by this same diff — got two tests, neither of which pins the structural claims its own header makes.

**Failure scenario.** A dropped NOT NULL, a lost DEFAULT, or a `CREATE INDEX` that silently stopped being UNIQUE passes review because the migration header asserts byte-identity and nothing checks it. The failure surfaces later as duplicate volume UUIDs — which is what `check_tape_contact`'s File-0 identity match is built on (docs/design/layout-session.md: "require ID-thunk identity match (label + uuid) — mismatch = divergence = quarantine") — or as an orphaned `writes`/`cartridge_volumes` row that makes `db::open` fail on the NEXT `.foreign_key_check()` migration, which is the exact brick #233 was filed for.

### [MEDIUM] scripts/first-run.sh — the script the operator runs on production day — was not reconciled with the verify path's new two-outcome contract
`scripts/first-run.sh:776`

ADR-0012's 2026-09-17 amendment requires that an operator be able to tell "this tape is bad" from "this drive could not read it". first-run.sh collapses both into one message and dies, and never mentions that the first case has now silently taken the volume out of service.

**Failure scenario.** On the first production write the operator's verify fails because the drive needs cleaning. Under #239 that is `ContentUnreadable`, the volume is correctly left alone — but first-run.sh prints "do not trust this tape" and stops, telling the operator the opposite of what the code just decided. In the other direction, a genuine hash mismatch quarantines the volume as a side effect of the very line that then dies with a message that never says a catalog change happened, so the operator does not learn that the copy count for every unit on that tape just dropped.

### [LOW] src/volume/write.rs (+1175, the largest code change in the diff) was opened by no dimension — it still carries two claims this diff itself falsified
`src/store.rs:173`

Two load-bearing doc comments contradict the code they sit on. store.rs tells the next editor the ADR has NOT been amended to match the `ContentUnreadable` ruling — commit af847bc in this same diff amended it. write.rs's `VerifyReport::quarantine` says it reports what the verify did to `volumes.status` — issue #242, also in this diff, made it never touch `status`.

**Failure scenario.** A future reviewer reads store.rs:173, believes the arm contradicts a ratified ADR, and either files it as a violation (wasting a review cycle on a settled question) or 'restores' `ContentUnreadable` to `proves_medium_bad() == true` on the strength of the comment's own instruction — reinstating the dirty-drive-condemns-a-good-tape defect #239 exists to close. Separately, a `--json` consumer reading `VerifyReport.quarantine`'s documentation writes code that looks for a `status` change that never happens.

## Two findings land on the reviewer's own work

Worth recording, because they are the argument for running the pass at all:

1. **`cartridge unretire` has the identical defect I fixed in migration 017 hours earlier.** I found that
   017 restored `volumes.status` out of the `events` trail without constraining it to the legal set, fixed
   017's own subquery, and wrote the general rule into the commit message — then did not check whether
   anything else read that same trail. `cartridge_unretire` does, unfiltered, and writes it straight back
   into `status`. Fixing one reader of a trail and not grepping for the others is precisely the shape of
   the #244 miss recorded the same night.
2. **The non-vacuity guard I added in #226 is itself vacuous.** `csc_fingerprint`'s "every set must still
   be LIVE" check — added specifically so a fingerprint comparison could not pass on two identical records
   of nothing — is written so it can never fire. The guard against green-for-the-wrong-reason was green for
   the wrong reason.

