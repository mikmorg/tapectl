# Decisions pending the CTO

Questions an unattended run reached but would not answer for itself — the
**defer** tier of `.claude/skills/unattended-run/SKILL.md`: bytes frozen onto
tape, DB schema, CLI surface. Each one also has an issue labelled `needs:cto`,
so either surface can be answered.

Answered questions move to `docs/design-errata.md` (or an ADR when they set
policy) and are struck from this file.

| # | Question | Raised | Issue |
|---|---|---|---|
| 1 | Envelope manifest writes `layout_version = 1` on a v2 tape. Is that the envelope schema's own version (rename it) or a stale copy of the tape-layout constant (bump it, changing bytes on every future tape)? | 2026-09-11 | [#134](https://github.com/mikmorg/tapectl/issues/134) |
