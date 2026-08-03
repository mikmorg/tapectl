#!/usr/bin/env bash
# mhvtl device discovery and generation-matched media loading.
#
# THE single implementation of this logic (issue #111). It was inline in
# `scripts/mhvtl-verify-gate.sh`, while `tests/mhvtl_e2e.rs` had a partial
# second copy plus two hardcoded device paths — so the Rust harness could
# silently drive the wrong device, or fail for a reason that looks like a code
# defect. Both callers now shell out to this; do not write a third copy.
#
# Why discovery at all: SCSI enumeration shuffles across module reloads and
# reboots, so `/dev/sg1` is not a stable name for anything. The chain is
#   st node -> `lsscsi -g` row -> HCTL -> /etc/mhvtl/device.conf Drive stanza
#   -> owning Library -> DTE index -> changer sg node by C:T:L
# and the media generation comes from the drive model (ULT3580-TDn -> Ln),
# because loading an L8 tape into a TD6 drive fails in a way that reads like a
# tapectl bug.
#
# Usage:
#   mhvtl-device.sh [--tape /dev/nstN] [--ensure-media]
#
# Prints eval-able KEY=VALUE lines on stdout:
#   TAPE_DEV DRIVE_MODEL DRIVE_SG CHG_SG DTE GEN LOADED_TAG
#
# With --ensure-media it first unloads a wrong-generation cartridge and loads a
# matching one, so LOADED_TAG is guaranteed to be a usable tape.
#
# Exits 2 with a message on stderr if anything cannot be resolved. Callers
# should treat that as a precondition failure, never proceed with defaults.
set -uo pipefail

TAPE_DEV="${TAPECTL_GATE_TAPE:-/dev/nst0}"
ENSURE_MEDIA=0

while [ $# -gt 0 ]; do
    case "$1" in
        --tape) TAPE_DEV="${2:-}"; shift 2 ;;
        --ensure-media) ENSURE_MEDIA=1; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "mhvtl-device.sh: unknown argument: $1" >&2; exit 2 ;;
    esac
done

die() { echo "mhvtl-device.sh: $*" >&2; exit 2; }

[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
for bin in lsscsi mtx; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
[ -r /etc/mhvtl/device.conf ] || die "/etc/mhvtl/device.conf is not readable"

# ---------- drive: st node -> lsscsi row -> HCTL ----------
ST_BASE="$(basename "$TAPE_DEV")"; ST_BASE="${ST_BASE#n}"   # nst0 -> st0
ROW="$(lsscsi -g | awk -v d="/dev/$ST_BASE" '$0 ~ d" " || $NF ~ d {print; exit}')"
[ -n "$ROW" ] || ROW="$(lsscsi -g | grep -F "/dev/$ST_BASE " | head -1)"
[ -n "$ROW" ] || die "cannot find $TAPE_DEV in lsscsi -g"
HCTL="$(echo "$ROW" | sed -n 's/^\[\([0-9:]*\)\].*/\1/p')"
DRIVE_MODEL="$(echo "$ROW" | awk '{print $4}')"
DRIVE_SG="$(echo "$ROW" | awk '{print $NF}')"
T_HOST="$(echo "$HCTL" | cut -d: -f1)"
T_CHAN="$(echo "$HCTL" | cut -d: -f2)"
T_TGT="$(echo "$HCTL" | cut -d: -f3)"
T_LUN="$(echo "$HCTL" | cut -d: -f4)"

# ---------- device.conf: Drive by CHANNEL/TARGET/LUN -> queue ----------
# The owning Library is the last `Library:` stanza seen above the Drive.
DRIVE_Q="" ; LIB_Q="" ; cur_lib=""
while read -r kind q rest; do
    case "$kind" in
        Library:) cur_lib="$q" ;;
        Drive:)
            c=$(echo "$rest" | sed -n 's/.*CHANNEL: *\([0-9][0-9]*\).*/\1/p')
            t=$(echo "$rest" | sed -n 's/.*TARGET: *\([0-9][0-9]*\).*/\1/p')
            l=$(echo "$rest" | sed -n 's/.*LUN: *\([0-9][0-9]*\).*/\1/p')
            if [ "$((10#${c:-99}))" -eq "$((10#$T_CHAN))" ] \
               && [ "$((10#${t:-99}))" -eq "$((10#$T_TGT))" ] \
               && [ "$((10#${l:-99}))" -eq "$((10#$T_LUN))" ]; then
                DRIVE_Q="$q"; LIB_Q="$cur_lib"
            fi ;;
    esac
done < <(grep -E '^(Library|Drive):' /etc/mhvtl/device.conf)
[ -n "$DRIVE_Q" ] || die "no device.conf Drive matches $TAPE_DEV at $HCTL"
DTE=$((DRIVE_Q - LIB_Q - 1))

# ---------- changer sg node: mediumx row matching the Library's C:T:L ----------
LIB_LINE="$(grep -E "^Library: $LIB_Q " /etc/mhvtl/device.conf)"
L_TGT=$(echo "$LIB_LINE" | sed -n 's/.*TARGET: *\([0-9][0-9]*\).*/\1/p')
CHG_SG="$(lsscsi -g | grep mediumx \
    | grep -E "^\[$((10#$T_HOST)):$((10#$T_CHAN)):$((10#$L_TGT)):[0-9]+\]" \
    | awk '{print $NF; exit}')"
[ -n "$CHG_SG" ] || die "cannot locate changer sg node for library $LIB_Q"

# ---------- media generation from the drive model ----------
GEN="$(echo "$DRIVE_MODEL" | sed -n 's/.*TD\([0-9]\).*/L\1/p')"
[ -n "$GEN" ] || die "cannot derive media generation from drive model '$DRIVE_MODEL'"

STATUS="$(mtx -f "$CHG_SG" status)" || die "mtx status failed on $CHG_SG"
LOADED_TAG="$(echo "$STATUS" | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"

if [ "$ENSURE_MEDIA" = "1" ]; then
    if [ -n "$LOADED_TAG" ] && [ "${LOADED_TAG: -2}" != "$GEN" ]; then
        ORIGIN="$(echo "$STATUS" | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
        mtx -f "$CHG_SG" unload "${ORIGIN:-1}" "$DTE" >&2 \
            || die "cannot unload wrong-generation tape"
        LOADED_TAG=""
    fi
    if [ -z "$LOADED_TAG" ]; then
        SLOT="$(echo "$STATUS" | grep -E "Storage Element [0-9]+:Full" \
                | grep "VolumeTag=[EF][0-9]*$GEN" | head -1 \
                | sed -n 's/.*Storage Element \([0-9]*\):Full.*/\1/p')"
        [ -n "$SLOT" ] || die "no $GEN data cartridge in library $LIB_Q"
        mtx -f "$CHG_SG" load "$SLOT" "$DTE" >&2 || die "mtx load $SLOT $DTE failed"
        LOADED_TAG="$(mtx -f "$CHG_SG" status \
            | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"
    fi
fi

# Quoted so a caller can `eval` this safely even if a value ever gains a space.
cat <<EOF
TAPE_DEV='$TAPE_DEV'
DRIVE_MODEL='$DRIVE_MODEL'
DRIVE_SG='$DRIVE_SG'
CHG_SG='$CHG_SG'
DTE='$DTE'
GEN='$GEN'
LOADED_TAG='$LOADED_TAG'
EOF
