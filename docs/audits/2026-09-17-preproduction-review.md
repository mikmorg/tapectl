# Pre-production adversarial review — 2026-09-17

Run under autopilot Policy rule 7, which requires this review of the full diff
since `5d4cc43` before the CTO's real-drive rehearsal, with whatever it finds
worked under the `review-2026-09-13` label. It is the same shape as the
2026-09-13 post-redesign review (`2026-09-13-post-redesign-review.md`), which
found 59 confirmed items.

**Scope:** 220 commits, 200 files, 23,234 insertions, `5d4cc43..c309509`.

**Method:** ten dimension reviewers in parallel, each given the ADRs as the spec
rather than the code's own comments, and each followed immediately by an
adversarial verifier instructed to REFUTE every finding and to default to
"not a defect" when unsure. A finding appears below only because someone tried
honestly to kill it and could not. A completeness critic then asked what the ten
dimensions were shaped *not* to catch.

Reviewers were read-only: no edits, no cargo, no binary, no tape.

**Result: 29 confirmed, 33 rejected.** The verification pass killed more than
half of what the finders produced, which is the intended ratio — a finder that
is never wrong is not looking hard enough.

## Confirmed findings

| # | Severity | Finding | Location |
|---|---|---|---|
| 1 | medium | `cartridge mark-erased` un-condemns a `retired_permanent` cartridge, defeating a refusal documented as un-overridable | `src/cli/operations.rs:1433` |
| 2 | medium | check_tape_contact's identity-MATCHES branch never consults the tape's own seal pointer, so a sealed tape can be overwritten without --force | `src/volume/session.rs:407` |
| 3 | medium | `audit --action-plan` prescribes `stage create <unit>`, which this diff's snapshot-minting rule makes unrunnable for a copy shortfall | `src/cli/audit.rs:437` |
| 4 | medium | `catalog rebuild` auto-registers a cartridge with the volume's RESOLVED capacity, the one figure binding.rs forbids on a cartridge row | `src/volume/rebuild.rs:1539` |
| 5 | medium | The unit dotfile's `[policy]` table is the one config surface #171 never reached: unknown keys are silently ignored | `src/policy/mod.rs:238` |
| 6 | medium | `unit rename` silently deletes a dotfile's `[policy] slice_size`, converting an operator's choice into silence | `src/unit/dotfile.rs:72` |
| 7 | medium | A dotfile's `[policy] compression`/`checksum_mode` are never closed-set validated — the exact `dar -zbanana` failure ADR-0012 closed elsewhere | `src/policy/mod.rs:240` |
| 8 | low | `cartridge retire`'s help says consent is only required below policy; the code asks every time | `src/cli/cartridge.rs:83` |
| 9 | low | volume_write's `UPDATE volumes SET mam_capacity_bytes` still runs before three tape-side refusals that were going to fire anyway | `src/volume/write.rs:918` |
| 10 | low | RESTORE.sh, the heir-facing recovery script, prints a MiB-divided figure as "MB" | `src/volume/layout.rs:1050` |
| 11 | low | `capacity_override`'s own doc comment still declares the #168 two-byte-counts gap that #200 closed | `src/config.rs:171` |
| 12 | low | `catalog rebuild`'s barcode-collision refusal tells the heir to run `volume init` on the loaded tape — an act ADR-0003 refuses even with `--force`, leaving no way out of the branch | `src/volume/rebuild.rs:1515` |
| 13 | low | A rebuild that learns a medium serial reports `no_changes: true` while its own stderr says it learnt one | `src/volume/rebuild.rs:341` |
| 14 | low | `stage create`'s own refusal points at `snapshot create`, which now creates nothing when content is unchanged | `src/cli/stage.rs:297` |
| 15 | low | The escrow-mismatch remedy names `key import --escrow`, which is refused in exactly the state that produces the finding | `src/cli/audit.rs:835` |
| 16 | low | `volume abort`'s consent fact says the staged slices are released by `staging clean`, which will never release them after an abort | `src/volume/write.rs:1383` |
| 17 | low | Test comment (mirroring `bind_cartridge`'s doc) misattributes the writer of `volumes.capacity_bytes` to `volume_write` | `src/volume/binding.rs:2625` |
| 18 | low | `volume list`'s VERIFIED column re-implements `policy::evidence::compact_age` and disagrees with it on an unparseable timestamp | `src/cli/volume.rs:1303` |
| 19 | low | `compaction.tape_only_safety_multiplier` is unvalidated, and 0 zeroes the base copy requirement it is supposed to multiply | `src/policy/reclaimable.rs:127` |
| 20 | low | A hand-edited second `[[backends.lto]]` on the same device loads cleanly, and `backend add`'s own error sends the operator there | `src/config.rs:1064` |
| 21 | low | `[[archive_sets]]` closed-set values are not validated at load, so `config check` passes a config `archive-set sync` will refuse | `src/config.rs:712` |
| 22 | low | `capacity_override`'s doc comment still states the #168/#200 gap as current fact, contradicting the code and the test that pins it | `src/config.rs:171` |
| 23 | low | Operator guide promises a later `catalog rebuild` will bind a legacy tape's cartridge; no code path ever can | `docs/operator-guide.md:1182` |
| 24 | low | README's "Volume Layout" table still documents the v1 10-file layout that ADR-0007 superseded | `README.md:124` |
| 25 | low | Operator guide's `volume write` announcement sample prints MB where the code prints MiB | `docs/operator-guide.md:227` |
| 26 | low | Operator guide's `cartridge list` sample shows Loads = 0; the code prints `unknown` for both rows | `docs/operator-guide.md:916` |
| 27 | low | Operator guide says `audit` implements "all six compliance checks"; `cli::audit::CHECKS` has eleven | `docs/operator-guide.md:654` |
| 28 | low | README command reference omits `volume list`/`volume info`, added in this same diff | `README.md:97` |
| 29 | low | Migration 013's header comment claims dar takes `--acl` and that `preserve_acls` is passed through to dar; neither is true | `src/db/migrations/013_drop_manifest_entry_flags.sql:17` |

### Detail

#### 1. [medium] `cartridge mark-erased` un-condemns a `retired_permanent` cartridge, defeating a refusal documented as un-overridable

**Location:** `src/cli/operations.rs:1433`

**Evidence:** `cartridge_mark_erased` reads the cartridge's status only to decide whether consent is needed (`if status != "pending_erase"`, line 1413) and then writes `UPDATE cartridges SET status = 'available'` unconditionally (1432-1435). There is no `retired_permanent` early return — unlike `cartridge_retire`, which has one at 1002-1014. `binding::refuse_retired` (src/volume/binding.rs:478-489) refuses to bind any `retired_permanent` cartridge and its own doc (binding.rs:464-476) says: "no amount of consent makes a medium you have declared permanently unfit fit again ... So this takes no `force` parameter AT ALL, structurally like `session.rs`'s `check_tape_contact`/`AlreadySealed`: a caller cannot defeat it even by mistake. The escape is `cartridge unretire`." One `cartridge mark-erased BC --yes` moves the row to `available` and the guard has nothing left to refuse. The existing test `mark_erased_consent_facts_name_each_volume_by_label` (src/cli/operations.rs:5940) even feeds `"retired_permanent"` into the facts builder, so the combination is anticipated and unhandled rather than assumed impossible.

**ADR basis:** ADR-0012, *Rulings recorded as consequences*: "`cartridge unretire` (Tier 1) reverses `cartridge retire` and restores the volumes' prior statuses; `cartridge mark-erased` remains the statement that the bytes are gone. ADR-0011's 'mark-erased is the only way back' is corrected there." The two statements are separate by ruling: erasing the bytes is not a judgement that the medium is fit, so it must not silently restore a condemned cartridge to a bindable status. ADR-0008's Tier-3 reasoning (risk vs incoherence) is the same one binding.rs:464-476 cites for taking no `force` parameter.

**Failure scenario:** A cartridge BC002 is retired permanently for read errors (`cartridge retire BC002 --reason "3 hard read errors"`). Later the operator bulk-erases it along with a pile of good tapes and runs `tapectl cartridge mark-erased BC002 --yes` to clear the shelf. The row goes to `available` with no refusal and no mention that this cartridge was condemned; `volume init` now binds it happily, because `refuse_retired` only ever tested the status that was just overwritten. Production data is then written to a medium the operator declared unfit, and the `cartridge unretire` audit trail that would have recorded the reversal was never written.

**Suggested fix:** Refuse `retired_permanent` in `cartridge_mark_erased` before the consent branch, with no `force` parameter in scope, naming `cartridge unretire` as the way back exactly as `refuse_retired` does — or, if recording "the bytes are gone" on a condemned cartridge must stay possible, move the volumes to `erased` while leaving `cartridges.status` at `retired_permanent` rather than writing `available`.

**Verifier (adversarial):** I could not kill this one. The code is as claimed: `cartridge_mark_erased` reads status only to decide consent (operations.rs:1413 `if status != "pending_erase"`) and then writes `UPDATE cartridges SET status = 'available' WHERE id = ?1` unconditionally (operations.rs:1432-1435). There is no `retired_permanent` early return, in contrast to `cartridge_retire`'s at operations.rs:1002-1014, and no pre-dispatch guard either — `src/cli/cartridge.rs:561-567` calls straight through.

The decisive ADR text is ADR-0011's own dated correction, not ADR-0012: "*Correction 2026-09-14: the diagram gained the `pending_erase → in_use` edge ... and `unretire` **replaced** `mark-erased` as the way back from `retired_permanent`.*" The lifecycle diagram above it shows only `cartridge unretire` returning from `retired_permanent`. The code leaves mark-erased as a second, consent-waivable way back, which is exactly what "replaced" forbids.

It also defeats a guard documented as structurally undefeatable. `binding::refuse_retired` (src/volume/binding.rs:478-489) tests `row.status == "retired_permanent"`, and its doc (binding.rs:464-476) says "no amount of consent makes a medium you have declared permanently unfit fit again ... So this takes no `force` parameter AT ALL ... a caller cannot defeat it even by mistake." One `cartridge mark-erased BC --yes` rewrites the only field that guard reads. Erasing the bytes is not a judgement that the plastic is fit, so the two statements should not be coupled.

One correction to the finding: the test at operations.rs:5940 does NOT pin the behaviour — `mark_erased_consent_facts_name_each_volume_by_label` only feeds the string "retired_permanent" into the pure facts builder and asserts wording. Drop the "anticipated and unhandled" framing; the defect stands without it. Severity medium is right: nothing is lost at the moment of the act (the bytes are declared gone anyway), but the condemnation is silently withdrawn with no `unretire` audit row, and `volume init` will then bind and write a medium the operator declared unfit.

---

#### 2. [medium] check_tape_contact's identity-MATCHES branch never consults the tape's own seal pointer, so a sealed tape can be overwritten without --force

**Location:** `src/volume/session.rs:407`

**Evidence:** `check_tape_contact` has two seal probes. In the identity-MISMATCH branch (session.rs:392-400) it reads the tape's OWN self-reported pointer:

    if let Ok(pointers) = format::parse_id_thunk_layout_pointers(&text) {
        if pointers.seal_marker >= 0 && seal_marker_parses_at(store, pointers.seal_marker as u32) {
            return ContactOutcome::AlreadySealed { seal_position: pointers.seal_marker as u32 };
        }
    }

and the comment at session.rs:380-391 gives the reason: the caller's `seal_position` is "a position in the CALLER's own layout and has no relationship to a different tape's real seal marker", and without this probe "a foreign-but-sealed cartridge would present as a plain `IdentityMismatch`, which the fresh-write path's `--force` override is allowed to defeat - silently permitting exactly the sealed-volume overwrite ADR-0003 forbids."

The identity-MATCHES path then falls through to session.rs:407-413, which probes ONLY the caller's position:

    if let Some(seal_pos) = seal_position {
        if seal_marker_parses_at(store, seal_pos) { return ContactOutcome::AlreadySealed { ... } }
    }
    if file_zero_present { ContactOutcome::Matches } else { ContactOutcome::Blank }

The same reasoning the mismatch branch documents applies here: `seal_position` comes from the layout THIS write just built (write.rs:1068-1071), and a tape carrying the same label+uuid but written from a different-sized layout has its real seal marker somewhere else. `ContactOutcome::Matches` makes `decide_fresh_write_contact` (write.rs:1876) return `Ok(())` with no force needed, and the session then overwrites the tape from BOT.

The precondition chain is two-part and both parts are needed: (a) the catalog row for the SAME uuid reads `initialized` with no `completed` writes row - `has_completed_write` (coverage.rs:158) closes the `catalog rebuild --from-volume` route, but a `tapectl.db` restored from a `db backup`/`db import` taken between `volume init` and `volume write` reaches exactly this state; and (b) the new layout's file count differs from the sealed tape's, so the caller's `seal_position` lands on a written-but-not-a-seal-marker position (probe false) or past end of data (read fails). With an identical file count the caller's position coincides with the real marker and the existing probe catches it - which is why the gap has not shown up.

Happy path is unaffected by the fix: `volume init` stamps File 0 with `PROVISIONAL_TOTAL_FILES = 8` (write.rs:367), so a freshly initialised tape's `[layout].seal_marker = 7` and position 7 is unwritten - `seal_marker_parses_at` returns false. `volume_init` itself is already safe because it passes a fresh `candidate_uuid` (write.rs:325), which always takes the mismatch branch.

**ADR basis:** ADR-0003 (docs/adr/0003-sealed-volumes-immutable-no-append.md): "a sealed volume is never written again, and tape append ... is explicitly rejected"; the refusal text at write.rs:1878-1884 states the operative rule as "ADR-0003: sealed volumes are immutable, there is no append, and --force cannot override this." The guard implementing it is `check_tape_contact`'s AlreadySealed detection, and in the identity-matching branch that guard is looking in the wrong place.

**Failure scenario:** Volume V (label L6-0004, uuid U) is written and sealed with 12 files; its seal marker is at position 11. Later the machine is rebuilt and `tapectl.db` is restored from a `db backup` taken right after `volume init` - so V's row reads `initialized` with no `writes` rows at all. The operator stages a smaller batch (one unit, one tenant, 3 slices -> 10 files, seal marker at 9) and runs `tapectl volume write L6-0004` with the sealed cartridge in the drive, no --force. `is_write_target` passes (initialized), `has_completed_write` passes (no writes rows), `corroborate_volume` passes (same cartridge, serial agrees), File 0 parses with label L6-0004 / uuid U so `matches` is true; `seal_marker_parses_at(store, 9)` reads position 9, which on the sealed tape is a data slice - ciphertext, not a seal marker - so it returns false. `ContactOutcome::Matches` -> `decide_fresh_write_contact` returns Ok -> `store.reposition_for_resume(0)` -> the session writes from BOT over a sealed volume. The tape's own File 0 said `seal_marker = 11` the whole time and was never read.

**Suggested fix:** Hoist the tape's self-reported probe out of the mismatch branch so it runs for a present File 0 regardless of whether the identity matched: parse `format::parse_id_thunk_layout_pointers(&text)` once after `file_zero_present`, and if `pointers.seal_marker >= 0 && seal_marker_parses_at(store, pointers.seal_marker as u32)` return `AlreadySealed` before the identity comparison. Keep the caller's `seal_position` probe at session.rs:407 as the belt-and-braces case for a tape whose File 0 is unreadable. Pin it with a MemStore test: same label+uuid, seal marker at the tape's own position 11, caller's layout seal_position 9, expect `AlreadySealed` - and keep `check_tape_contact_matches_when_identity_agrees_and_seal_position_is_unwritten` (session.rs:2384) green, which the `PROVISIONAL_TOTAL_FILES = 8` pointer makes safe.

**Verifier (adversarial):** REFUTATION ATTEMPTED AND FAILED — every kill I tried came apart.

The cited code says exactly what the finding claims. src/volume/session.rs:392-400 (mismatch branch) probes `format::parse_id_thunk_layout_pointers(&text)`'s `pointers.seal_marker` — the TAPE's own pointer. session.rs:407-413 (the fall-through that both the matching and the File-0-absent cases reach) probes only the caller's argument:

    if let Some(seal_pos) = seal_position {
        if seal_marker_parses_at(store, seal_pos) { return ContactOutcome::AlreadySealed { seal_position: seal_pos }; }
    }
    if file_zero_present { ContactOutcome::Matches } else { ContactOutcome::Blank }

Kill attempt 1 — a later guard catches it. Failed. `decide_fresh_write_contact` (write.rs:1875-1876) is literally `ContactOutcome::Blank | ContactOutcome::Matches => Ok(())`. The only thing after it is `store.reposition_for_resume(0)` (write.rs:1074) then `built.into_validated` (session.rs:95-118), which I read in full: it re-runs `validate(keys)` and one `store.capacity()` shortfall check. No seal probe, no tape read of File 0. `parse_id_thunk_layout_pointers` has exactly three non-test callers (raw.rs:92, rebuild.rs:282, session.rs:392) — none on the write path's matching branch.

Kill attempt 2 — the caller's position always coincides with the real marker, so the existing probe covers it. Failed, but it narrows the gap and I record it as the strongest available defence: if the stale catalog postdates `stage create` AND the staging files survive, the rebuilt layout has the same file count, `seal_position` lands on the real marker, and the probe fires. The gap needs a differing file count — which a re-stage produces. Both directions leak: a smaller layout probes a position holding ciphertext (`parse_seal_marker` fails), a larger one probes past EOD (`read_file` fails) — `seal_marker_parses_at` (session.rs:424-432) returns false for both, by design ("a read failure ... is the expected, safe 'not sealed' case").

Kill attempt 3 — `seal_position` might be `None` so the argument is moot. Failed, and confirms the finding: build.rs:518 always emits a `ZoneKind::SealMarker` entry and build.rs:1210 asserts `entries.last()` is one, so write.rs:1068-1071 always yields `Some`. And layout.rs:147-148 shows File 0's template carries `seal_marker = {seal_marker}` from the real layout — so a sealed tape's own File 0 does state its true seal position in plaintext, and the matching branch never reads it. The finding's closing line is literally true.

Kill attempt 4 — unreachable given how callers behave. Failed. `coverage::has_completed_write` (coverage.rs:158-164) is `EXISTS(SELECT 1 FROM writes WHERE volume_id = ?1 AND status = 'completed')`, which closes the `catalog rebuild --from-volume` route (rebuild always inserts `completed`) and the unresolved-session check at write.rs:848 closes the interrupted route — but neither closes a `tapectl.db` restored from a `db backup`/`db import` predating the write, which leaves the row `initialized` with zero `writes` rows. `corroborate_volume` then PASSES (same cartridge, serial agrees, binding.rs:1416). CONTEXT.md's own "Contact: the only moment the tape is authoritative, and therefore the reconciliation event" — quoted back in binding.rs:1335 — is the principle that says the tape's pointer should be consulted in both branches, not just one.

SCOPE CAVEAT (recorded, not a flip): `check_tape_contact` is UNCHANGED in 5d4cc43..HEAD — `git diff 5d4cc43..HEAD -- src/volume/session.rs` touches only test fixtures (`cartridge_identity_source: None` additions). This is pre-existing code, not new in the diff. I leave `real: true` because "pre-existing" is not among the listed refutation reasons and the branch is reached through the write path this diff rebuilt.

SEVERITY: medium is honest and I decline to upgrade. The rubric's "can lose data" fits the outcome (a sealed archival tape overwritten from BOT), but it takes a stale catalog AND a fresh staging set to get there.

---

#### 3. [medium] `audit --action-plan` prescribes `stage create <unit>`, which this diff's snapshot-minting rule makes unrunnable for a copy shortfall

**Location:** `src/cli/audit.rs:437`

**Evidence:** `check_copy_count` emits action `"tapectl stage create {unit} && tapectl volume write <LABEL>"` (audit.rs:437); `check_escrow_coverage` (audit.rs:612) and `check_encryption` (audit.rs:730) emit the same shape. The action lines themselves are unchanged in this range, but `src/unit/content_match.rs` (new, +329) and `staging::snapshot_create_detailed` (src/staging/mod.rs:85-140) are new here and close the loop: a unit with a shortfall has its latest snapshot at status `current` (set at seal, src/volume/session.rs:977). `stage create <unit>` with no `--version` selects only `status = 'created'` (src/cli/stage.rs:288-299) and refuses. Its refusal says "run `tapectl snapshot create` first"; `snapshot create` on unchanged content now returns `minted: false` (src/staging/mod.rs:126-133) and prints "unit \"X\" is unchanged since v1; no snapshot created" (src/cli/snapshot.rs:187-190), exit 0. The operator cycles between two commands, neither of which does anything.

**ADR basis:** ADR-0012, "A copy is identical content": "a version is minted only when content changed: `snapshot create` on a unit whose walk matches its latest current snapshot ... reports the existing version and creates nothing." That ruling is what invalidates the pre-existing action text; ADR-0004 makes audit advisory, so its action plan is the operator's only instruction.

**Failure scenario:** `min_copies = 2`; unit `photos` v1 written to one sealed volume; `staging clean` released the set. `tapectl audit --action-plan` prints "fix: tapectl stage create photos && tapectl volume write <LABEL>". `stage create photos` → "no unstaged snapshot for unit \"photos\" — run `tapectl snapshot create` first". `snapshot create photos` → "unit \"photos\" is unchanged since v1; no snapshot created". The second copy is never made and the violation persists.

**Suggested fix:** Make the action version-explicit and branch on whether the set still holds live slices, exactly as `operations::refuse_last_eligible_copy` (src/cli/operations.rs:641-679) already does: `tapectl stage create <unit> --version <current>` when the set is `cleaned`/`failed`, and a bare `tapectl volume write <LABEL>` when it is still `staged`. Reuse `staging::stage_set_has_live_slices` so the two sites cannot drift.

**Verifier (adversarial):** SURVIVES. I could not kill it.

Every link verified:
- audit.rs:436-439 emits action "tapectl stage create {unit} && tapectl volume write <LABEL>" for a copy shortfall.
- session.rs:977 flips the snapshot to 'current' at seal ("UPDATE snapshots SET status='current' WHERE id=?1 AND status IN ('created','staged')").
- stage.rs:288-299: the no---version arm selects strictly "WHERE unit_id=?1 AND status='created'" and otherwise errors "no unstaged snapshot for unit \"{name}\" — run `tapectl snapshot create` first".
- staging/mod.rs:96-140 (new in 878061d, in range): on an unchanged walk with latest_status in ("current","created","staged") it returns minted:false, and cli/snapshot.rs:186-191 prints "unit \"{name}\" is unchanged since v{n}; no snapshot created" and exits 0.
The two-command cycle is real and neither command advances it.

Refutations attempted and failed: (a) "pre-existing text, not this diff" — the ACTION text is pre-existing, but the rule that invalidates it (878061d "staging: mint a snapshot version only when content changed") is inside 5d4cc43..HEAD, so the diff is what broke it; (b) "a working command exists" — yes, `stage create <unit> --version <n>` (stage.rs:301-330, which selects any status and only gates on live slices), but neither the audit action nor either refusal names it; (c) audit is advisory (ADR-0004), which is precisely why the action line is the operator's only instruction.

Medium is correct: the violation is real and correctly reported, only the remedy is unrunnable — wrong but recoverable.

---

#### 4. [medium] `catalog rebuild` auto-registers a cartridge with the volume's RESOLVED capacity, the one figure binding.rs forbids on a cartridge row

**Location:** `src/volume/rebuild.rs:1539`

**Evidence:** Four sites create a `cartridges` row. Three write the generation table's native figure or an explicit operator declaration:

- `src/volume/binding.rs:695` (auto-register at `volume init`) writes `generation.native_capacity_bytes() as i64`, under a comment at binding.rs:670-684 that states the rule verbatim: "`nominal_capacity` is the GENERATION TABLE's native figure for the detected medium, never the caller's resolved capacity (ADR-0010 decision 3, issue #183). A drive `capacity_override` sits ABOVE the cartridge row in that decision's precedence ladder precisely so it can lie about ONE volume on ONE drive (mhvtl's 2400 MB fiction); writing that resolved figure onto a brand-new row would make the next `volume init` — on any drive, override or none — read the drive's lie back as an operator's declaration at `cartridge register --capacity`. The row describes the plastic".
- `src/cli/cartridge.rs:359-361` — `parse_capacity_to_bytes(c)` when `--capacity` is given, else `parsed.native_capacity_bytes()`.
- `src/cli/operations.rs:2945-2947` — same shape for `import --capacity`.

The fourth, added by this diff (d9f2d78, issue #165), writes the resolved figure instead. `src/volume/rebuild.rs:1531-1541`:

    tx.execute(
        "INSERT INTO cartridges
            (barcode, media_type, manufacturer, serial_number, tape_length_meters,
             nominal_capacity, status, total_load_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'in_use', NULL)",
        params![serial, meta.media_type, manufacturer, serial, length,
                meta.nominal_capacity_bytes],
    )?;

and identically at `src/volume/rebuild.rs:1637` in `resolve_operator_identity`'s auto-register arm.

`meta.nominal_capacity_bytes` is File 0's `[volume].nominal_capacity_bytes` (`src/volume/format.rs:254`, written by `src/volume/layout.rs:143`). Tracing what the write path puts there: `src/volume/write.rs:293-302` computes

    let capacity_override = match &backend.capacity_override { Some(v) => Some(media::parse_capacity_to_bytes(v)? as u64), None => None };
    let (nominal_capacity, capacity_source) = media::resolve_capacity(capacity_override, lookup.row.as_ref().map(|r| r.nominal_capacity as u64), generation);

and hands exactly that to `layout::generate_id_thunk_v2` at write.rs:400. So File 0 carries the output of `resolve_capacity` — override-first — not the generation table's figure.

Note rebuild.rs:803, which writes the same `meta.nominal_capacity_bytes` into `volumes.capacity_bytes`, is CORRECT and not part of this finding: `volumes.capacity_bytes` is exactly where ADR-0010 decision 3 says the resolved figure belongs. Only the two `cartridges` inserts are wrong.

**ADR basis:** ADR-0010 decision 3 ("generation is a cartridge property"; capacity is decided once at `volume init` and the precedence is drive `capacity_override` > cartridge row > generation table). The cartridge row is rung 2; the override is rung 1. Writing rung-1 output into a rung-2 column collapses the ladder, which is the promotion `src/volume/binding.rs:670-684` exists to refuse. Also CLAUDE.md's ADR-0010 summary: "capacity follows it, overridden by the cartridge row then by the drive's `capacity_override` (virtual drives only)".

**Failure scenario:** A tape is written on a drive with `capacity_override` set — the documented mhvtl case, `capacity_override = "2748779069440"` in `scripts/mhvtl-verify-gate.sh:170`, or any virtual drive. File 0 records that overridden figure. Disaster; the operator runs `tapectl catalog rebuild --from-volume` on that cartridge. `resolve_mam_identity` finds no matching serial and auto-registers the cartridge with `nominal_capacity` = the override. Later, the same physical cartridge is re-initialised on the real LTO-6 drive (no override): `media::resolve_capacity(None, Some(<override>), Lto6)` returns `CapacitySource::CartridgeRow` and `volumes.capacity_bytes` is stored at the override, never the 2.5 TB the medium actually holds. If the override was smaller than native, `volume write`'s pre-flight capacity gate (write.rs:879-886) refuses batches the tape would easily hold and the cartridge is silently retired-by-arithmetic. If it was larger, the gate passes a plan the tape cannot hold and the session ends at a real EOT — which Layout v2 turns into a clean abort to an unsealed tape (end-of-tape salvage is gone), so the whole write is thrown away.

**Suggested fix:** Derive the cartridge row's capacity from the medium's own generation, exactly as binding.rs:695 does, not from File 0's resolved figure: parse `meta.media_type` through `media::Generation::parse` and use `native_capacity_bytes()`, falling back to NULL (not the resolved value) when the media type does not parse. Better still, extract binding.rs's auto-register INSERT into one `pub(crate)` helper in `src/volume/binding.rs` and call it from both rebuild arms, so the rule and its comment live at one site.

**Verifier (adversarial):** Refutation attempted and failed; the code says what the finding claims. Verified each link in the chain: (a) `src/volume/write.rs:293-302` sets `capacity_override` from `backend.capacity_override` and takes `resolve_capacity(override, row, generation)` — `src/media.rs:282-291` returns the override FIRST (`CapacitySource::DriveOverride`); (b) that resolved `nominal_capacity` is handed to `layout::generate_id_thunk_v2` at write.rs:400 and printed into File 0 as `nominal_capacity_bytes = {nominal_capacity}` (`src/volume/layout.rs:143`); (c) `format::parse_id_thunk_volume_meta` reads it back into `meta.nominal_capacity_bytes`; (d) both rebuild auto-register arms bind that value to the `nominal_capacity` column — `src/volume/rebuild.rs:1531-1541` (MAM arm) and `:1627-1641` (operator arm) — while `src/volume/binding.rs:672-696` writes `generation.native_capacity_bytes() as i64` under a comment stating the prohibition verbatim, and `src/cli/cartridge.rs`/`src/cli/operations.rs` write either the operator's `--capacity` or the table figure. ADR-0010 decision 3 (docs/adr/0010, read in full) defines rung 2 as "the bound cartridge row's `nominal_capacity` (an operator said so at `cartridge register --capacity`)" — so a drive-override figure landing in that column is a rung-1 value promoted to rung 2, and `media::resolve_capacity` will hand it back as `CapacitySource::CartridgeRow` at the next `volume init` on that cartridge. No guard elsewhere blocks it: nothing in rebuild.rs warns about or normalises the capacity, and `resolve_capacity` cannot tell a promoted override from a declaration. Two narrowings the finding does not state: (i) the defect is override-only — with no override, `meta.nominal_capacity_bytes` is either the operator's own `--capacity` declaration or the generation-table figure, both legitimate row values; (ii) the practically reachable arm is `resolve_operator_identity` (rebuild.rs:1637), since the documented override case is mhvtl, which exposes no medium serial, so init took `--cartridge` and File 0 carries a barcode. `config check` only WARNS about `capacity_override` on a real drive (`src/cli/config.rs:407-409`), it does not refuse, so the override-on-real-drive path is open too. Severity sits at the medium/low boundary — no data is lost (an over-credit ends in Layout-v2's clean abort to an unsealed tape, an under-credit in spurious pre-flight refusals) — but 'recoverable but wrong' is the right rubric line, so medium stands. The fix is available in-place: `meta.media_type` parses via `media::Generation::parse` (src/media.rs:56) to the native figure.

---

#### 5. [medium] The unit dotfile's `[policy]` table is the one config surface #171 never reached: unknown keys are silently ignored

**Location:** `src/policy/mod.rs:238`

**Evidence:** `policy::resolve`'s layer-1 (highest-priority) dotfile read parses the file as a bare `toml::Table` and then pulls four keys out by hand:
```
if let Some(pol) = toml.get("policy").and_then(|v| v.as_table()) {
    if let Some(v) = pol.get("checksum_mode")...
    if let Some(v) = pol.get("compression")...
    if let Some(v) = pol.get("slice_size")...
    if let Some(v) = pol.get("warehouse_copies")...
```
Anything else in `[policy]` is simply not looked at. The structural reader is no stricter: `DotfileToml`/`UnitSection`/`PolicySection`/`ExcludesSection` (src/unit/dotfile.rs:69-85) carry `#[derive(Serialize, Deserialize)]` with NO `#[serde(deny_unknown_fields)]`, unlike every struct in src/config.rs (lines 65, 96, 119, 143, 250, 267, 355, 389, 399, 435, 460). So a misspelled dotfile policy key is accepted by `read_dotfile` and ignored by `resolve`, with no error and no advisory — `policy::unknown_keys::scan` is scoped to config.toml's `[defaults]` only (src/policy/unknown_keys.rs:52-56).

**ADR basis:** ADR-0012, "Unknown config keys are errors everywhere; closed-set values are validated at load": "A misspelled key was a hard exit in `[[backends.lto]]` (ADR-0010), an advisory in `[defaults]` and silent everywhere else... One rule: `deny_unknown_fields` on every section... Lenient warnings were considered and rejected — a key that silently reads as its default is the failure that never gets noticed." CLAUDE.md names `.tapectl-unit.toml` as "Per-unit config", and it is the HIGHEST-priority layer of the documented policy chain (v4.0 §2.12, src/policy/mod.rs:42-45). Scope caveat for the reader: the ADR's body and the #171 commit message enumerate config.toml sections specifically; the heading says "everywhere".

**Failure scenario:** An operator hand-edits `photos/.tapectl-unit.toml` to pin a smaller slice for a unit full of huge files and types `[policy]\nslize_size = "500M"`. `unit status`, `config check`, `audit` and `stage create` all succeed and say nothing. The unit is archived at the 10G default forever. The same happens for `min_copies`, `verify_interval_days`, `preserve_xattrs` or any other plausible-looking key an operator copies out of the design doc's §2.2 example — exactly the #129 `defaults.min_copies` failure, one file over.

**Suggested fix:** Put `#[serde(deny_unknown_fields)]` on `DotfileToml` and all three of its sections, and have `policy::resolve` deserialize `PolicySection` (extended with `slice_size`) instead of hand-picking keys off a `toml::Table` — one type, one definition of what a dotfile may contain, refused by name at read time the way `Config::load` refuses config.toml.

**Verifier (adversarial):** Refutation attempted and failed. src/policy/mod.rs:228-265 is exactly as quoted: the dotfile is parsed as `contents.parse::<toml::Table>()` and only four keys are pulled by hand (`checksum_mode`, `compression`, `slice_size`, `warehouse_copies`); nothing else in `[policy]` is looked at. src/unit/dotfile.rs:50-85 confirms `DotfileToml`/`UnitSection`/`PolicySection`/`ExcludesSection` carry only `#[derive(Serialize, Deserialize)]` — `grep -n deny_unknown_fields src/unit/dotfile.rs` returns nothing, while src/config.rs has it at 65, 96, 119, 143, 250, 267, 355, 389, 399, 435, 460. I looked for a guard elsewhere: `policy::unknown_keys::scan` is explicitly '[Scoped] to `[defaults]` on purpose' (src/policy/unknown_keys.rs:50-56); `policy::shadowing::scan` only reports `df.checksum_mode.is_some()`/`df.compression.is_some()` (shadowing.rs:56-58); `policy::lenient_config` borrows validity from `Config`'s own Deserialize and `Config::semantic_problems` — config.toml only. So nothing anywhere reports a misspelled dotfile key. Scope caveat, stated honestly: ADR-0012:151-157's body enumerates config.toml sections (`[[backends.lto]]`, `[defaults]`) and #171 landed there; the dotfile is a different artifact, and the design doc's §2.2 example (tapectl-design-v4_0.md:163-178) shows only the two keys the code declares, so the finder's 'copies min_copies out of §2.2' colour is overstated. But `[policy] slice_size` IS live spec (§2.12:450-455) and IS read, so the surface is real and the failure mode ('slize_size' accepted and ignored forever) is the #129 shape one file over. One thing the finder missed and whoever fixes it must know: a mechanical `deny_unknown_fields` on `PolicySection` would reject the LEGAL `slice_size` key, because that key is deliberately not in the struct (staging/mod.rs:903-907 says so) — the fix has to model slice_size first.

---

#### 6. [medium] `unit rename` silently deletes a dotfile's `[policy] slice_size`, converting an operator's choice into silence

**Location:** `src/unit/dotfile.rs:72`

**Evidence:** `PolicySection` declares only `checksum_mode`, `compression`, `warehouse_copies` (src/unit/dotfile.rs:72-81). `slice_size` is absent — yet `policy::resolve` reads `[policy] slice_size` from the raw table (src/policy/mod.rs:246) and its own doc calls it "the only layer parsed at USE time" (src/policy/mod.rs:47-49). `read_dotfile` (dotfile.rs:125-145) therefore drops it, and `write_dotfile` (dotfile.rs:106-118) re-serializes only the three declared fields. `unit rename` is a read→mutate→write round trip over exactly that pair:
```
match dotfile::read_dotfile(&dotfile_path) {
    Ok(mut df) => { df.name = new_name.to_string();
                    if let Err(e) = dotfile::write_dotfile(&dotfile_path, &df) {
```
(src/unit/mod.rs:192-196). The rename is reported as successful; only a write failure is warned about.

**ADR basis:** v4.0 §2.12 "Slice Size — Resolution order: 1. Unit dotfile `[policy] slice_size`" is live spec: docs/design-errata.md has no entry superseding §2.12 (its §2.2 entry, line 51, is the separate `Option`-ness recast). That recast's own ruling — dotfile policy fields are `Option`, omitted unless set, and "absent means defer upward" because "a filled-in default is indistinguishable from a deliberate operator choice" — is the rule this breaks in the other direction: `slice_size` is the field the recast never reached, so the rewrite turns "the operator chose 500M" into "the operator was silent."

**Failure scenario:** A unit has `[policy] slice_size = "500M"` (honoured by `policy::resolve`, and by `staging::resolve_slice_size_string`, which re-reads the same raw key). The operator runs `tapectl unit rename photos family-photos`. The command prints success. The dotfile comes back without the `slice_size` line, and every subsequent `stage create` cuts 10G slices instead of 500M — a 20x change in blast radius per damage event and in retry quantum, with nothing anywhere saying the policy changed.

**Suggested fix:** Add `slice_size: Option<String>` to `PolicySection` and `UnitDotfile` and carry it through `read_dotfile`/`write_dotfile`, so the round trip is lossless; have `policy::resolve` read it from the same struct rather than from a raw `toml::Table`. Add a round-trip test that a dotfile carrying every `[policy]` key survives `read_dotfile` → `write_dotfile` byte-identically.

**Verifier (adversarial):** Refutation attempted and failed on every leg. (a) `PolicySection` really declares only three fields (dotfile.rs:71-81) and `write_dotfile` re-serializes only those three (dotfile.rs:90-100); `read_dotfile` builds `UnitDotfile` from `wrapper.policy` alone (dotfile.rs:125-145), so `slice_size` is dropped on read and absent on write. (b) The key is genuinely honoured elsewhere: policy/mod.rs:246 parses it, and staging/mod.rs:897-909 re-reads the same raw key with the comment 'This key isn't part of the structured `UnitDotfile`/`PolicySection` model (only checksum_mode/compression are), so it has to be read the same ad-hoc way'. (c) The round trip is real and reachable: src/unit/mod.rs:189-196 — `match dotfile::read_dotfile(&dotfile_path) { Ok(mut df) => { df.name = new_name.to_string(); if let Err(e) = dotfile::write_dotfile(...)' — and only a WRITE failure warns. (d) I checked whether some other path would restore it: `grep write_dotfile src/` shows the only production callers are `unit::init` (mod.rs:108, writes None) and this rename; collection/sync.rs:535 and collection/fingerprint.rs:699/829 are inside `mod tests` (sync.rs:350, fingerprint.rs:231). (e) Spec basis holds: docs/design-errata.md has no entry superseding §2.12, and its §2.2 recast (line 51) rules that 'absent means defer upward' precisely because 'a filled-in default is indistinguishable from a deliberate operator choice' — which is what the rewrite manufactures. No test pins the loss (grep slice_size src/unit/ is empty). Medium is right: silent policy mutation, recoverable by re-editing, no data loss.

---

#### 7. [medium] A dotfile's `[policy] compression`/`checksum_mode` are never closed-set validated — the exact `dar -zbanana` failure ADR-0012 closed elsewhere

**Location:** `src/policy/mod.rs:240`

**Evidence:** `policy::resolve` assigns the dotfile's values straight through with no validation:
```
if let Some(v) = pol.get("checksum_mode").and_then(|v| v.as_str()) { policy.checksum_mode = v.to_string(); }
if let Some(v) = pol.get("compression").and_then(|v| v.as_str()) { policy.compression = v.to_string(); }
```
(only `slice_size`, two lines below, is parse-checked). `stage_create` then takes `resolved.compression` verbatim into the `stage_sets` row and the dar invocation (src/staging/mod.rs:371-372, 405-407, and `dar::create` builds `-z` from it). Contrast the two boundaries that DO validate the identically-named fields: `Config::closed_set_problems` for `[defaults]` (src/config.rs:713-717) and `archive-set create/edit/sync` (src/cli/archive_set.rs:649-658). The dotfile is the layer that outranks both.

**ADR basis:** ADR-0012: "`compression = \"banana\"` surfaced as a raw dar failure at stage time... One rule: ... every closed set (generation, compression, checksum mode, statuses on `--status` filters) rejected by name at the boundary." `validate_compression`/`validate_checksum_mode` already exist as the shared validators (src/config.rs:1002, 1013) precisely so no boundary states the rule twice; this boundary states it zero times.

**Failure scenario:** A dotfile carries `[policy] compression = "zstd "` (trailing space) or `"banana"`. `config check`, `audit` and `unit status` are all clean. `stage create` on that unit runs sha256 over the whole source, writes the `stage_sets` row, then dies on `dar -zbanana` — hours in, for a multi-TB unit. With `checksum_mode = "sha256sum"` instead, the failure is the opaque SQLite CHECK-constraint rejection ADR-0012 names as the other motivating case (units.checksum_mode CHECK, src/db/migrations/001_initial.sql:68). Note the dotfile is hand-edited in both cases: `unit init` writes `checksum_mode: None, compression: None` (src/unit/mod.rs:104-105) and exposes no flag for either.

**Suggested fix:** Call `config::validate_compression` and `config::validate_checksum_mode` on the dotfile values inside `policy::resolve`'s layer-1 block, returning `PolicyUnresolvable { layer: Dotfile, .. }` the way the `slice_size` parse already does — so `audit` reports it as `policy_unresolvable` instead of dar reporting it at stage time.

**Verifier (adversarial):** Could not refute; both branches verified end to end. Compression: policy/mod.rs:240-245 assigns `policy.compression = v.to_string()` with no validation (only `slice_size` two lines down is parse-checked); staging/mod.rs:372 `let compression = resolved.compression.clone();`, inserted at 405-408 and passed to dar at 461 (`compression: &compression`), which becomes `cmd.arg(format!("-z{}", params.compression))` at dar/create.rs:38-44 — and the source sha256 pass (staging/mod.rs:111) runs BEFORE that, so the finder's 'hours in' is accurate. Checksum mode: the dotfile value reaches `queries::insert_unit` via `df.checksum_mode.as_deref().unwrap_or(DEFAULT_CHECKSUM_MODE)` at unit/discovery.rs:143 and collection/sync.rs:299, and queries.rs:359-371 INSERTs it straight into the column whose CHECK is `CHECK(checksum_mode IN ('mtime_size','sha256','sha256_on_archive'))` (001_initial.sql:64-65) — the opaque CHECK failure ADR-0012:153 names as the second motivating case. The shared validators exist and are used by the two lower-priority layers (config.rs:1002/1013, called from `closed_set_problems` at 713-717 and from archive_set.rs:648-667) but by no dotfile path. ADR-0012:154-156 is quoted correctly ('every closed set ... rejected by name at the boundary'). Same dotfile-scope caveat as finding 1, but here the harm is the exact harm the ADR names, so it survives at medium.

---

#### 8. [low] `cartridge retire`'s help says consent is only required below policy; the code asks every time

**Location:** `src/cli/cartridge.rs:83`

**Evidence:** The variant doc at src/cli/cartridge.rs:83-84 reads "ADR-0008 Tier 2: the coverage impact is displayed first, and `--force`/`--yes` is required when a unit is left below its policy", and the `--force` help at :91-95 says it waives the prompt "when the retirement leaves a live version below its policy but above zero". The implementation asks unconditionally: `cartridge_retire` builds `facts` and calls `consent::confirm(&action, &facts, force || assume_yes)` outside any at-risk/below-policy test (src/cli/operations.rs:1119-1145), which its own doc at 975-978 states deliberately — "consent is still asked EVERY time — retiring a medium permanently is a declaration worth confirming even when no unit loses coverage by it". The two operator-facing strings and the code disagree about when the flag is needed.

**ADR basis:** ADR-0008: "When stdin is **not** a terminal and no `--yes`/`--force` was given, the operation **refuses** with a non-zero exit." The help therefore determines whether a scripted invocation succeeds, and issue #147's own commit (626040a) states the rule being applied: "Operator-facing help that contradicts the code is worse than none."

**Failure scenario:** An operator writes a maintenance script that retires a worn cartridge whose every unit has two other copies, and omits `--yes` because the help says the flag is only required "when a unit is left below its policy". The run is non-interactive, so `consent::confirm` takes the non-TTY branch and returns "retire cartridge \"BC001\" permanently refused: non-interactive session with no confirmation given", with a fully-covered facts list that names no shortfall at all. The script fails on a cartridge that met every stated precondition.

**Suggested fix:** Change both strings to say consent is asked on every `cartridge retire` and that `--force`/`--yes` supplies it, keeping the existing (correct) sentence that neither reaches the Tier-3 refusal; regenerate docs/man/tapectl-cartridge-retire.1.

**Verifier (adversarial):** Verified at both ends and it survives, though the finding points at the wrong side of the divergence.

Help (src/cli/cartridge.rs:83-84): "ADR-0008 Tier 2: the coverage impact is displayed first, and `--force`/`--yes` is required when a unit is left below its policy." `--force` help (:91-95): "Waive the ADR-0008 Tier-2 prompt: proceed when the retirement leaves a live version below its policy but above zero."

Code (src/cli/operations.rs:1119-1145): the facts vector is built and `crate::cli::consent::confirm(&action, &facts, force || assume_yes)` is called outside any below-policy or at-risk test, and `facts` always contains at least the trailing "cartridge ... will never be written again" line. Its own doc says so deliberately (operations.rs:975-978): "consent is still asked EVERY time — retiring a medium permanently is a declaration worth confirming even when no unit loses coverage by it." `confirm_with` (src/cli/consent.rs:~85) refuses on the non-TTY branch with "refusing rather than assuming consent (re-run with --yes to proceed)", so a scripted retire of a fully-covered cartridge fails, as the scenario describes.

The finding understates its own case: ADR-0011 says the same thing the help does — "`--force`/`--yes` is required when any unit is left below its policy (Tier 2) ... When nothing loses coverage there is nothing to consent to and it asks for nothing." So the help matches the ADR and the CODE is the deviation. Whichever side is judged wrong, they disagree, and the disagreement decides whether a non-interactive invocation succeeds.

Low is the right severity: it fails loudly with a message naming `--yes`, nothing is lost, and the fix is a one-line condition or a one-line doc edit.

---

#### 9. [low] volume_write's `UPDATE volumes SET mam_capacity_bytes` still runs before three tape-side refusals that were going to fire anyway

**Location:** `src/volume/write.rs:918`

**Evidence:** The MAM bookkeeping UPDATE is issued at write.rs:918-925, immediately after `detect` at line 916:

    let det = crate::tape::media_detect::detect(device, &backend.device_sg);
    let mam = det.mam.clone();
    if mam.max_capacity_bytes.is_some() || mam.remaining_bytes.is_some() {
        let _ = conn.execute(
            "UPDATE volumes SET mam_capacity_bytes = ?1, mam_remaining_at_start = ?2
             WHERE id = ?3",
            params![mam.max_capacity_bytes, mam.remaining_bytes, volume_id],
        );
    }

Every tape-side refusal in volume_write comes AFTER it: `binding::corroborate_volume` (write.rs:936 - the wrong-cartridge fact refusal), `check_loaded_generation` (write.rs:942), `check_drive_can_write` (write.rs:958), then `built.validate` (write.rs:1038 - capacity exceeded / corrupt staged slice / EscrowRecipientMissing), `TapeStore::open` (write.rs:1063), `check_fresh_write_contact` (write.rs:1074 - the ADR-0003 already-sealed refusal) and `into_validated` (write.rs:1084 - the at-contact capacity refusal).

The values written are read from WHATEVER MEDIUM IS LOADED. On the wrong-cartridge refusal at line 936 the row for volume X now permanently records cartridge Y's `mam_capacity_bytes` and `mam_remaining_at_start`, and the write is then refused with "wrong cartridge: volume ... was initialised on <recorded>, the drive holds <loaded>" (binding.rs:1420-1426).

The distinction from the other mutation on this path: `corroborate_contact` also writes (it learns a NULL serial via `record_medium_serial`, binding.rs:1546), but only after every one of its own refusals has run and the loaded medium has been proven to be the bound cartridge - the comment at binding.rs:1528-1540 states exactly that ("every refusal above has already run"). The MAM UPDATE has no such protection: it runs before corroboration establishes which cartridge is even in the drive.

Honest scope: I grepped `mam_capacity_bytes`/`mam_remaining_at_start` across src/ - no reader outside `db/models.rs:188-189`, the ID-thunk template (`layout.rs:144`, fed from `mam` in memory, not from the row) and the rebuild INSERT. Nothing gates on the column today, which is why this is medium and not high.

Nothing pins the current order as correct, and nothing can catch it either: every ordering test in write.rs (the status loop at 4666, the #199 rebuild test at 4785, the #166 drive-generation tests added by 191606f) uses a nonexistent device path, so `detect` returns no MAM and the `if` at line 919 is never entered.

**ADR basis:** ADR-0012, "The write target" (docs/adr/0012-...md:126-129): "What was lost is the *ordering* #161 exists to guarantee - the refusal arrived from the tape side after `find_staged_data`, the `mam_capacity_bytes` UPDATE and `TapeStore::open` had already run." The ADR names this exact UPDATE as the mutation a tape-side refusal must not have performed. #199 fixed that ordering only for the catalog-side facts (`is_write_target`, `has_completed_write`); the tape-side refusals at write.rs:936/942/958 still arrive after the UPDATE. write.rs:814-818 restates the same rule in the code's own words ("nothing after this point -- `find_staged_data`, the MAM `UPDATE`, `check_loaded_generation`, `TapeStore::open`, `check_fresh_write_contact`, `bind_late` -- ever runs"), and the UPDATE sits above two of the items on its own list.

**Failure scenario:** Volume L6-0007 was initialised on cartridge A (MAM capacity 2 499 053 MiB). The operator loads cartridge B by mistake and runs `tapectl volume write L6-0007`. `detect` reads B's MAM; write.rs:920 commits B's `max_capacity_bytes` and `remaining_bytes` onto L6-0007's row; `corroborate_volume` at line 936 then refuses with "wrong cartridge". Nothing rolls the UPDATE back (it is `let _ = conn.execute`, outside any transaction). L6-0007's catalog row now states a capacity and a remaining-at-start belonging to a cartridge it was never on, and will keep stating it until a successful write overwrites it. The same thing happens for a routine over-capacity refusal at write.rs:1038, a missing-escrow refusal, and the ADR-0003 already-sealed refusal at write.rs:1074.

**Suggested fix:** Move the `UPDATE volumes SET mam_capacity_bytes = ..., mam_remaining_at_start = ...` block down to sit with `bind_late` (write.rs:1101-1108) - after `into_validated`, before `plan()` - for exactly the reason bind_late's own comment gives: it is the latest point before anything is written, and by then every refusal the write can still suffer has fired. `det`/`mam` are already in scope there, so no second device open is needed, and the ID thunk's MAM fields (which read `mam` in memory, not the row) are unaffected. Then extend the #166 ordering tests with a fixture whose MAM read is non-empty, so the ordering is actually pinned.

**Verifier (adversarial):** REFUTATION ATTEMPTED, PARTIALLY SUCCEEDED — the mechanism is real but the ADR basis overreaches and the severity is overstated.

What the cited code actually says (src/volume/write.rs:916-925, read directly):

    let det = crate::tape::media_detect::detect(device, &backend.device_sg);
    let mam = det.mam.clone();
    if mam.max_capacity_bytes.is_some() || mam.remaining_bytes.is_some() {
        let _ = conn.execute(
            "UPDATE volumes SET mam_capacity_bytes = ?1, mam_remaining_at_start = ?2
             WHERE id = ?3",
            params![mam.max_capacity_bytes, mam.remaining_bytes, volume_id],
        );
    }

and `binding::corroborate_volume` is at write.rs:936, `check_loaded_generation` at 942, `check_drive_can_write` at 958. The ordering claim is accurate, the UPDATE is unconditional-on-success (`let _ =`), untransacted, and nothing rolls it back.

IN SCOPE, and worse than pre-existing: `git log -L 905,930:src/volume/write.rs` shows commit c8e5c65 ("volume: corroborate the loaded medium at every contact (issues #193, #164)"), inside 5d4cc43..HEAD, is the commit that ADDED `corroborate_volume` — and placed the new refusal below the pre-existing UPDATE.

WHERE THE FINDING OVERREACHES (1) — the ADR citation. docs/adr/0012...md:120-133 reads: "Bytes were never at risk... What was lost is the *ordering* #161 exists to guarantee — the refusal arrived from the tape side after `find_staged_data`, the `mam_capacity_bytes` UPDATE and `TapeStore::open` had already run. The ruling is **not** to have rebuild write a better status. It is to stop using a status as the proxy: `is_write_target` additionally refuses a volume that already has write or slice rows attached." That paragraph DESCRIBES the pre-#199 symptom; the ruling it hands down is on `is_write_target`/`has_completed_write` — a CATALOG-side fact that should never have required touching the tape — and that ruling is implemented (coverage.rs:158, write.rs:819/836). The ADR nowhere rules that the UPDATE must sit below every TAPE-side refusal, and the distinction matters: corroborate/generation/drive-can-write inherently require reading the medium, which is the same `detect` call that produced these values. So the finding survives on the task's own definition ("an ordering that lets a mutation happen before a refusal"), not on ADR-0012.

WHERE THE FINDING OVERREACHES (2) — severity. Two independent reasons this is low, not medium:
  (a) Only ONE of the listed refusals writes a FOREIGN cartridge's numbers: `corroborate_volume`'s wrong-cartridge branch (binding.rs:1416-1425) and its `refuse_retired` sibling. Every other refusal the finding names (check_loaded_generation:942, check_drive_can_write:958, validate:1038 capacity/escrow, check_fresh_write_contact:1074 ADR-0003) runs AFTER corroboration has already proved the loaded medium IS the bound cartridge — so `mam_capacity_bytes` is that volume's own cartridge's correct figure and only `mam_remaining_at_start` is semantically stale ("at start" of a write that never started).
  (b) I re-ran the reader grep independently and it is worse for the finding than stated: `models::Volume` (models.rs:181-189) is never CONSTRUCTED anywhere — `grep -rn "models::Volume\b|: Volume\b|Volume {" src/` returns only the struct definition itself. No SELECT, no CLI display, no `--json`, no report reads either column; layout.rs:144 is fed from the in-memory `mam`, rebuild.rs:804 from `meta` parsed off the tape. So the columns cannot mis-state coverage to anyone, and the next successful write overwrites them. That is "cosmetic-but-incorrect" = low.

The test claim checks out (write.rs:4666/4785 use `/nonexistent/...` paths so `detect` yields no MAM and the `if` never fires), but those tests pin the CORRECT ordering for the catalog-side refusals — they do not pin wrong behaviour.

---

#### 10. [low] RESTORE.sh, the heir-facing recovery script, prints a MiB-divided figure as "MB"

**Location:** `src/volume/layout.rs:1050`

**Evidence:** The per-slice decrypt line in the generated script reads `info "  decrypted ($((bytes / 1048576)) MB)"` — `bytes` is `wc -c` of the decrypted dar slice (a measured data size, correctly binary), divided by 1 048 576 and labelled with the decimal unit. The generator is live, not dead code: `src/volume/build.rs:261` materialises File 2 as `layout::generate_restore_script_v2(&inputs.label, total_files)`. It is the same class-1 mislabel d4d6b71 fixed across the Rust display surfaces; the shell fragment was not part of that sweep.

**ADR basis:** ADR-0012, "data sizes are binary ... the two are named apart". The script is the ADR-0007/0009 heir path, the one surface read by someone who has no tapectl and no other source for the convention.

**Failure scenario:** An heir runs RESTORE.sh --restore on a volume whose slices are 10 GiB. The script reports "decrypted (10240 MB)" for each; the heir sizes the scratch filesystem from that number and is short by ~7% (10.74 GB vs 10.24 GB per slice) across the whole restore.

**Suggested fix:** Change the label to MiB. Note the constraint: RESTORE.sh bytes are pinned by tests/on_tape_golden.rs:30-37 ("RESTORE.sh bytes changed. That is an on-tape format change and a CTO decision"), so this is a format change with a deliberate re-pin, not a two-character edit — CLAUDE.md's rule that a golden failure is never re-pinned on its own applies.

**Verifier (adversarial):** Could not refute. src/volume/layout.rs:1050 is `info "  decrypted ($((bytes / 1048576)) MB)"` with `bytes=$(wc -c <"$dar_dir/restore.$num.dar")` two lines above — a measured data size divided binarily and labelled decimally. Tried "dead v1 generator": the surrounding fragment uses `read_tape_raw`, `idx_hash` and "checksum verified against front index", i.e. Layout v2, and src/volume/build.rs:261 materialises File 2 from `layout::generate_restore_script_v2`, so it is live. Tried "the ADR governs only tapectl's own output": ADR-0012's naming rule is stated unconditionally and this is the heir surface. One material caveat the parent must carry: tests/on_tape_golden.rs:26-37 pins RESTORE.sh's sha256 with "RESTORE.sh bytes changed. That is an on-tape format change and a CTO decision" — so the fix is gated, not a free relabel. Low: a cosmetic-but-incorrect label, not a wrong byte count.

---

#### 11. [low] `capacity_override`'s own doc comment still declares the #168 two-byte-counts gap that #200 closed

**Location:** `src/config.rs:171`

**Evidence:** The field doc reads: "**Known gap (issue #168):** only `LtoBackendConfig::planning_capacity_bytes` ... reads this field decimally; `volume init` ... still parses it with the binary `staging::parse_size_to_bytes` ... Until that is fixed too, the SAME string means two different byte counts depending on which of those two paths reads it" (src/config.rs:171-180). That is false at HEAD: commit 61032e9 changed `volume_init` to `crate::media::parse_capacity_to_bytes` (src/volume/write.rs:293-295) and pinned it with a source-scan test (src/volume/write.rs:7201-7231) precisely so it cannot regress.

**ADR basis:** ADR-0012, "cartridge capacities are decimal ... One parser cannot serve both" — the ruling is now fully implemented, and the doc on the field that owns the string says otherwise.

**Failure scenario:** A maintainer reading `LtoBackendConfig` to answer "what does capacity_override mean on the write path?" is told authoritatively that the write path reads it binarily. They either re-fix a fixed bug, or — worse — reason about a downstream capacity gate on the assumption that `volumes.capacity_bytes` is a binary interpretation of the string, and size the ENOSPC margin from a number that is 10% off in their head.

**Suggested fix:** Delete the "Known gap" sentence (and its "Until that is fixed too" clause), leaving the first two lines that state the decimal rule; cite #200 and the write.rs pin instead.

**Verifier (adversarial):** Survives, though it is the borderline one. The claim is verifiable in both directions: src/config.rs:171-180 still reads "**Known gap (issue #168):** ... `volume init` (`volume::write::volume_init`, the path that actually decides and stores `volumes.capacity_bytes`) still parses it with the binary `staging::parse_size_to_bytes` ... the SAME string means two different byte counts," while src/volume/write.rs:293-295 is `Some(crate::media::parse_capacity_to_bytes(v)?)` and commit 61032e9 ("volume/init: capacity_override is decimal, like every other reader of it") closed it deliberately, with the write.rs:285-292 comment recording it in the past tense ("It parsed BINARY until issue #200"). So the doc is not merely stale, it is false in a way that contradicts a comment 200 lines away. Tried to kill it on the rubric ("the spec is the ADRs, not the code's own comments" — a stale comment is not one of the enumerated finding types): it survives only because it is a demonstrably false claim about WHICH parser reads the field that sets `volumes.capacity_bytes`, the value ADR-0010 decision 3 makes authoritative for every later capacity gate. Doc-only, zero runtime effect — low.

---

#### 12. [low] `catalog rebuild`'s barcode-collision refusal tells the heir to run `volume init` on the loaded tape — an act ADR-0003 refuses even with `--force`, leaving no way out of the branch

**Location:** `src/volume/rebuild.rs:1515`

**Evidence:** rebuild.rs:1505-1519, `resolve_mam_identity`'s collision arm:

    "If it IS this cartridge, bind the serial onto it by hand, then re-run this rebuild:\n    \
     tapectl volume init <a throwaway label> --device <dev> --cartridge {serial}\n..."

The tape in the drive at that moment is the one being rebuilt from — by construction SEALED (`rebuild_from_store` has already read its File 0 layout pointers and is about to walk the front index). `volume_init` calls `check_fresh_write_contact(&mut store, label, &candidate_uuid, None, force)` (src/volume/write.rs:324). With `seal_position = None`, `check_tape_contact` (src/volume/session.rs:380-400) takes the identity-mismatch branch and FIRST probes the loaded tape's OWN `[layout].seal_marker`; it parses, so the outcome is `AlreadySealed`, which `decide_fresh_write_contact` refuses regardless of `--force` — pinned by `check_fresh_write_contact_foreign_sealed_tape_refuses_even_with_force` (src/volume/write.rs:6120-6150), which asserts the ADR-0003 citation specifically so a forced overwrite cannot slip through.

So the recipe cannot run. Nor is there a second way to do what it describes: `cartridge edit --serial` writes `operator_serial`, NEVER `serial_number` (src/cli/cartridge.rs:375-381, 969), and rebuild's own resolve consults only `serial_number` — `select_cartridge(tx, "serial_number", serial)` at rebuild.rs:1495 and `select_cartridge(tx, "barcode", serial)` at rebuild.rs:1505; `select_cartridges_by_operator_serial` (src/volume/binding.rs:531) is reached only from `lookup_cartridge`. No command writes `cartridges.serial_number` by hand, so the refusal re-fires on every re-run.

**ADR basis:** ADR-0003 (a sealed volume is immutable; the File 0 check is the consent point and `--force` never defeats an `AlreadySealed` outcome). The DR promise in CLAUDE.md/ADR-0005 — "catalog rebuild with the operator or escrow key" — requires the refusal's remediation to be executable; naming a command that is structurally refused turns a recoverable state into a dead end on the one path an heir has.

**Failure scenario:** An operator registers a cartridge by hand before first use, typing the serial printed on the shell as the BARCODE (`cartridge register S ...`), then `volume init`/`volume write` binds and seals it from a MAM read, so File 0 carries `cartridge_serial = S`, `cartridge_identity_source = "mam"`, while `cartridges.serial_number` is still NULL. The catalog is lost. The heir runs `catalog rebuild --from-volume --key operator.key`: `select_cartridge("serial_number", S)` misses, `select_cartridge("barcode", S)` hits, and the rebuild aborts with the recipe above. Running it gives an ADR-0003 `AlreadySealed` refusal; adding `--force` gives the same refusal; `cartridge edit S --serial S` changes a different column and the rebuild refuses identically on re-run. The only escape is `cartridge relabel S <new>` — which the message presents as the answer for the opposite case ("if it is a DIFFERENT cartridge"), so the heir is told to assert something they believe to be false.

**Suggested fix:** Replace the first recipe. Either (a) point at `cartridge relabel {serial} <new-barcode>` for BOTH branches, explaining that relabelling frees the string so rebuild can register the chip-verified row itself; or (b) have `resolve_mam_identity` consult `operator_serial` the way `lookup_cartridge` does (binding.rs:531's `serial_number IS NULL` query), so `cartridge edit {barcode} --serial {serial}` becomes the real, executable "bind it by hand" step the message promises. Do not name `volume init` on a tape being rebuilt from, under any wording.

**Verifier (adversarial):** The *mechanism* survived every attempt to refute it; the *headline* did not.

Survived: rebuild.rs:1505-1519 does print `tapectl volume init <a throwaway label> --device <dev> --cartridge {serial}`, and that command is structurally refused on the tape in the drive. write.rs:324 runs `check_fresh_write_contact(&mut store, label, &candidate_uuid, None, force)` BEFORE the catalog transaction; with `seal_position = None`, session.rs:375-400 takes the identity-mismatch branch, and since a sealed tape's File 0 carries `[layout] seal_marker = {seal_marker}` (layout.rs:147) that parses, it returns `AlreadySealed`; write.rs:1877-1883 refuses it with the ADR-0003 text and never consults `allow_overwrite`. I also confirmed the finding's supporting claims: `cartridge register --serial` and `cartridge edit --serial` both write `operator_serial` only (cartridge.rs:319-333, 375-383, and the doc at 123-125: `serial_number` "no operator command may ever touch"), and the cartridge subcommand list (cartridge.rs: Register/List/Info/Move/Retire/MarkErased/Edit/Relabel/Unretire) has no delete.

Did NOT survive: "leaving no way out of the branch". The same message's second branch works. `cartridge relabel S <new>` renames the colliding row; the re-run's `select_cartridge(tx, "barcode", serial)` at rebuild.rs:1505 then misses and the auto-register arm creates a correct `{barcode: S, serial_number: S}` row bound to the volume. The residue is one orphan row with no serial and no volumes, which the next `volume init` on that plastic never resolves to (it matches on `serial_number`). Nothing is lost and no coverage is mis-stated — the heir is only asked to assert a relabel they may believe is semantically wrong.

So what is left is a wrong sentence in an error message whose adjacent sentence is a working escape. By the rubric that is cosmetic-but-incorrect, not "recoverable but wrong" with a real recovery cost: corrected to low.

---

#### 13. [low] A rebuild that learns a medium serial reports `no_changes: true` while its own stderr says it learnt one

**Location:** `src/volume/rebuild.rs:341`

**Evidence:** rebuild.rs:336-342 calls `binding::corroborate_volume(conn, volume_id, &ident.label, &medium)?` and discards the returned `Corroboration`. That function can return `Corroboration::SerialLearned` after `record_medium_serial` has written `cartridges.serial_number` and its events row, and it prints to stderr: `note: cartridge "{barcode}" had no medium serial recorded; learnt {serial} from the tape in the drive at this contact.` (src/volume/binding.rs:1307-1313).

Nothing propagates that into the report. `report.serial_learned` is set only by `resolve_operator_identity`'s own learn branch (rebuild.rs:1614-1622), which is unreachable once the serial was already learnt pre-transaction — `select_cartridge(tx, "serial_number", observed)` now finds the row (rebuild.rs:1584), `already_mounted == resolved.id` returns early at rebuild.rs:1382, and every counter stays zero. `RebuildReport::is_noop()` (rebuild.rs:193-206) therefore returns true, and `cli::catalog` prints/emits `"no_changes": true`.

**ADR basis:** ADR-0001 (the catalog is a ledger of claims; a run that changed the ledger must say so) and ADR-0004's Tier-1 evidence-display rule as this repo applies it — the report is the operator's evidence of what a DR run did, and it must not contradict the same run's own stderr.

**Failure scenario:** An heir re-runs `catalog rebuild --from-volume --json` on a tape whose volume row already exists and whose bound cartridge was registered by barcode with no serial (identity_source `operator`). The drive reports a serial. The run writes `cartridges.serial_number` plus an events row and prints the "learnt" note on stderr, but the JSON says `"no_changes": true`. A script (or an operator) trusting `no_changes` concludes the catalog is untouched and skips the `db backup` that a changed catalog warrants.

**Suggested fix:** Capture the outcome: `if let Corroboration::SerialLearned { .. } = binding::corroborate_volume(...)? { report.serial_learned = true; }`. `serial_learned` is already in `is_noop()` and in the JSON, so nothing else changes.

**Verifier (adversarial):** Could not refute; every link checks out and the report contradicts its own documented contract.

Reachability, end to end: a `volume init` on a drive with no readable serial binds with `identity_source = 'operator'` and `serial_number` NULL, and File 0 records `cartridge_identity_source = "operator"` (layout.rs `[media]` block). On a later rebuild from a drive that CAN read the serial: `File0Facts::chip_serial()` returns None for an `operator` source, so binding.rs:1428's refusal is skipped; the row has no serial and `identity_source != "mam"`, so binding.rs:1550-1562 calls `record_medium_serial` and returns `Corroboration::SerialLearned`, printing the stderr note at binding.rs:1308-1313. rebuild.rs:336-342 calls `corroborate_volume(...)?` and DISCARDS the value. In the transaction, `resolve_operator_identity`'s `select_cartridge(tx, "serial_number", observed)` at rebuild.rs:1583-1585 now hits (the serial was just written), returns before the `report.serial_learned = true` branch at 1613 — the only assignment in the file (`grep serial_learned` → 150, 153, 206, 1613) — and `already_mounted == resolved.id` returns early at rebuild.rs:1390. All counters stay zero, so `is_noop()` (rebuild.rs:193-206) is true and cli/catalog.rs:590 emits `"no_changes": true` with `"serial_learned": false`, while the text path prints "no changes — the catalog already knew this volume".

The contract is stated in the code itself: `serial_learned`'s doc at rebuild.rs:150-153 defines it as "whether that observation was recorded onto a row" — it WAS recorded, by this very run, plus an events row — and `is_noop()`'s doc says "True when the run changed nothing". So this is a derivation disagreeing with its own definition, not a missing feature.

Low stands: the write itself is correct and beneficial; only the run's summary understates it.

---

#### 14. [low] `stage create`'s own refusal points at `snapshot create`, which now creates nothing when content is unchanged

**Location:** `src/cli/stage.rs:297`

**Evidence:** src/cli/stage.rs:297: `"no unstaged snapshot for unit \"{name}\" — run `tapectl snapshot create` first"`. Before this diff that instruction worked: `snapshot create` minted `MAX(version)+1` unconditionally. This diff adds `src/unit/content_match.rs` and the short-circuit in `staging::snapshot_create_detailed` (src/staging/mod.rs:96-133), so for a unit whose directory has not changed, `snapshot create` returns the existing row with `minted: false` and `src/cli/snapshot.rs:187` prints "unit \"{name}\" is unchanged since v{n}; no snapshot created" and exits 0. Re-running `stage create <unit>` then produces the identical refusal. The command that does work — `stage create <unit> --version <n>` — is defined two arms below (stage.rs:301-330) and is never named by this message.

**ADR basis:** ADR-0012, "a version is minted only when content changed". The refusal's remedy was written against the superseded unconditional-mint behaviour and was not updated with the rule.

**Failure scenario:** Operator wants a second copy of an unchanged unit whose stage set was cleaned. `tapectl stage create docs` → "no unstaged snapshot for unit \"docs\" — run `tapectl snapshot create` first". `tapectl snapshot create docs` → "unit \"docs\" is unchanged since v3; no snapshot created". Repeat forever; `--version 3`, the command that works, is never mentioned.

**Suggested fix:** Extend the message: "...  — run `tapectl snapshot create` first if the contents changed, or re-stage an existing version with `tapectl stage create {name} --version <N>` (`tapectl snapshot list --unit {name}` shows them)."

**Verifier (adversarial):** SURVIVES, but it is the same defect as finding 3 seen from the other end, and narrower.

Verified verbatim at stage.rs:295-299: "no unstaged snapshot for unit \"{name}\" — run `tapectl snapshot create` first", reached when no snapshot has status='created'. And snapshot create now short-circuits (staging/mod.rs:96-140) for unchanged content, printing "unit X is unchanged since vN; no snapshot created" (cli/snapshot.rs:186-191).

Partial refutation that lands: the refusal is CORRECT in its other, commoner population — a unit whose content HAS changed but which has not been snapshotted yet. `snapshot create` then mints and `stage create` proceeds. The text is stale only for the unchanged-content case, which is a subset of when it fires. It is also not an ADR contradiction: ADR-0012 rules on when a version is minted, not on what a refusal must say.

What survives: for the unchanged case the remedy loops, and `--version <n>` — defined two arms below at stage.rs:301-330 — is never named. Real, but cosmetic-but-incorrect text with a working (unnamed) escape: low, not medium. Counting it and finding 3 as two mediums double-counts one stale-remedy problem.

---

#### 15. [low] The escrow-mismatch remedy names `key import --escrow`, which is refused in exactly the state that produces the finding

**Location:** `src/cli/audit.rs:835`

**Evidence:** Pre-existing at 5d4cc43, not introduced by this range — reported because it is the exact defect shape under review and it is still live. `escrow_identity_findings` returns early when NO escrow is registered (audit.rs:781-783: "nothing registered; nothing to compare against"), so the finding can only fire while an escrow identity IS registered. Its action is `"tapectl key import --escrow {}   # the ORIGINAL escrow public key, from the heir kit"` (audit.rs:835). `key import --escrow` routes through `adopt_escrow_recipient`, which raises `escrow_already_registered_error` (src/cli/key.rs:576-583): "an escrow recipient is already registered — ADR-0005 permits exactly one permanent escrow identity for the life of the archive; replacing it is a deliberate, separate act, not automated by this command." The same dead-end recipe is printed by `catalog rebuild` at src/cli/catalog.rs:725: "attest them with `catalog rebuild --key <the REGISTERED escrow key>` (import the original with `key import --escrow` first)". `tapectl init` registers an escrow by default (src/main.rs:455-465), so the post-disaster machine this finding exists for always has one.

**ADR basis:** ADR-0005 ("one identity generated once ... exempt from key rotate") plus ADR-0012's consequence recorded via #139: the DR recipe is `init --escrow-public-key <original>`, and no command replaces a registered escrow identity. The action text predates that ruling.

**Failure scenario:** Machine rebuilt after site loss: plain `tapectl init` mints a new escrow, `catalog rebuild --from-volume` restores the tapes. `tapectl audit` reports `escrow_identity_mismatch` with action `tapectl key import --escrow age1old...`. Running it fails with "an escrow recipient is already registered". The real remedy — start over from a fresh home with `tapectl init --escrow-public-key age1old...` — is never named.

**Suggested fix:** Replace both action strings with the #139 recipe: "re-initialise a fresh tapectl home with `tapectl init --escrow-public-key <age1old...>` (no command replaces a registered escrow identity — ADR-0005), then re-run `catalog rebuild`." Keep `key import --escrow` only for the `init --no-escrow` path, and say so.

**Verifier (adversarial):** SURVIVES as a defect, but is OUT OF SCOPE for the diff under review, and the finding concedes it.

Verified: escrow_identity_findings returns early on `None` escrow (audit.rs:780-783), so it fires only while one IS registered; its action is "tapectl key import --escrow {}" (audit.rs:834-836); adopt_escrow_recipient raises escrow_already_registered_error at key.rs:485-486 ("an escrow recipient is already registered — ADR-0005 permits exactly one permanent escrow identity...", key.rs:576-583). Genuine dead end.

Scope refutation, which I checked both ways: `git log -S 'the ORIGINAL escrow public key, from the heir kit' 5d4cc43..HEAD` is EMPTY (the text landed in d5c9638), and `git log -S 'escrow_already_registered_error' 5d4cc43..HEAD` is also EMPTY (the refusal landed in the T2 escrow-wiring commits). audit.rs's only commit in range is 346f55a, a test fixture. So BOTH halves predate 5d4cc43 — unlike findings 3/4, nothing in this diff created or broke this pairing. It is a standing defect in the codebase, not a regression in the 220 commits under review.

Severity corrected to low for this review: it does not misstate coverage (the finding's own message text correctly explains the situation and names the suspected original key) — only the action line is unrunnable, and the true recipe (init --escrow-public-key, #139) lives in CLAUDE.md and ADR-0005.

---

#### 16. [low] `volume abort`'s consent fact says the staged slices are released by `staging clean`, which will never release them after an abort

**Location:** `src/volume/write.rs:1383`

**Evidence:** Added in this diff. The fourth ADR-0004 fact shown at the `volume abort` consent point reads: "The staged slices stay pinned on disk until `tapectl staging clean` runs, so the data can be re-staged or written to another volume." (src/volume/write.rs:1383-1385). `volume_abort` then sets every `writes` row for the volume to `'aborted'` (write.rs:1395-1400). `clean_staging`'s non-force candidate query (src/staging/clean.rs:90-97) requires `NOT EXISTS (SELECT 1 FROM writes w WHERE w.stage_set_id = stage_sets.id AND w.status <> 'completed')`, so an `aborted` row blocks the set permanently; clean.rs's own doc comment (lines 30-36) says `--force` is "the deliberate operator override for a stage_set stuck behind a copy that will never complete" — precisely this case.

**ADR basis:** ADR-0008: "tapectl prompts ... and displays the ADR-0004 coverage facts at that moment, which is the one place the operator is guaranteed to read them." A fact shown there must be true read alone.

**Failure scenario:** Operator aborts a killed session on L6-0011, reads that `staging clean` will release the slices, and later runs `tapectl staging clean` to recover disk. It reports zero sets cleaned and zero bytes freed with no explanation, and the staged slices occupy the staging directory indefinitely.

**Suggested fix:** Change the fact to name the flag: "The staged slices stay pinned on disk; because this session's `writes` row becomes `aborted`, plain `tapectl staging clean` will not release them — use `tapectl staging clean --force`, or write them to another volume first."

**Verifier (adversarial):** SURVIVES, and this is the one finding whose ADR basis is exactly right.

Verified: write.rs:1383-1385 is the fourth fact passed to consent::confirm at :1387 — i.e. genuinely at the ADR-0008 consent point ADR-0008:36-38 calls the one place the operator is guaranteed to read. write.rs:1392-1400 then sets every `writes` row to 'aborted'. clean.rs:88-96's non-force candidate SQL requires NOT EXISTS(... w.status <> 'completed'), so an 'aborted' row permanently disqualifies the set, and clean.rs's own doc comment at :26-36 names this case: "`--force` is the deliberate operator override for a stage_set stuck behind a copy that will never complete". The consent text therefore contradicts the module that owns the behaviour. cli/volume.rs:475-478 repeats the same claim after the abort, so there is no later correction.

Refutation that partly lands: the fact's stated CONSEQUENCE is true — the slices are not deleted and can be re-staged or written to another volume. The sentence is imprecise (it omits `--force`) rather than affirmatively promising a release that destroys something, and it errs on the conservative side (data kept, not lost).

Low is right, as filed: a consent-point fact that under-specifies the recovery command, costing the operator a confusing zero-result `staging clean`.

---

#### 17. [low] Test comment (mirroring `bind_cartridge`'s doc) misattributes the writer of `volumes.capacity_bytes` to `volume_write`

**Location:** `src/volume/binding.rs:2625`

**Evidence:** `binding_never_touches_the_volumes_own_resolved_capacity` opens with "`volumes.capacity_bytes` is written by `volume_write` before `bind_cartridge` is ever called (ADR-0010 decision 3 — decided once at init, from the same resolved figure)". The sentence contradicts itself and the code: the only non-test `INSERT INTO volumes (... capacity_bytes ...)` in the crate is in `volume_init` (src/volume/write.rs:333-340), and no path UPDATEs the column afterwards — `volume_write` only UPDATEs `mam_capacity_bytes`/`mam_remaining_at_start` (src/volume/write.rs:920). The same wrong attribution was written into `bind_cartridge`'s own doc comment by 729bbfa ("`volumes.capacity_bytes` is written by `volume_write` alone, from a value this function never sees"). The assertion itself is sound; only the stated ordering premise is false — and `bind_cartridge` is in fact reached from `volume_init` (the same transaction that inserts the row) as well as from `bind_late`.

**ADR basis:** ADR-0010 decision 3 as amended: capacity "is decided ONCE at init and stored in `volumes.capacity_bytes`; no path reads capacity from config after init." The ADR names `volume init` as the deciding point; the comment names `volume write`.

**Failure scenario:** A maintainer chasing where a volume's capacity is set follows the comment to `volume_write`, finds only the `mam_capacity_bytes` UPDATE, and either concludes the column is unset on the init path or adds a second writer there — reintroducing exactly the two-writers drift ADR-0010 decision 3 exists to prevent. The test would stay green throughout, since it only asserts the column is unchanged by `bind_cartridge`.

**Suggested fix:** Replace `volume_write` with `volume_init` in both the test comment (binding.rs:2625) and `bind_cartridge`'s doc comment, citing src/volume/write.rs:335.

**Verifier (adversarial):** I tried to kill this and could not. The attribution is verifiably wrong: the only non-test writers of `volumes.capacity_bytes` are the INSERT in `volume_init` (src/volume/write.rs:334-347, in the same transaction that then calls `bind_cartridge` at line 349) and the INSERT in `catalog rebuild` (src/volume/rebuild.rs:794-795). `volume_write` (src/volume/write.rs:784) only ever UPDATEs `mam_capacity_bytes`/`mam_remaining_at_start` (write.rs:920). The file uses `volume_write` to mean the function, explicitly contrasted with `volume_init` ('volume_init only ever writes the provisional identity thunk; real capacity gating happens in volume_write's pre-open validate', write.rs:316-318), so the module-vs-function rescue does not hold. The same wrong attribution appears in `bind_cartridge`'s own doc, added by 729bbfa ('`volumes.capacity_bytes` is written by `volume_write` alone, from a value this function never sees'), with no correcting clause; the test comment at binding.rs:2623-2627 at least adds '(ADR-0010 decision 3 — decided once at init)', which blunts the misdirection. Both comments are inside the reviewed range (bb02eca, 729bbfa). The assertion itself is sound and the ordering claim ('before `bind_cartridge` is ever called') is true — only the naming is wrong, and it names a second real function that does touch a capacity column, which is the mildly confusing part. That is exactly the brief's 'cosmetic-but-incorrect' tier: low, no behavioural consequence, no ADR violated by the code.

---

#### 18. [low] `volume list`'s VERIFIED column re-implements `policy::evidence::compact_age` and disagrees with it on an unparseable timestamp

**Location:** `src/cli/volume.rs:1303`

**Evidence:** `src/policy/evidence.rs:9-11` declares the split: "This module is split into a query half (`remaining_coverage_evidence`, `per_volume_verification`) and a pure formatter half (`describe`, `compact_age`) so the wording can be unit tested without a database." `compact_age` (evidence.rs:437) is the table-column renderer, and its doc says it is "Built from the exact same `weakness_of` computation `tape_detail`/`describe` use, so the column can never disagree with the sentence forms":

    pub fn compact_age(last_verified: Option<&str>, now: chrono::NaiveDateTime) -> String {
        match weakness_of(last_verified, now) {
            Weakness::Never => "never".to_string(),
            Weakness::Age(days) => format!("{days}d ago"),
            Weakness::Unparseable => "unparseable".to_string(),
        }
    }

`cli/catalog.rs:252` calls it. But `volume list`, added by this diff (f14aea0, issue #195), writes its own at `src/cli/volume.rs:1303`:

    fn verified_display(stamp: Option<&str>, now: chrono::NaiveDateTime) -> String {
        match stamp {
            None => "never".to_string(),
            Some(raw) => match chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S") {
                Ok(dt) => format!("{}d ago", (now - dt).num_days()),
                Err(_) => raw.to_string(),
            },
        }
    }

Same parse format, same `never`, same `{n}d ago` — and a different third arm. The doc comment above it (volume.rs:1299-1302) claims "An unparseable stamp renders raw, matching that module's honesty rule rather than silently reading as 'never'", but that module's honesty rule renders the word `unparseable`, not the raw bytes. `src/cli/volume.rs:2508` pins the divergent behaviour as correct.

**ADR basis:** ADR-0004 Tier-1 evidence-age DISPLAY, whose derivation `src/policy/evidence.rs` is declared to own (module header, lines 1-11; `compact_age`'s own doc: "so the column can never disagree with the sentence forms about what counts as 'old' or 'never'"). CLAUDE.md states the rule directly: "policy::evidence owns evidence-age derivation".

**Failure scenario:** A `verification_sessions.completed_at` that is not `%Y-%m-%d %H:%M:%S` — a row written by an older build, a hand-edited catalog, or a row carried through `db import` from a differently-formatted export. `tapectl catalog locate <unit>` prints `unparseable` in the evidence column; `tapectl volume list` prints the raw string (e.g. `2026-09-11T14:02:33Z`) under a header named VERIFIED, where it reads as a verification date rather than as evidence the timestamp could not be read. Two commands, one fact, opposite messages — and the wide raw value also breaks the column the `compact_age` form exists to keep narrow.

**Suggested fix:** Delete `verified_display` and call `crate::policy::evidence::compact_age` from `VolumeRow::display_verified` (volume.rs:1280-1284), keeping only the `copies.is_none() => "—"` guard that is genuinely local to this table. Update the test at volume.rs:2496-2510 to expect `unparseable`.

**Verifier (adversarial):** Survives, but the finding's stated basis is partly fabricated and the parent should discount it accordingly. What is true: `src/cli/volume.rs:1303-1311` re-implements the parse (`%Y-%m-%d %H:%M:%S`, `never`, `{n}d ago`) with a third arm returning the raw string, while `src/policy/evidence.rs:437-443` `compact_age` returns the word `"unparseable"`; both render the same fact — the latest PASSED `verification_sessions.completed_at` (volume.rs:1352 subquery; cli/catalog.rs:252 calls `compact_age`) — so `catalog locate` and `volume list` can print opposite things about one timestamp, and `src/cli/volume.rs:2506-2511` pins the divergent arm. What is NOT true: the finding's adr_basis quotes CLAUDE.md as saying "policy::evidence owns evidence-age derivation" — `grep -rn evidence CLAUDE.md` returns NOTHING; that sentence does not exist in the file. And ADR-0004's Tier-1 requirement is scoped to display "wherever a destructive operation consumes copy coverage" (evidence.rs:3-7 restates it); `volume list` consumes no coverage, so the ADR does not govern this column either. The only real basis is a code comment (`compact_age`'s doc) plus the task's own 'a derivation that disagrees with the one place that owns it' category. Reachability is marginal: every writer of `completed_at` I found uses SQLite `datetime('now')` (session.rs:926, evidence.rs:653, operations.rs:4429, catalog.rs:1154-1163), so the unparseable arm is essentially dead in practice. One mitigation the finding omits: the sibling SENTENCE form `tape_detail` (evidence.rs:414-423) does print the raw stamp — "last verified at {raw} (unparseable timestamp)" — so volume.rs's claim to match the module's honesty rule is half right; what it drops is the marker that says the stamp could not be read. Real, but at the very bottom of low.

---

#### 19. [low] `compaction.tape_only_safety_multiplier` is unvalidated, and 0 zeroes the base copy requirement it is supposed to multiply

**Location:** `src/policy/reclaimable.rs:127`

**Evidence:** ```
if unit.status == "tape_only" {
    let multiplier = config.compaction.tape_only_safety_multiplier as i64;
    required_copies *= multiplier;
    required_locations *= multiplier;
}
```
(src/policy/reclaimable.rs:126-130). Nothing validates the field: `CompactionConfig` (src/config.rs:434-441) is absent from both `size_problems` and `closed_set_problems`, so any `i32` loads. `0` makes `required_copies` 0 and `required_locations` 0, and the subsequent guards are `if copy_count < required_copies` (line 144) and `if required_locations > 0 && ...` (line 158) — both vacuous. A negative value is worse than vacuous. Separately, the refusal text the operator reads hardcodes the factor: `if unit.status == "tape_only" { " (tape-only 2x)" }` (src/policy/reclaimable.rs:152), and a test pins that string (line 422), so the message asserts a 2x rule regardless of what is configured.

**ADR basis:** ADR-0012, retire-family paragraph: the escapes from the floor "are commands, not flags", and one of them is "release the version with `snapshot mark-reclaimable`, whose own preconditions apply" — these preconditions are that release gate. v4.0 §2.11 (not superseded in docs/design-errata.md) states the rule as "the requirements are multiplied by `tape_only_safety_multiplier` (default 2)", i.e. multiplied, never replaced. ADR-0012's config ruling puts the burden on load-time validation rather than on the operator guessing what a value does.

**Failure scenario:** An operator who wants tape-only units held to the ordinary `min_copies = 2` writes `tape_only_safety_multiplier = 0` in `[compaction]`, reading it as "no extra multiplier." `config check` reports valid. `snapshot mark-reclaimable` on a tape-only unit then computes `required_copies = 0`, passes precondition 2 with the superseding snapshot on ZERO eligible volumes, and reports Blocked-free success with the message "...needs 0 (tape-only 2x)" — naming a 2x rule that was not applied. The version is released, and `snapshot purge` is then free to delete the source for content no live volume carries.

**Suggested fix:** Validate at load in `Config::closed_set_problems` (or a new `range_problems`): `tape_only_safety_multiplier >= 1` and `0.0 < utilization_threshold <= 1.0`, each naming the key and the file. Build the "(tape-only Nx)" fragment from the configured multiplier instead of the literal `2x`, and retarget the test at reclaimable.rs:422 accordingly.

**Verifier (adversarial):** Partly refuted; what survives is only the message. (1) The finder's own failure scenario is self-contradictory: with multiplier 0, `required_copies *= 0` (reclaimable.rs:126-130) makes `if copy_count < required_copies` (line 144) false, so the verdict is `Releasable` — which carries no `reason` field at all (lines 170-173). The claimed output 'reports Blocked-free success with the message "...needs 0 (tape-only 2x)"' cannot be printed; no message is printed. (2) The range-validation half falls to the same reasoning as finding 4: `CompactionConfig` (config.rs:434-441) carries `deny_unknown_fields` but no range check, and no ratified rule requires one for an int knob documented as a multiplier (design §2.11:443 'tape_only_safety_multiplier (default 2)'). (3) The gate is not the absolute floor the finder implies: `snapshot mark-reclaimable` takes `--force` ('Override preconditions', cli/snapshot.rs:66-69) and every Blocked reason ends 'use --force to override' — ADR-0012 puts the absolute floor on the RETIRE family (Tier 3), and explicitly says releasing a version with mark-reclaimable '--force is the operator saying in so many words that the version is given up'. A mis-set multiplier therefore grants silently what a flag already grants explicitly. (4) The finder's 'a test pins that string' is wrong: reclaimable.rs:411-424 runs with `Config::default()`, i.e. multiplier 2, so `(tape-only 2x)` is true there — it pins correct behaviour, not wrong. What does survive: `if unit.status == "tape_only" { " (tape-only 2x)" }` (line 152) hardcodes a factor the config owns, so at `tape_only_safety_multiplier = 3` the operator reads 'has 2 copies, needs 6 (tape-only 2x)' — a message stating a rule that was not applied. Real, cosmetic-but-incorrect: low.

---

#### 20. [low] A hand-edited second `[[backends.lto]]` on the same device loads cleanly, and `backend add`'s own error sends the operator there

**Location:** `src/config.rs:1064`

**Evidence:** `backend add` refuses a duplicate name and a duplicate device (`src/cli/backend.rs:128-162`), and its refusal text says: "two backends on the same drive make that resolution ambiguous. Use a different --device-tape, or edit the existing block." `Config::load` applies no such check — `size_problems` (config.rs:642) and `closed_set_problems` (config.rs:712) iterate the backends for value validity only, and neither they nor `stale_lto_fields_message` compare entries to each other. Both resolvers then take the first match and say nothing: `.find(|b| device_matches(&b.device_tape, dev))` at src/config.rs:1064 (`resolve_lto_backend`, the STRICT write path) and again at src/config.rs:1123 (`resolve_device`).

**ADR basis:** ADR-0010's device-resolution rule, as the code itself states it at src/config.rs:1044-1047: resolution is "STRICT... never a silent fallback to 'whichever backend happened to be first', which is the exact shape of issue #141." #174 made `backend add` police that; the config file, which ADR-0012 rules must reject a bad key by name at load, does not police it at all — so the guard exists only on the path that already refuses, and is absent on the path its own error message recommends.

**Failure scenario:** An operator follows `backend add`'s advice and hand-edits, or copies a block to try a second `enospc_buffer`, leaving two `[[backends.lto]]` entries with the same `device_tape` and different `generation`/`enospc_buffer`/`capacity_override`. `Config::load` and `config check` both pass. `volume write --device /dev/tape/by-id/...` silently uses the FIRST block: the pre-flight reserve at src/volume/write.rs:886 comes from a backend the operator thinks they replaced, and `check_drive_can_write` is asked about a generation they thought they corrected. With `--device` omitted the same file is instead refused outright ("multiple LTO backends configured"), so the two spellings of the same mistake behave oppositely.

**Suggested fix:** Add a duplicate check to `Config::size_problems`/a sibling collector: reject two entries sharing a `name`, and two sharing a `device_tape` under `device_matches`, naming both indices and the file — the same rule `cli::backend::add` already implements, moved to the one place both boundaries can read.

**Verifier (adversarial):** Could not refute the mechanism. `resolve_lto_backend` (config.rs:1053-1080) is `.find(|b| device_matches(&b.device_tape, dev))` — first match, no ambiguity error — and `resolve_device` (1117-1127) the same; `size_problems`/`closed_set_problems` iterate backends for value validity only and never compare entries (`grep -n 'duplicate|unique' src/config.rs` finds only doc-comment text). `backend add`'s guard and its 'Use a different --device-tape, or edit the existing block' text are at cli/backend.rs:139-162, exactly as quoted. The project itself agrees this is the current state: issue #174's landing comment says 'Only `backend add` enforces this ... A hand-edited config.toml with two [[backends.lto]] entries on the same device still loads cleanly and passes config check', routed to #186, whose comment says 'what is not defensible is the current state, where the same config is rejected if you reach it one way and accepted if you reach it another'. Two corrections to the finding rather than a refutation. First, the ADR basis is weaker than claimed: the 2026-09-13 audit's own verifier note records that 'ADR-0010:75-79's "no path reads backends.lto.first()" is not violated here — a .find() keyed on device is not that read; the ADR simply does not address two entries canonicalising to the same device', so this is an open scope question, not a contradicted ruling. Second, severity by consequence, not by tracking: a stale `generation` in the first block is bounded by the drive itself (`check_drive_can_write`, and physics — a drive cannot write a medium it cannot write), `capacity_override` is virtual-drives-only by declaration (config.rs:153-166), and a wrong `enospc_buffer`/`usable_capacity_factor` costs a clean abort and a re-init, not bytes. Low.

---

#### 21. [low] `[[archive_sets]]` closed-set values are not validated at load, so `config check` passes a config `archive-set sync` will refuse

**Location:** `src/config.rs:712`

**Evidence:** `closed_set_problems` validates `defaults.compression`, `defaults.checksum_mode`, `logging.level`, `logging.format` and nothing else (src/config.rs:712-728); `size_problems` never looks at `archive_sets[].slice_size` either (src/config.rs:642-683). `ArchiveSetConfig` declares `compression: Option<String>` and `checksum_mode: Option<String>` (src/config.rs:256-257). The only guard is at use: `ArchiveSetCommands::Sync` validates every entry up front (src/cli/archive_set.rs:648-667). `config check` cannot see it either — its compression scan reads DB rows, not the config: `"SELECT name, compression FROM archive_sets WHERE compression IS NOT NULL"` (src/policy/compression_capability.rs:52).

**ADR basis:** ADR-0012: "Unknown config keys are errors everywhere; closed-set values are validated at load... One rule: `deny_unknown_fields` on every section, and every closed set (generation, compression, checksum mode, statuses on `--status` filters) rejected by name at the boundary." `compression` in `[defaults]` is load-validated; the identically-named field one section over is not, which is the split the ruling was written to end. Mitigating, and worth saying: `sync` does catch it before dar ever runs, so this is a diagnosis gap, not the raw-dar-failure gap the ADR describes.

**Failure scenario:** `[[archive_sets]] name = "cold"` with `compression = "zstd "` (trailing space) or `checksum_mode = "sha256sums"`. `tapectl config check` prints the config valid and exits 0 — the operator's one "is my config right?" command says yes. The next `tapectl archive-set sync` refuses the whole file all-or-nothing with `archive set "cold": invalid compression ...`, from a command whose job the operator believed `config check` had already covered.

**Suggested fix:** Extend `Config::closed_set_problems` to walk `self.archive_sets`, applying `validate_compression`/`validate_checksum_mode` (and `parse_size_to_bytes` on `slice_size` from `size_problems`), each message naming `archive_sets[i] ("name").<field>` and the file, exactly as the backend loop already does. `archive-set sync`'s guard then becomes a restatement rather than the only statement — and `config check`'s verdict stops disagreeing with `sync`'s.

**Verifier (adversarial):** Facts confirmed, severity not. `closed_set_problems` (config.rs:712-728) validates exactly `defaults.compression`, `defaults.checksum_mode`, `logging.level`, `logging.format`; `size_problems` (642-683) never touches `archive_sets[].slice_size`; `ArchiveSetConfig` (250-267) declares `compression`/`checksum_mode` as `Option<String>` with `deny_unknown_fields` but no value check. `config check` inherits exactly that: policy/lenient_config.rs:29-33 says '"Is this value legal?" is decided by Config::semantic_problems — the exact validators Config::load itself calls', and semantic_problems = size_problems + closed_set_problems (config.rs:734-741). So yes, `config check` prints valid on a config `archive-set sync` will refuse. But the harm stops there, and the finder concedes it ('a diagnosis gap, not the raw-dar-failure gap the ADR describes'): archive_set.rs:640-667 validates compression capability, checksum_mode AND slice_size for EVERY entry before any row is written ('Validated for EVERY entry up front, before any row is written'), and policy resolution reads archive_sets from the DB, never from config, so a bogus value can never reach `dar -z` or the CHECK constraint. Contradicting ADR-0012:151 'closed-set values are validated at load' with a misleading `config check` verdict is real and worth fixing; it is not 'recoverable but wrong' data behaviour. Low.

---

#### 22. [low] `capacity_override`'s doc comment still states the #168/#200 gap as current fact, contradicting the code and the test that pins it

**Location:** `src/config.rs:171`

**Evidence:** src/config.rs:171-180 reads: "**Known gap (issue #168):** only `LtoBackendConfig::planning_capacity_bytes` ... reads this field decimally; `volume init` (`volume::write::volume_init`, the path that actually decides and stores `volumes.capacity_bytes`) still parses it with the binary `staging::parse_size_to_bytes` ... Until that is fixed too, the SAME string means two different byte counts depending on which of those two paths reads it." That is no longer true: `volume_init` parses it with `media::parse_capacity_to_bytes` (src/volume/write.rs:294), under a comment saying "It parsed BINARY until issue #200", and a source-scan test asserts the binary parser must never come back there (src/volume/write.rs:7201-7230).

**ADR basis:** ADR-0012, "Cartridge capacities are decimal; data sizes are binary; the two are named apart" — the field doc is the place a reader learns which parser owns this value, and it currently teaches that two do. CLAUDE.md's own review standard for this codebase is that a derivation has one owner and the code says who it is.

**Failure scenario:** A maintainer reads the field doc while touching capacity math, believes the write path is still binary, and either "fixes" `volume_init` back to `staging::parse_size_to_bytes` (caught by the #200 test, but only after the change is written) or — the worse branch — adds a new capacity consumer matched to the documented binary behaviour, over-crediting an LTO-6 by ~10% (2 748 779 069 440 vs 2 500 000 000 000 bytes) in a path no test pins.

**Suggested fix:** Replace the "Known gap" paragraph with the settled state: all three callers (`planning_capacity_bytes`, `volume_init`, `Config::size_problems`) parse this decimally, per #168 and #200, and point at the guard test in `volume/write.rs`.

**Verifier (adversarial):** Could not refute; verified at both ends. config.rs:169-180 still reads: '**Known gap (issue #168):** only [LtoBackendConfig::planning_capacity_bytes] ... reads this field decimally; `volume init` ... still parses it with the binary `staging::parse_size_to_bytes` — out of #168's scope ... Until that is fixed too, the SAME string means two different byte counts depending on which of those two paths reads it.' The code says the opposite: write.rs:283-296 parses it as `crate::media::parse_capacity_to_bytes(v)?` under the comment 'It parsed BINARY until issue #200 — which made it the last site disagreeing with Config::validate_sizes and LtoBackendConfig::planning_capacity_bytes, both decimal since #168', and write.rs:7201-7230's source-scan test asserts `media::parse_capacity_to_bytes(` is present and `parse_size_to_bytes(` absent in that statement. So the field doc — the one place a reader learns which parser owns this value — teaches a gap that no longer exists, in the field whose whole ADR-0012 point is that capacities are decimal. Documentation-only, no runtime effect, and the #200 test catches the 'fix it back' branch: low, as rated.

---

#### 23. [low] Operator guide promises a later `catalog rebuild` will bind a legacy tape's cartridge; no code path ever can

**Location:** `docs/operator-guide.md:1182`

**Evidence:** The DR chapter (added in this diff) says: "A tape older than this field binds nothing and says so in the report; a later run that can read the drive's serial finishes the job." The code disagrees. `src/volume/rebuild.rs:1272-1297` `classify_media` returns `RebuildIdentity::Unknown` for all three legacy shapes (no `[media]` table, empty `cartridge_serial`, `cartridge_serial` present but no `cartridge_identity_source`), and `resolve_and_bind_cartridge` (`src/volume/rebuild.rs:1341-1352`) short-circuits on Unknown: `report.unbound_reason = Some(reason); return Ok(());` — before `resolved` is ever computed. `observed_serial` (the live MAM read) is threaded into the function at `src/volume/rebuild.rs:1334` but is consulted only inside `resolve_operator_identity` (`src/volume/rebuild.rs:1358`, used at 1561-1635); the Unknown arm never sees it. A legacy File 0 will never gain a `[media]` table, so re-running rebuild — with or without a readable drive serial — reaches the same early return every time. Nothing else writes `cartridge_volumes` for a rebuilt volume: `binding::mount_and_record` is the only writer and rebuild only reaches it past that early return, and the tape is sealed so `volume write`/`bind_late` can never run on it again (ADR-0003).

**ADR basis:** ADR-0010, "Init binds the cartridge" + ADR-0012 "A cartridge is known by the serial its chip reports" — a rebuilt volume that no cartridge claims is, in ADR-0010's own words, "a copy the catalog cannot place". `docs/design/volume-format-v2.md` §1.1 (added in this diff) states the rule the code follows: "Absent means unknown, and never means `\"mam\"`." The guide's sentence describes a recovery that the ADR-mandated rule structurally forbids.

**Failure scenario:** An heir rebuilds the catalog from a pre-ADR-0010 tape on a working LTO-6 drive. The report prints `warning: rebuilt unbound — this tape's File 0 carries no [media] table (written before ADR-0010)`. Following the guide, the operator re-runs the rebuild on the same drive (which reads the medium serial fine) expecting it to "finish the job". It prints the same warning and binds nothing, forever. The volume stays with no `cartridge_volumes` mount and no `location_id`, so `cartridge list`/`cartridge move`/`cartridge retire` cannot reach that physical tape and `audit`'s `location_presence` check flags the volume on every run, with no command the guide names to cure it.

**Suggested fix:** Replace the clause with what is true: a tape written before File 0 carried `[media]` can never be bound by `catalog rebuild`, on any drive — record the cartridge by hand (`cartridge register`, then place the volume with `volume move`) or accept it as unbound. If auto-binding a legacy tape from a live MAM read is wanted, that is a code change to the Unknown arm of `resolve_and_bind_cartridge`, not a re-run.

**Verifier (adversarial):** Refutation failed. I re-read `classify_media` (src/volume/rebuild.rs:1272-1297): all three legacy shapes return `RebuildIdentity::Unknown`, and `resolve_and_bind_cartridge` (:1341-1352) sets `report.unbound_reason` and `return Ok(())` BEFORE `resolved` is computed, so `observed_serial` (threaded in at :1334) is only ever consulted by `resolve_operator_identity` (:1566, :1585, :1606). `classify_media` is called at :289 with `media.as_ref()` only — the live `medium_serial` is used at :294-300 solely to REFUSE a disagreeing `Mam` claim, never to upgrade an `Unknown` one. File 0 is immutable on a sealed tape (ADR-0003), so re-running the rebuild re-enters the same arm forever. I also tried to kill the 'no cure' half by checking every other `cartridge_volumes` INSERT: src/cli/location.rs:713, src/cli/volume.rs:2160, src/db/mod.rs:632/849, src/cli/operations.rs:5058+ all sit inside `mod tests` blocks (first `mod tests {` at :582, :1844, :339, :3148 respectively), leaving `volume::binding::mount_and_record` as the only production writer. Two honest corrections: (a) the guide's sentence IS true for the `Operator` arm, where a live serial does supersede a typed barcode — it is only false for the `Unknown` arm the prose attaches it to, so this is a wrong-but-narrow doc error, not a phantom; (b) the same false promise is already in the code comment at src/volume/rebuild.rs:1345-1348 ('A later run that observes a real identity binds it'), which is almost certainly where the guide sentence came from. Severity corrected to low: the affected population is pre-2026-09-13 validation tapes (the mhvtl and HP LTO-6 tapes of 2026-09-10/11) — no tape this code writes can lack `[media]`, nothing has shipped to a production tape, and the runtime behaviour is a clear named warning, not silent loss.

---

#### 24. [low] README's "Volume Layout" table still documents the v1 10-file layout that ADR-0007 superseded

**Location:** `README.md:124`

**Evidence:** README.md:124-134 states "Each tape contains a self-describing 10-file layout:" and tabulates `3 | Planning header | Operator`, `4..N | Data slices`, `N+1 | Mini-index (position map) | No`, `N+2..K | Tenant envelopes (shuffled)`, `K+1,K+2 | Operator envelopes (dual)`. The actual format is Layout v2: `docs/design/volume-format-v2.md:21-52` fixes the order as File 0 ID thunk, File 1 system guide, File 2 RESTORE.sh, **File 3 FRONT INDEX (plaintext)**, then **envelopes**, then data slices, then a trailing **seal marker** — and the generator confirms it (`src/volume/layout.rs:137-157` writes `layout_version = 2` and `[layout] front_index = 3`). There is no planning header, no mini-index, and envelopes come before slices, not after. `docs/design-errata.md` records this supersession for the design doc ("§8.1–§8.8 + §2.6 … The v1 mid-tape mini-index and the '8/10-file' labels are gone") — but README was never brought along.

**ADR basis:** ADR-0007 (on-tape format v2: front index + seal marker) and `docs/design/volume-format-v2.md` §1, which the CLAUDE.md normative-set paragraph names as "authoritative for the bytes". `docs/design-errata.md` §2.6 / §8.1–§8.8 rows mark exactly this layout Superseded.

**Failure scenario:** An heir, or a new contributor, reads README first (it is the repo's front door and the only place the layout is tabulated outside the design notes), goes looking for the mini-index at position N+1 to enumerate the tape, and finds a data slice. Worse for an implementer: a change written against "planning header at File 3" would contradict `tests/on_tape_golden.rs`'s byte pins, which CLAUDE.md says are a CTO decision to move.

**Suggested fix:** Replace the table with the v2 zone order from `docs/design/volume-format-v2.md` §1 (ID thunk / system guide / RESTORE.sh / front index / envelopes / slices / seal marker), or delete it and link to that document as the single source.

**Verifier (adversarial):** Refutation failed. README.md:124-134 does read 'Each tape contains a self-describing 10-file layout' with '3 | Planning header', '4..N | Data slices', 'N+1 | Mini-index (position map)', 'N+2..K | Tenant envelopes', 'K+1,K+2 | Operator envelopes'. docs/design/volume-format-v2.md:21-52 fixes the real order as File 0 ID thunk / 1 system guide / 2 RESTORE.sh / 3 FRONT INDEX (plaintext) / then tenant+operator envelopes / then data slices / File M SEAL MARKER — no planning header, no mini-index, envelopes before slices. The README even contradicts itself: :173-174 in the module tour says 'volume/ Layout v2: build, session (typestate write), format (front index / seal / ID thunk parsers)'. I could not find any note in README marking the table historical, and docs/design-errata.md records the v1 labels as superseded. Low is the right severity — it is a stale doc table with no code consequence.

---

#### 25. [low] Operator guide's `volume write` announcement sample prints MB where the code prints MiB

**Location:** `docs/operator-guide.md:227`

**Evidence:** The sample output block added with the announcement feature reads `tv/breaking-bad/s01 v1: 3 slices, 42 MB` / `photos/2019 v2: 1 slices, 8 MB` / `total: 4 slices, 50 MB` (docs/operator-guide.md:227-230). The renderer uses the binary humaniser: `src/volume/write.rs:3117-3127` calls `crate::util::format_bytes_binary(bytes)`, and `src/util.rs:194-208` emits `TiB`/`GiB`/`MiB`/`KiB`. The unit test pins it: `src/volume/write.rs:5121-5127` asserts `"  solo v1: 1 slices, 1.0 MiB\n"`. The doc block landed in b6d1812; the MB→MiB relabel (d4d6b71, "fix(display): relabel binary-divided data sizes MB/GB/KB -> MiB/GiB/KiB") landed after it and did not update the sample.

**ADR basis:** ADR-0012, "Cartridge capacities are decimal; data sizes are binary; the two are named apart." A slice byte count is a data size, so it must be labelled MiB — the guide labels it MB, which is precisely the naming collision the ruling exists to prevent.

**Failure scenario:** An operator comparing the guide's sample with real output sees a different unit suffix and cannot tell whether the tool changed, whether the number means 42×10^6 or 42×2^20, or whether they are reading a capacity figure (decimal by ruling) or a data size (binary). The 5% delta is exactly the ambiguity ADR-0012 ruled out.

**Suggested fix:** Change the three lines to `42 MiB`, `8 MiB`, `50 MiB` (matching `format_bytes_binary`'s one-decimal form, e.g. `42.0 MiB`).

**Verifier (adversarial):** Refutation failed. docs/operator-guide.md:227-230 does print '42 MB' / '8 MB' / 'total: 4 slices, 50 MB', and `render_staged_selection` (src/volume/write.rs:3108-3127) formats both the per-unit and total figures with `crate::util::format_bytes_binary`, which emits 'MiB' with one decimal (src/util.rs:194-208). So the sample is doubly off: wrong suffix (MB vs MiB) and wrong shape ('42 MB' vs '42.0 MiB'). ADR-0012's named-apart ruling is real and `format_bytes_binary`'s own doc comment (src/util.rs:190-193) restates it. Cosmetic-but-incorrect: low.

---

#### 26. [low] Operator guide's `cartridge list` sample shows Loads = 0; the code prints `unknown` for both rows

**Location:** `docs/operator-guide.md:916`

**Evidence:** The "do not register first" warning box (added in this diff) shows:
`| E01001L8_1775794348 | LTO-8 | in_use    |          | 0     | L6-0009 |`
`| L6-0009             | LTO-8 | available |          | 0     |         |`
The hand-registered row cannot render `0`: `cartridge register`'s INSERT binds the column to NULL — `src/cli/cartridge.rs:380-383`, `(barcode, media_type, nominal_capacity, operator_serial, notes, total_load_count) VALUES (?1, ?2, ?3, ?4, ?5, NULL)` — and `CartridgeRow.loads` renders `Option<i64>` through `display_opt_i64`, `src/cli/cartridge.rs:216-219`: `v.map(|n| n.to_string()).unwrap_or_else(|| "unknown".to_string())`. Migration 015 (`src/db/migrations/015_cartridge_load_count_unknown.sql`) backfills every pre-existing `0` to NULL for the same reason. The auto-registered row takes whatever MAM reported (`src/volume/binding.rs:685-693`), which is a real count or NULL/`unknown` — `0` only if a chip genuinely reported zero loads.

**ADR basis:** Issue #184 / migration 015's ratified rule, stated in the migration header: "a stored 0 in `cartridges.total_load_count` means 'never observed', not 'zero loads' — distinguish them", and "every cartridge already on the shelf would keep displaying a confident '0' — which is the precise half of the defect the ruling calls out as misleading an operator deciding whether a cartridge is worn." The guide reprints exactly the confident `0` the ruling removed.

**Failure scenario:** An operator follows the cartridge-tracking chapter, sees `unknown` in the Loads column where the guide shows `0`, and reads it as a bug or as data loss — or, in the other direction, learns from the guide that `0` is what an unused cartridge shows and later misreads a genuine chip-reported `0` (a real fact about wear) as "never observed".

**Suggested fix:** Change both `0` cells in the sample to `unknown`.

**Verifier (adversarial):** Refutation failed. The sample at docs/operator-guide.md:914-917 shows '0' in the Loads column for both rows. `cartridge register` binds `total_load_count` to literal NULL (src/cli/cartridge.rs:379-383) with a comment saying exactly why ('"unknown" is the honest state, not a false "zero loads"'), and the list table renders that column through `display_opt_i64` (src/cli/cartridge.rs:203 `#[tabled(rename = "Loads", display_with = "display_opt_i64")]`, :216-219), whose unit test pins `display_opt_i64(&None) == "unknown"` (:1121-1123). So the hand-registered row can never render '0'. Only the auto-registered row could, and only if a chip genuinely reported zero. Low.

---

#### 27. [low] Operator guide says `audit` implements "all six compliance checks"; `cli::audit::CHECKS` has eleven

**Location:** `docs/operator-guide.md:654`

**Evidence:** docs/operator-guide.md:654 — "Run `tapectl audit`. It implements all six compliance checks, including copy count, location presence, and **verification age**…" and again at :703 — "one of `audit`'s six compliance checks". `src/cli/audit.rs`'s `CHECKS` table names eleven: `copy_count` (:185), `warehouse_copies` (:193), `location_presence` (:201), `verify_age` (:209), `escrow_coverage` (:217), `no_archive` (:225), `dirty` (:233), `encryption`, `compaction_candidate`, `escrow_kit`, `escrow_identity_mismatch` (:270). Six is the count in design §2.20 (`tapectl-design-v4_0.md:516-517`: "copy count, location presence, verification age, encryption compliance, dirty status, compaction candidates"), which `docs/design-errata.md` has since extended twice (#137 escrow findings, #138 per-check scope, C4 checks-as-a-table) without anyone updating the number.

**ADR basis:** `docs/design-errata.md` §2.20 rows (#137, #138, architecture-review C4) supersede design §2.20's check list; ADR-0005/ADR-0009 add the escrow and heir-kit checks. The word "all" asserts completeness against a list the errata has already replaced.

**Failure scenario:** An operator auditing their own coverage counts six findings categories and concludes the escrow-coverage, escrow-kit-staleness, escrow-identity-mismatch, warehouse-copies and no-archive checks are things they must chase by hand — the exact set ADR-0005's disaster-recovery story depends on `audit` surfacing.

**Suggested fix:** Drop the number ("It implements every compliance check") or cite `cli::audit::CHECKS` as the list, so the sentence cannot go stale again the next time a check is added.

**Verifier (adversarial):** Refutation failed. docs/operator-guide.md:654 says 'It implements all six compliance checks' and :703 repeats 'one of `audit`'s six compliance checks'. `CHECKS` (src/cli/audit.rs:183-277) is a single table of eleven entries — copy_count, warehouse_copies, location_presence, verify_age, escrow_coverage, no_archive, dirty, encryption, compaction_candidate, escrow_kit, escrow_identity_mismatch — and its own doc comment says 'the table below IS the scope'. I looked for a narrower reading (informational vs compliance rows); there is no such split in the table, every entry is run by `collect_findings` and contributes findings. Doc-only, no behavioural consequence: low.

---

#### 28. [low] README command reference omits `volume list`/`volume info`, added in this same diff

**Location:** `README.md:97`

**Evidence:** README.md:97-99 lists `tapectl volume  init, write, resume, abort, verify, identify, move, retire, read-slices, plan, deposit, compact-read, compact-write, compact-finish, compact`. `VolumeCommands` also has `List` (`src/cli/volume.rs:286`) and `Info` (`src/cli/volume.rs:302`), added by f14aea0/686a089 (#195) inside this diff, with new man pages `docs/man/tapectl-volume-list.1` and `docs/man/tapectl-volume-info.1` generated for them. The adjacent `tapectl cartridge` line WAS updated in this diff (edit, relabel, move, retire, unretire), so the omission is an oversight rather than a policy of brevity. The same section's `tapectl report` line (README.md:102-104) also omits `supersedable`, which `src/cli/report.rs:70` defines and `docs/man/tapectl-report-supersedable.1` documents.

**ADR basis:** Not an ADR rule — the README section is titled "Command Reference" and enumerates every subcommand of every other noun, so an incomplete row is a factual error about the current CLI surface (verified against the clap `Subcommand` derives in `src/cli/volume.rs` and `src/cli/report.rs`).

**Failure scenario:** A user (or an agent) looking for "how do I see what volumes I have" reads the Command Reference, finds no `volume list`, and concludes the catalog-only inventory command #195 was built to provide does not exist — falling back to `catalog stats` or raw SQL.

**Suggested fix:** Add `list, info` to the `tapectl volume` row and `supersedable` to the `tapectl report` row.

**Verifier (adversarial):** Refutation survives, but it is the weakest class here and the caller should weigh it accordingly. README.md:97-99 lists volume subcommands without `list` or `info`, while `VolumeCommands` defines `List {` at src/cli/volume.rs:286 and `Info {` at :302; `report supersedable` is likewise defined (src/cli/report.rs:70, :103, :123) and absent from README:102-104. The adjacent cartridge line was updated in this diff (register, edit, relabel, list, info, move, retire, unretire, mark-erased), so brevity is not the policy. Against it: there is no ADR basis, the finding says so itself, and an incomplete enumeration is closer to missing documentation than to a defect. Low at most.

---

#### 29. [low] Migration 013's header comment claims dar takes `--acl` and that `preserve_acls` is passed through to dar; neither is true

**Location:** `src/db/migrations/013_drop_manifest_entry_flags.sql:17`

**Evidence:** Lines 17-19 of the migration read: "dar owns xattr and ACL handling (`--alter=atime`, `--acl`, `--ea` — `defaults.preserve_xattrs` / `.preserve_acls` are passed THROUGH to dar), and dar records what it preserved in its own archive catalog." `docs/design-errata.md`'s dar-invocation row states the measured fact: "`--acl` is not a dar option at all: `dar --acl` → `dar: unrecognized option '--acl'`", and its `preserve_acls` row records the CTO decision that "**dar exposes no independent ACL switch**" and that "The dead plumbing between `policy::resolve` and `DarCreateParams` was removed as part of #50". The code agrees: `DarCreateParams` carries `preserve_xattrs` only (`src/dar/create.rs:17`, consumed at `:55`); there is no `preserve_acls` field and no `--acl`/`--ea` argument anywhere in `src/dar/`.

**ADR basis:** `docs/design-errata.md`, rows "§6 — dar invocation example" (Rejected (mechanism), verified against installed dar 2.7.13) and "§7 / §4 schema — `preserve_acls` config knob and column" (Clarified — no-op made visible). The migration comment restates, as current behaviour, the two claims those rows exist to retract.

**Failure scenario:** A contributor reading migration 013 to understand why the manifest flags were dropped takes the parenthetical as the current dar contract, adds `--acl` to `src/dar/create.rs` to honour `preserve_acls`, and every `stage create` fails with `dar: unrecognized option '--acl'` — the same wrong turn the errata row was written to prevent.

**Suggested fix:** Correct the parenthetical to the measured facts: on Linux dar carries ACLs as Extended Attributes whenever EA support is compiled in, tapectl passes no EA-exclusion mask, `preserve_xattrs` is the only knob reaching `DarCreateParams`, and `preserve_acls` is a documented no-op subsumed by it (see `docs/design-errata.md`).

**Verifier (adversarial):** Refutation failed. src/db/migrations/013_drop_manifest_entry_flags.sql:17-19 reads verbatim: 'dar owns xattr and ACL handling (`--alter=atime`, `--acl`, `--ea` — `defaults.preserve_xattrs` / `.preserve_acls` are passed THROUGH to dar)'. docs/design-errata.md:53 records, verified against installed dar 2.7.13, that '`--acl` is not a dar option at all: `dar --acl` → `dar: unrecognized option \'--acl\''', and :54 ratifies that 'dar exposes no independent ACL switch' and that 'The dead plumbing between `policy::resolve` and `DarCreateParams` was removed as part of #50'. The code agrees: `DarCreateParams` has `preserve_xattrs` (src/dar/create.rs:17) and `preserve_fsa` only, no `preserve_acls`; `src/dar/create.rs:55-64` passes `-am` and carries a comment explicitly saying `-am` is unrelated to ACLs, and no `--acl`/`--ea` appears anywhere in src/dar/. `src/policy/subsumed.rs:4-8` exists precisely to surface `preserve_acls` as a no-op. So both clauses of the parenthetical are false and restate what the errata exists to retract. It is a comment in a landed migration with no runtime effect: low.

---

## Per-dimension

| Dimension | Confirmed | Rejected |
|---|---|---|
| consent-tiers | 2 | 3 |
| cartridge-identity | 0 | 6 |
| write-path-ordering | 2 | 0 |
| capacity-units | 2 | 5 |
| restore-dr | 2 | 2 |
| operator-text | 4 | 5 |
| tests-pinning-defects | 1 | 4 |
| derivation-discipline | 2 | 3 |
| config-strictness | 7 | 2 |
| docs-code-drift | 7 | 3 |

## Rejected findings

Recorded so the same ground is not re-ploughed, and so a wrong rejection can be challenged.

- **`cartridge mark-erased` reaches zero coverage with no Tier-3 floor — the documented alternative to the one command that has it** — The code facts are right (operations.rs:1413-1446 has no `refuse_last_eligible_copy` call and unconditionally writes `UPDATE volumes SET status = 'erased'`), but the ADR basis and the failure chain both fail.

(a) SCOPE. ADR-0012's floor is scoped by its own words: "Tier 3 fires when the thing being retired is currently an *eligible* copy ... it is defined by what the act removes", in a paragraph 
- **`unit mark-tape-only --force` proceeds at zero eligible copies when every copy is `erased` or `missing`** — This is the closest call of the five, and the finding's strongest point is genuine: ADR-0008's rationale sentence does read "Marking a unit tape-only when it is on no tape is not a riskier version of that — it is a contradiction in terms," and a unit whose every volume is `erased` is on no tape. But the ratified RULE the code must implement is the enumeration, not the rationale, and the enumeratio
- **The global `--yes` never reaches `unit mark-tape-only`, though its regenerated man page advertises that it does** — The mechanical claim is accurate — `cli::unit::run` takes no `yes` parameter (src/cli/unit.rs:137-143) and dispatches `unit_mark_tape_only(conn, config, name, *force, json_output)` (src/cli/unit.rs:366), so `Cli::yes` (src/cli/mod.rs:40-43, `global = true`) never reaches it — but it is not a defect.

`-y|--yes` appearing in docs/man/tapectl-unit-mark-tape-only.1 (lines 7 and 24) is clap's `global 
- **The write path commits a learnt chip serial onto a cartridge row before File 0 has been read — a wrong tape permanently stamps the wrong identity, with no command to undo it** — no verdict returned - treated as unverified
- **`bind_late` can displace live volumes on the strength of an operator-TYPED serial, the exact case `refuse_unwitnessed_displacement` exists to refuse** — no verdict returned - treated as unverified
- **`catalog rebuild` never consults `operator_serial`, so a pre-registered cartridge is registered a second time under its own chip serial** — no verdict returned - treated as unverified
- **`refuse_rebind` and both `already_mounted` guards look only at OPEN mounts, so a closed binding surfaces as a raw UNIQUE constraint failure instead of the named refusal the code promises** — no verdict returned - treated as unverified
- **`volume write` commits the loaded medium's MAM capacity onto the volume row before the wrong-cartridge refusal runs** — no verdict returned - treated as unverified
- **`cartridge edit --serial` omits the chip-confirmed-collision refusal that `cartridge register --serial` applies to the identical value** — no verdict returned - treated as unverified
- **Binary-divided data sizes still logged under decimal unit names in src/staging/ (the one directory #204's relabel pass fenced out)** — no verdict returned - treated as unverified
- **`backend add --capacity-override` validates a DECIMAL capacity with the BINARY parser** — Refuted. The call site is not an oversight — it is explicitly reasoned about and pinned. src/media.rs:774-779, the doc on `capacity_and_size_parsers_agree_on_which_strings_are_valid`, names this exact call site: "a validator that only checks parseability (e.g. `backend add`'s pre-check of `capacity_override` before it is written to config) is unaffected by which of the two backs it," and the test 
- **util.rs's formatter docs assert `mam_capacity_bytes` is decimal, but it is computed as MiB x 1 048 576 and ADR-0012 leaves the unit explicitly unsettled** — no verdict returned - treated as unverified
- **`--slice-size` help neither names its unit family nor uses a valid example — it advertises the whole-tape default the design ratified against** — no verdict returned - treated as unverified
- **`config check` renders staging free space decimally while `stage create` renders the same number binarily, through a third private humaniser** — no verdict returned - treated as unverified
- **`catalog rebuild` auto-registers a cartridge with File 0's RESOLVED capacity, laundering a drive's `capacity_override` into the cartridge row — the exact defect #183 fixed in binding.rs** — no verdict returned - treated as unverified
- **The `mam`-identity mismatch refusal declares "there is no --force for this" and names the bypass in the same sentence — and following it binds the wrong cartridge** — The mechanics are as described (rebuild.rs:299-309; binding.rs:1238-1246 `loaded_medium_serial` returns `None` when `config::resolve_device` finds no backend, so the `if let Some(observed)` never fires and `corroborate_volume` at rebuild.rs:341 also sees an absence) — but it is not a defect.

(a) The "bypass" is a RATIFIED design stance, not a hole. ADR-0010 states read paths stay usable without a
- **`volume write`/`resume` refusal prescribes `volume init <new-label>` on a cartridge it has just said is sealed — and `volume init` refuses a sealed tape, with no --force** — no verdict returned - treated as unverified
- **`catalog rebuild`'s barcode-collision refusal tells the operator to run `volume init` on the tape being recovered — refused on a sealed tape, destructive on an unsealed one** — no verdict returned - treated as unverified
- **`cartridge retire` prints its impact block after the commit, so it reports the pre-retirement status as "Current" and a completed loss in the future tense** — no verdict returned - treated as unverified
- **An inconsistent-binding refusal during `volume write` suggests `volume init` a new label on the same cartridge, which the File-0 identity check refuses without `--force`** — REFUTED — this is the design, not a defect.

The mechanics are as described: resolve_cartridge_identity (write.rs:685-692) ends "Restore the database from a `db backup`, or `volume init` a new label on this cartridge.", and `volume init L6-0005` on a tape whose File 0 names L6-0004 hits ContactOutcome::IdentityMismatch (session.rs:370-402) and is refused at write.rs:1893-1898 without --force.

Why
- **`collection run`'s batch-index error points at `collection plan`, whose batch numbering the same command's own `--batch` help says may differ** — REFUTED — a missing `run --dry-run`, not a text defect.

The quotations are accurate: collection.rs:353-359 errors "...({} batch(es) currently pending — run `collection plan` to see them)" and the `--batch` help at collection.rs:76-82 warns that plan's ordering matches only when plan was run with the matching `--generation`. The two budgets genuinely differ (plan uses planning_capacity_bytes / --g
- **`edit_generation_and_serial_together_gate_only_the_serial_half` never exercises either gate, hiding a Tier-1 commit that lands before the Tier-2 refusal** — The mechanical facts check out — `run`'s signature is `(conn, config, command, json_output, yes, dry_run)` (src/cli/cartridge.rs:279-286), so the test's `false, true, false` is `yes = true`, and `confirm_with` returns `Ok(())` immediately on `assume_yes` (src/cli/consent.rs:~77). The ordering is also as described: the `Edit` arm calls `cartridge_edit` (which commits its own `conn.unchecked_transac
- **`the_already_sealed_recipe_is_runnable_on_a_rebuilt_tape` proves runnability by passing `force = true`, which the printed recipe does not contain** — The finding misquotes the message it is judging. The `AlreadySealed` refusal `volume_init` actually reaches (write.rs:322 -> `check_fresh_write_contact` -> `decide_fresh_write_contact`, src/volume/write.rs:1877-1883) prints: 'If this cartridge should be reused: retire its current volume, bulk-erase the physical tape, then run `tapectl cartridge mark-erased` before writing to it again.' That is a T
- **`auto_register_records_the_generation_tables_capacity_not_a_drive_override`'s `assert_ne!` became vacuous when #202 removed the parameter it was guarding** — The `assert_ne!` is redundant, but the hazard it names is fully guarded two lines above it by `assert_eq!(cap, Generation::Lto8.native_capacity_bytes() as i64, "the new cartridge row must record the generation table's native capacity ... never the drive's resolved (possibly overridden) figure")` (src/volume/binding.rs:2607-2614). If auto-registration ever copied a resolved/overridden figure (2 400
- **`run_refuses_an_unknown_destination_label_before_staging`'s snapshots before/after assertion is vacuous, and its doc names it as the proof** — The finding reads only the doc comment and skips the inline comment inside the same test body, which says the opposite and is correct: 'if the budget were ever resolved AFTER `batches_for_budget` ran (i.e. the bug this issue fixes, reintroduced), this oversized unit would surface as the "exceed the per-tape budget" error instead of `VolumeNotFound`, and the `matches!` below would catch that regres
- **The byte-humaniser consolidation asserts sole ownership but leaves a third decimal humaniser in policy::depth_check, rendering the same capacity two ways** — no verdict returned - treated as unverified
- **`backend add` validates `capacity_override` with the binary size parser while config.rs validates and consumes it with the decimal capacity parser; a test pins the false claim that this is safe** — no verdict returned - treated as unverified
- **The usable-tape-budget formula is hand-copied at four sites and `volume plan` is the one that omits the ENOSPC buffer** — Refuted. The code facts are accurate — `src/cli/volume.rs:720-722` computes `usable = tape_cap * factor` with no `enospc_buffer` subtraction, while write.rs:879/886, plan.rs:116-118 and plan.rs:210-212 all subtract — but this is not a defect under the task's definition. (1) No ADR or documented rule requires `volume plan` to reproduce the write gate. The finding concedes this ("Not an ADR clause")
- **`backends.lto[].usable_capacity_factor` is accepted unvalidated, and it is the multiplier on the sole pre-flight capacity defence** — The facts check out — `#[serde(default = "default_usable_capacity_factor")] pub usable_capacity_factor: f64` (config.rs:183-184) is absent from `size_problems` (config.rs:642-683, which does validate `generation`, `capacity_override`, `enospc_buffer`) and from `closed_set_problems` (712-728), and the consumers multiply raw (write.rs:319, 879, 1243, 2084; collection/plan.rs:116, 210). But it is not
- **`backend add --capacity-override` is validated by the binary size parser, the one value ADR-0012 gives to the decimal parser** — Refuted. I diffed the two parsers: media::parse_capacity_to_bytes (media.rs:318-350) and staging::parse_size_to_bytes (staging/mod.rs:1274-1306) are character-for-character the same grammar — same split on the first alphabetic char, same `f64` parse, same NaN/negative refusal, same `{'', K, KB, M, MB, G, GB, T, TB}` suffix set — differing only in multiplier (10^n vs 2^n) and in the word 'capacity'
- **CLAUDE.md describes `cartridge retire` as Tier-2-gated only, omitting the absolute Tier-3 floor this diff installed** — Refuted. CLAUDE.md:88-90 is a dated narrative bullet summarising what ADR-0011 landed on 2026-09-13; the Tier-3 restoration is ADR-0012, dated 2026-09-14, and it is stated in the very next bullet of the same section (CLAUDE.md:104-105: 'the retire family's zero floor is absolute (ADR-0008 Tier 3, the code had it inverted)'). The ADR itself is not misstated anywhere: docs/adr/0011-*.md:40-41 alread
- **CLAUDE.md says config.toml holds "locations … policy"; no such sections exist and `deny_unknown_fields` now rejects them** — Refuted. CLAUDE.md:261 reads '(dar path, backends, locations, defaults, exclusions, policy)' — a topical list of what the file governs, not a list of TOML tables. Its own first item, 'dar path', is not a table either (the table is `[dar]`, the key is `binary`), which forecloses the literal-section reading the failure scenario depends on. And the topics do exist in config: exclusions at `defaults.g
- **CLAUDE.md and `src/store.rs` still list `WarehouseStore`/`ExportStore` as peers pending under #72/#73, after #72 was rescoped away and #73 landed without one** — Refuted by the very errata row the finding cites. docs/design-errata.md:41 opens: 'ADR-0006 keeps `WarehouseStore` as a first-class peer of `TapeStore`; this row records only how far tapectl builds it now, and does not amend the ADR.' It then says native S3 'remains open on the same seam if the procedure proves insufficient — nothing here forecloses it.' So src/store.rs:5-6 ('`WarehouseStore`/`Exp

## Completeness critic — what the ten dimensions did not reach

## Completeness gaps — subsystems and defect classes the ten dimensions did not reach

Ranked by risk to the first production write. Where I could only establish "unread + plausible failure shape" I say so.

---

### 1. `scripts/` — 669 changed lines, read by zero dimensions, and it is the *only* end-to-end gate for the first write
`scripts/lifecycle-suite.sh` +402 (15 commits), `scripts/first-run.sh` +218, `scripts/mhvtl-verify-gate.sh` +49. Only `scripts/lib/drive-generation.sh` was read (derivation-discipline, and only to rule it out as a second capacity table).

What makes this the top gap is *what those commits are*: a30e284 "two scenarios asserted states they never created (#203)", df09d2a "make the compaction scenario test what it says (#198)", 15f2c84 "retire-and-reuse erases the tape it says it erases (#198)", eb280e8 "really erase a freshly loaded slot tape (#194)". The harness was found four separate times in this diff to assert things it never created — and **nobody reviewed the fixes**. MEMORY's own standing rule (`feedback-verify-before-landing-harness-changes`) is "the tape is the only test a scripts/ change has"; none of the ten ran anything.

Two positive results I did establish read-only, so a follow-up need not redo them: no stale `--media` survives anywhere in `scripts/` (821aef8's rename to `--generation` is complete), and `EXPECTED_FAIL=()` at `scripts/mhvtl-verify-gate.sh:124` is genuinely empty, matching CLAUDE.md's claim.

Still unchecked: whether the refusal-asserting scenarios actually assert non-zero exit rather than just running the command; whether `--yes` at `scripts/lifecycle-suite.sh:461, 1854, 1954, 2030` sits anywhere a Tier-3 floor is what should refuse (the comment at :1851 claims "`--yes` does not reach Tier 3" — asserted in prose, not pinned by a scenario).

---

### 2. `collection run`'s destination budget validates label *existence*, not write-targetness — medium
`src/collection/plan.rs:188-215` (`destination_budget`) resolves each `--label` with:

```
"SELECT capacity_bytes FROM volumes WHERE label = ?1"
```

`.ok_or_else(|| TapectlError::VolumeNotFound(...))` — no status predicate. `policy::coverage::is_write_target` is the declared sole owner of that question and is never called. Its own doc comment (`src/collection/plan.rs:222-228`, repeated at `src/cli/collection.rs:325-333`) states the purpose of resolving the budget first: so a bad `--label` "fails immediately, long before `batch::execute_batch` stages a single unit (hours of dar + age, a tape's worth of staging disk)."

A `sealed`, `retired`, `erased` or `quarantined` label passes. `cmd_run` then prints `budget X from volume "VOL-SEALED"` (`src/cli/collection.rs:336-350`), `execute_batch` stages every unit in the batch (`src/collection/batch.rs:98-140`), and only then does `volume write` refuse with `VolumeNotWriteTarget`. Naming yesterday's finished tape is a far more likely operator error than mistyping a label, and it is the one the early check does not catch. The budget printed is also derived from a volume that will never be written.

No dimension owned this: capacity-units and derivation-discipline touched `plan.rs` only for the arithmetic and the `planning_capacity_bytes` ban (both correct); neither asked what the row's status was.

---

### 3. Version minting vs. per-version copy counting — flagged by tests-pinning as out of dimension, routed to nobody — potentially high
`src/unit/content_match.rs:154-166`:

```
"SELECT id, version, status FROM snapshots WHERE unit_id = ?1
 ORDER BY version DESC LIMIT 1"
```

No status filter. `src/staging/mod.rs:118-140` short-circuits (mints nothing) only when that latest row is `current | created | staged`; for `superseded | reclaimable | purged | failed` it deliberately falls through and mints a fresh version — the comment calls a content match against a dead row "coincidence, not identity."

The consequence nobody traced: a unit whose content is reverted to an *older* version's bytes, or whose latest row is `reclaimable`, mints a NEW version byte-identical to one already on tape. ADR-0012's "two versions of a unit never hold identical content" is exactly the premise `policy::coverage::copy_count_expr`'s per-version MIN rests on. The new version reports zero copies while its precise bytes are sealed on a cartridge — `audit` then reports a shortfall for content that is fully covered, and `snapshot mark-reclaimable`/`unit mark-tape-only` reason off that number. This is the "silently mis-states coverage" class.

It fell between three dimensions: consent-tiers owns the consumer, tests-pinning owns the test and explicitly routed it out ("OUT OF MY DIMENSION but observed", item (a)), derivation-discipline owns `copy_count_expr` but not its premise. Nobody owns the producer.

---

### 4. The whole Collection layer went unowned — 878 changed lines, and `tests/collection.rs` is untouched by the entire diff
`src/collection/fingerprint.rs` (-296/+63 restructure, extracting the comparison into `unit::content_match`), `src/collection/plan.rs` (+387, the whole `plan_for_run`/`destination_budget`/`batches_for_budget` seam), `src/collection/batch.rs` (+53, the unminted-status enumeration at 4fc5f44).

`tests/collection.rs` exists and appears nowhere in `git diff --stat 5d4cc43..HEAD -- tests/`. The new seam is covered only by in-module tests written in the same commits that introduced it — the shape tests-pinning was commissioned to distrust, but it read `plan.rs`/`batch.rs` only "as diffs" and did not reach `tests/collection.rs` or the extraction's behavioural equivalence. This is the path a bulk first write plausibly uses.

---

### 5. `src/cli/location.rs` (+361) — the ADR-0011 mover, named "unreviewed" by cartridge-identity's coverage note and picked up by nobody
Cartridge-identity's note item (1) explicitly lists this. Operator-text swept its strings only; derivation-discipline examined `location.rs:79` and deliberately declined to file it ("an absence rather than a disagreeing derivation … I did not verify that `location_id` survives retire/mark-erased").

I read `move_together` (`src/cli/location.rs:326-470`) and it is sound on the two things ADR-0011 promises: the warehouse refusal covers both columns (:349-386) and the cartridge+volumes update is one `unchecked_transaction` (:390). 5ba5a6a's name-resolution fix is correct. What remains unreviewed: the **selection** half — which volumes `cartridge move` and `volume move` hand to the mover; `move_cartridge`'s behaviour on a `retired_permanent` cartridge (ADR-0011 says it is still physically on a shelf, so moving it should be allowed — unverified); `resolve_location_name`; and `LocationCommands::Rename`/`Delete` against locations that still hold cartridges.

---

### 6. `src/main.rs` (+185) added a third, silently-lenient config surface — audited as "dispatch" only
config-strictness read `main.rs` for the `config check` special case and did a thorough job on `lenient_config.rs`, concluding leniency is contained. It did not name `peek_logging_config` + `resolve_paths` (`src/main.rs:85-130+`), which now run **before** `Config::load` on every single command and read `[logging]` out of config.toml with documented silent fallback on "a missing home, unreadable file, unparseable TOML, absent `[logging]` table, or a `[logging]` table that itself fails to deserialize."

Low risk in itself (the authoritative `Config::load` still refuses afterwards, and `validate_closed_sets` still owns the log-format set). The un-reviewed part is that `resolve_paths` — the `--config`-implies-`--home` derivation — was **moved out of `run()` and is now executed twice**, once before any subscriber exists and once authoritatively. Home resolution governs which database and which config every command touches, and no dimension checked the two calls agree in every branch.

---

### 7. No real-tape coverage for the two highest-risk new behaviours
`tests/mhvtl_e2e.rs` +158 was not read (tests-pinning, budget). The two tests it adds are `mhvtl_verify_by_id_device_records_drive_health` and `mhvtl_verify_with_no_backend_says_health_not_collected` — drive-health and by-id paths.

So across the entire 220-commit diff, nothing added an on-media test for:
- **cartridge displacement / rebinding** — the ADR-0010/0012 machinery that is the largest single behavioural change in the diff (`src/volume/binding.rs` +2996).
- **`collection run` with more than one `--label`.** `scripts/lifecycle-suite.sh:2626` runs `collection run --collection media --batch 0 --label VOL-COL1 --device "$TAPE_DEV"` — exactly one label. `destination_budget`'s minimum-across-destinations rule (gap 2) and `execute_batch`'s write-N-copies loop have therefore *never run on media*, and the mixed-generation case that motivated issue #175 (an LTO-5 cartridge budgeted as LTO-6) is untested end to end.

---

### 8. Cross-dimension: the one data-loss-class finding in the whole audit is precisely what the gate cannot catch
write-path-ordering confirmed `check_tape_contact`'s identity-**matches** branch never consults the tape's own seal pointer, so a sealed tape can be overwritten without `--force` (`src/volume/session.rs`). The lifecycle suite's ADR-0003 scenarios (`scripts/lifecycle-suite.sh:1961-1995`) exercise `volume init VOL-X` over a sealed cartridge — the identity-**mismatch** branch, with and without `--force`. There is no scenario that writes to an already-sealed tape whose catalog row *matches*. Ranked medium by its own dimension; it is an ADR-0003 violation with a data-loss outcome and zero harness coverage, which is a combination worth re-ranking before the first write.

---

### 9. Smaller, genuinely unread
- **Migration 012's rebuild.** Narrower than cartridge-identity feared — `src/db/mod.rs:766-822` has a real `open_memory_at_012` regression test, and `'offsite'` survives only as a named refusal (`src/cli/cartridge.rs:267`) plus test fixtures. But 012 is the one *rebuild* in the range and nobody verified its recreated indexes or that it ran a `.foreign_key_check()` (014/015/016 correctly skip one as plain ADD COLUMNs, per `src/db/mod.rs:127+`).
- **`src/cli/archive_set.rs` (+112), `src/cli/snapshot.rs` (+96), `src/cli/report.rs` (+188), `src/cli/catalog.rs` (+389)** were each touched by two or three dimensions for one narrow question (a string, a predicate, a humaniser) and by none as a subsystem.
- **Ruled clean, no follow-up needed:** `src/tape/ioctl.rs` +27 is a pure `pub(crate)` visibility change unifying one `MTIOCGET`/`MtGet` declaration (31584d7) — the "two `#[repr(C)]` layouts casting a raw pointer" hazard it names is real and the change closes it correctly. `src/tape/mam.rs` +24 is comments only. `cartridge_retire`'s multi-row mutation (`src/cli/operations.rs:1165-1222`) inlines the volume retirement inside its own transaction rather than nesting `volume_retire`'s — the `unchecked_transaction` flattening hazard does not arise.