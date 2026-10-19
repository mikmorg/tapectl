#!/usr/bin/env bash
# lto6-fill.sh -- the end-of-tape fill, now a thin wrapper of scripts/lto-profile.sh --fill-only
# (issue #479, ADR-0012 2026-10-17 item 96).
#
# The fill (issue #420) moved into the generation-agnostic profiling script, which also runs the
# measurements of the old scripts/lto6-measure.sh and builds a report and a [[backends.lto]] block
# from them. This wrapper keeps the old command line working: every option it took
# (--device, --i-will-lose-the-cartridge, --out, --chunk-gib, --stop-at-warning, --no-hard-end,
# --to-hard-end, --hard-end-cap-gb, --refusal-run-limit, --slice-blocks, --no-warning-snapshot,
# --fill-cap-bytes) means what it meant, and so do the exit codes.
#
# What moved: the run directory is <out>/profile-<timestamp>-<serial>/ (it was
# <out>/fill-<timestamp>/); the fill's files, summary.txt among them, are in its fill/ subdirectory,
# and PROFILE.txt beside it states the figures with their sources. The keystream file is
# <out>/lto-profile-src.bin. The writer (scripts/lto6-fill-writer.py) is unchanged apart from the
# summary's wording and a --nominal-bytes option.
#
# Usage and the procedure for the real drive: docs/lto-profiling.md, docs/lto6-validation-checklist.md.
exec "$(dirname "$(readlink -f "$0")")/lto-profile.sh" --fill-only "$@"
