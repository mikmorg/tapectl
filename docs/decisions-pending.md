# Decisions pending the CTO

Questions an unattended run reached but would not answer for itself — the
**defer** tier of `.claude/skills/unattended-run/SKILL.md`: bytes frozen onto
tape, DB schema, CLI surface. Each one also has an issue labelled `needs:cto`,
so either surface can be answered.

Answered questions move to `docs/design-errata.md` (or an ADR when they set
policy) and are struck from this file.

| # | Question | Raised | Issue |
|---|---|---|---|
| ~~1~~ | **ANSWERED 2026-09-11** — deleted (it was a stale copy of the layout constant, and nothing reads it). Recorded in `docs/design-errata.md`; landed `64d3f91`. ~~Envelope manifest writes `layout_version = 1` on a v2 tape. Is that the envelope schema's own version (rename it) or a stale copy of the tape-layout constant (bump it, changing bytes on every future tape)?~~ | 2026-09-11 | [#134](https://github.com/mikmorg/tapectl/issues/134) |
| ~~2~~ | **ANSWERED 2026-09-11** — a new command, `catalog rebuild --from-volume`, not a change to `import`; landed `edef129`. ~~When the DB is gone and there is no backup, what rebuilds the catalog? `import` registers a bare `volumes` row and nothing else. Any rebuild-from-tape must decrypt an envelope, since the plaintext zones carry no unit names by invariant — so it is inherently a keyed operation.~~ | 2026-09-11 | [#136](https://github.com/mikmorg/tapectl/issues/136) |
| ~~3~~ | **ANSWERED 2026-09-11** (grilling Q1–Q13): third state + receipt on the tape + attestation; landed `d5c9638`/`4292cf9`/`3249ac6`, closed. ~~A catalog rebuilt from tape can never show escrow coverage: `stage_sets.key_fingerprints` is the sole evidence and is on neither the tape nor the on-tape `catalog.db`, so `escrow::gap` fail-closes to `NO` on every rebuilt set, permanently and un-clearably. Accept the noise, add a third state, or start recording the recipient list in the envelope?~~ | 2026-09-11 | [#137](https://github.com/mikmorg/tapectl/issues/137) |
| 4 | Is the on-tape work of the 2026-11-12 review's queue authorized (RESTORE.sh, File 1, RECOVERY.md, `paths.tsv`, perhaps File 0), and where does the authorization end? | 2026-11-13 | [#554](https://github.com/mikmorg/tapectl/issues/554) |
| 5 | The fix for the catalogue's slice numbers: make the catalogue right (isolate it from the last slice), or stop using slice numbers (extract one path from the whole stream)? | 2026-11-13 | [#518](https://github.com/mikmorg/tapectl/issues/518) |
| 6 | What is a production run: a new catalog, keys and units, or home2's as they are; is the escrow identity kept; what becomes of the two test cartridges, and when? | 2026-11-13 | [#554](https://github.com/mikmorg/tapectl/issues/554) |
| 7 | Which features production uses at its start (spanning, `copies_at`, `volume rewrite`, compaction), which decides the reads that come before it; and how two places are held for active units. | 2026-11-13 | [#551](https://github.com/mikmorg/tapectl/issues/551), [#550](https://github.com/mikmorg/tapectl/issues/550) |
| 8 | The channel a failed timer reports on, and the default exclusion list. | 2026-11-13 | [#535](https://github.com/mikmorg/tapectl/issues/535), [#542](https://github.com/mikmorg/tapectl/issues/542) |
| 9 | The small new behaviours: an `audit` check for an unregistered collection folder, an "unstaged" pending state, a clock for the heir drill. | 2026-11-13 | [#534](https://github.com/mikmorg/tapectl/issues/534), [#529](https://github.com/mikmorg/tapectl/issues/529), [#548](https://github.com/mikmorg/tapectl/issues/548) |
| 10 | The three rulings adjusted while building (1.7.0's X12, item 159 as built, items 149 against 156): ratified or changed? | 2026-11-13 | [#545](https://github.com/mikmorg/tapectl/issues/545) |
