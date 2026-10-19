#!/usr/bin/env bash
# lto6-measure.sh -- the LTO-6 measurement harness, now a thin wrapper of
# scripts/lto-profile.sh --quick (issue #479, ADR-0012 2026-10-17 item 96).
#
# The measurements (block size, hardware compression state, LBP, MAM's capacity unit and
# over-report, end-of-data semantics, drive inventory) moved into the generation-agnostic profiling
# script, which also runs the fill and builds a [[backends.lto]] block. This wrapper keeps the old
# command line working:
#
#   --erase-cartridge <SERIAL>  becomes --i-will-lose-the-cartridge. The argument is the cartridge's
#                               MEDIUM SERIAL as MAM reports it (the old script also accepted a MAM
#                               barcode; MAM's barcode attribute is often empty, so the serial is the
#                               one consent check).
#   --allow-unverified-barcode  is REFUSED: a consent check that can be switched off is not one.
#   --payload-mb, --device, --out  are passed through, as are TAPECTL_MEASURE_TAPE,
#                               TAPECTL_MEASURE_OUT and TAPECTL_MEASURE_MB.
#
# What moved: the run directory is <out>/profile-<timestamp>-<serial>/ and the report there is
# PROFILE.txt and profile.json (it was <out>/run-<timestamp>/REPORT.md). The ENOSPC drill this script
# always left out is scripts/lto-profile.sh without --quick.
exec_dir="$(dirname "$(readlink -f "$0")")"
args=()
[ -z "${TAPECTL_MEASURE_TAPE:-}" ] || args+=(--device "$TAPECTL_MEASURE_TAPE")
[ -z "${TAPECTL_MEASURE_OUT:-}" ] || args+=(--out "$TAPECTL_MEASURE_OUT")
[ -z "${TAPECTL_MEASURE_MB:-}" ] || args+=(--payload-mb "$TAPECTL_MEASURE_MB")
while [ $# -gt 0 ]; do
    case "$1" in
        --erase-cartridge) args+=(--i-will-lose-the-cartridge "${2:-}"); shift 2 ;;
        --allow-unverified-barcode)
            echo "lto6-measure: --allow-unverified-barcode is no longer accepted: the named cartridge must equal the medium serial MAM reports (scripts/lto-profile.sh)" >&2
            exit 2 ;;
        *) args+=("$1"); shift ;;
    esac
done
exec "$exec_dir/lto-profile.sh" --quick "${args[@]}"
