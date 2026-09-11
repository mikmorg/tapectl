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
| 3 | A catalog rebuilt from tape can never show escrow coverage: `stage_sets.key_fingerprints` is the sole evidence and is on neither the tape nor the on-tape `catalog.db`, so `escrow::gap` fail-closes to `NO` on every rebuilt set, permanently and un-clearably. Accept the noise, add a third state, or start recording the recipient list in the envelope? | 2026-09-11 | [#137](https://github.com/mikmorg/tapectl/issues/137) |
