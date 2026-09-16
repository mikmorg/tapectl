# shellcheck shell=bash
#
# drive_generation_from_model — the DRIVE's own generation, from its INQUIRY
# product identification (issue #178, ADR-0010 decision 1).
#
# A drive declares only the one generation it IS. What cartridge happens to be
# loaded says nothing about that: an LTO-5 tape sitting in an LTO-6 drive
# during setup used to become `generation = "LTO-5"` in config.toml, and every
# later `volume init` then refused with a message about physics rather than
# naming the config key that was wrong.
#
# Sourced by scripts/first-run.sh. Kept in its own file with no `set -euo
# pipefail` prologue so a test can source it directly
# (tests/first_run_drive_generation.rs).
#
# Recognises the two families this repository has evidence for:
#   IBM      ULT3580-TD<n> / ULT3580-HH<n>, and the older
#            ULTRIUM-TD<n> / ULTRIUM-HH<n>   (mhvtl reports ULT3580-TD6/TD8 --
#            docs/lto6-drive-passthrough.md; scripts/mhvtl-device.sh has the
#            single-digit precedent this generalises)
#   HP/HPE   "Ultrium <n>-SCSI"              (the real drive -- see
#            docs/lto6-drive-passthrough.md and src/tape/health.rs)
#
# Anything else prints nothing and returns 1: an unrecognised model must make
# first-run ASK or DIE, never guess. Guessing is the defect.
drive_generation_from_model() {
    local model="${1:-}" n=""

    # One or more digits, never exactly one: LTO-10 exists and a `[0-9]`
    # pattern would silently read "TD10" as generation 1.
    case "$model" in
        *TD[0-9]*|*HH[0-9]*)
            n="$(printf '%s' "$model" | sed -n 's/.*[TH][DH]\([0-9][0-9]*\).*/\1/p')"
            ;;
        *Ultrium\ [0-9]*-SCSI*|*ULTRIUM\ [0-9]*-SCSI*)
            n="$(printf '%s' "$model" | sed -n 's/.*[Uu][Ll][Tt][Rr][Ii][Uu][Mm] \([0-9][0-9]*\)-SCSI.*/\1/p')"
            ;;
    esac

    [ -n "$n" ] || return 1

    # Type M (LTO-7-M8) is a CARTRIDGE format, never a drive generation
    # (src/media.rs). No model string encodes it, but say so explicitly so a
    # future pattern cannot introduce one by accident.
    case "$model" in
        *M8*|*-M[0-9]*) return 1 ;;
    esac

    printf 'LTO-%s\n' "$n"
}
