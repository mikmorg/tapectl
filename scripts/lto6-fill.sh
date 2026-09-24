#!/usr/bin/env bash
# The end-of-tape fill: write a real LTO cartridge from BOT until the drive
# refuses, and record where physical end-of-tape fell against MAM's native
# capacity and the generation table's planning figure (2.5 TB for LTO-6).
#
# Ruled by the CTO on 2026-09-23 (ADR-0012 amendment of that evening, item 4):
# run once, by dd, unattended. `scripts/lto6-measure.sh` deliberately leaves
# this drill out ("filling a real LTO-6 cartridge takes hours"), and tapectl's
# own EOT abort path cannot be driven here -- this host cannot stage 2.5 TB and
# the pre-flight gate never plans past 92% of the planning figure. So this
# measures the HARDWARE fact only: bytes accepted before the first write error,
# the errno, the drive's own counters (page 0x0c native-from-BOP-to-EOD,
# page 0x17 total used native) and MAM remaining, before and after.
#
# It writes incompressible data (AES-CTR keystream, generated once into
# /dev/shm) in 512 KiB blocks -- tapectl's block size -- from a file, so the
# feed is steady (a bursty pipe cost 48% more tape on this drive: issue #323).
# The host feed rate is recorded because capacity depends on it.
#
# THIS SCRIPT ERASES THE LOADED CARTRIDGE, and takes hours. Consent is
# ADR-0008's shape: name the cartridge, and the name is checked against the
# medium serial MAM reports before anything destructive runs (the same
# anchored match lifecycle-suite.sh and lto6-measure.sh use).
#
# Usage:
#   scripts/lto6-fill.sh --device /dev/tape/by-id/scsi-<serial>-nst \
#       --i-will-lose-the-cartridge <MEDIUM_SERIAL> [--out DIR] [--chunk-gib N]
#
# Never reads page 0x2E (TapeAlert may be read-to-clear; only tapectl's own
# sweep reads it, journalled). Reads 0x0c and 0x17 with --maxlen so each read
# is ONE LOG SENSE (#328).
set -uo pipefail

TAPE_DEV=""; LOSE_SERIAL=""; OUT_DIR="/scratch/tapectl-lto6"; CHUNK_GIB=2
while [ $# -gt 0 ]; do
    case "$1" in
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --chunk-gib) CHUNK_GIB="${2:-}"; shift 2 ;;
        -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
        *) echo "lto6-fill: unknown argument: $1" >&2; exit 2 ;;
    esac
done
die() { echo "lto6-fill: $*" >&2; exit 2; }

[ -n "$TAPE_DEV" ] || die "--device is required; there is no default (/dev/nstN numbering moves)"
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
[ -n "$LOSE_SERIAL" ] || die "--i-will-lose-the-cartridge SERIAL is required: this fill erases the loaded cartridge"
for bin in mt sg_read_attr sg_logs openssl dd python3; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done

# One tape user at a time, across processes (shared with the gate and the suite).
exec 9>/tmp/tapectl-tape.lock
flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock)"

# ---------- the drive, through the canonical node (issue #334 idiom) ----------
TAPE_REAL="$(readlink -f "$TAPE_DEV")"; ST_NODE="$(basename "$TAPE_REAL")"
[ -d "/sys/class/scsi_tape/$ST_NODE" ] || die "$TAPE_DEV ($TAPE_REAL) is not an st tape node"
SG_ENTRIES=(); for e in "/sys/class/scsi_tape/$ST_NODE/device/scsi_generic"/sg*; do [ -e "$e" ] && SG_ENTRIES+=("$(basename "$e")"); done
[ "${#SG_ENTRIES[@]}" -eq 1 ] || die "cannot resolve exactly one sg node for $TAPE_DEV (got ${SG_ENTRIES[*]:-none})"
DRIVE_SG="/dev/${SG_ENTRIES[0]}"
DRIVE_SERIAL="$(tail -c +5 "/sys/class/scsi_tape/$ST_NODE/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
[ -n "$DRIVE_SERIAL" ] || die "cannot read the drive serial from sysfs"

# ---------- consent: the named cartridge is the loaded one ----------
MAM_TXT="$(sg_read_attr "$DRIVE_SG" 2>&1)" || die "sg_read_attr failed on $DRIVE_SG"
SERIAL="$(echo "$MAM_TXT" | grep -iE '^[[:space:]]*medium serial number[[:space:]]*:' | head -1 | sed 's/^[^:]*: *//' | tr -d ' \r')"
[ -n "$SERIAL" ] || die "sg_read_attr reported no medium serial number -- cannot verify consent"
[ "$SERIAL" = "$LOSE_SERIAL" ] || die "loaded cartridge reports serial '$SERIAL' but you named '$LOSE_SERIAL' -- refusing"

STAMP="$(date +%Y%m%d-%H%M%S)"; RUN="$OUT_DIR/fill-$STAMP"; mkdir -p "$RUN" || die "cannot create $RUN"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$RUN/fill.log"; }
log "drive $DRIVE_SERIAL at $TAPE_DEV ($TAPE_REAL, $DRIVE_SG); cartridge $SERIAL verified; run $RUN"

# One LOG SENSE per page (#328); never 0x2E here.
page() { sg_logs --page="$1" --maxlen=65532 --raw "$DRIVE_SG" > "$RUN/$2.page_$1.bin" 2>"$RUN/$2.page_$1.err" \
         && sg_logs --in="$RUN/$2.page_$1.bin" --raw --pdt=1 > "$RUN/$2.page_$1.txt" 2>&1; }
snapshot() {  # <tag>: MAM + pages 0x0c/0x17 + mt status, all read-only
    sg_read_attr "$DRIVE_SG" > "$RUN/$1.mam.txt" 2>&1 || true
    page 0x0c "$1"; page 0x17 "$1"
    mt -f "$TAPE_DEV" status > "$RUN/$1.mt.txt" 2>&1 || true
    log "$1: MAM remaining [MiB] = $(grep -F 'Remaining capacity in partition' "$RUN/$1.mam.txt" | head -1 | sed 's/^[^:]*: *//')" \
        "| 0x17 used native [MB] = $(grep -F 'Total used native capacity' "$RUN/$1.page_0x17.txt" | head -1 | sed 's/^[^:]*: *//')"
}

# ---------- source: incompressible, steady ----------
# On /scratch, NOT /dev/shm: a 2 GiB tmpfs file is 2 GiB of RAM on a 9.9 GB VM,
# and the 2026-09-23 fill was killed for memory pressure with it in place. The
# page cache keeps the hot file in RAM when it can and gives it back when it
# must; /scratch reads at 200+ MB/s cold, above LTO-6's 160 MB/s anyway.
SRC="${OUT_DIR}/lto6-fill-src.bin"
if [ ! -s "$SRC" ] || [ "$(stat -c %s "$SRC")" -ne $((CHUNK_GIB * 1024 * 1024 * 1024)) ]; then
    log "generating $CHUNK_GIB GiB AES-CTR keystream into $SRC"
    head -c $((CHUNK_GIB * 1024 * 1024 * 1024)) /dev/zero \
        | openssl enc -aes-128-ctr -K 000102030405060708090a0b0c0d0e0f -iv 00000000000000000000000000000000 -nosalt \
        > "$SRC" 2>>"$RUN/fill.log" || die "keystream generation failed"
fi

# ---------- truncate at BOT, then fill ----------
mt -f "$TAPE_DEV" rewind || die "rewind failed"
mt -f "$TAPE_DEV" setblk 524288 || die "setblk failed"
mt -f "$TAPE_DEV" weof 1 && mt -f "$TAPE_DEV" rewind || die "truncate at BOT failed"
snapshot before

log "filling: one continuous stream (the $CHUNK_GIB GiB keystream repeated), bs=512K, until the drive refuses"
# ONE dd, one open of the device: closing an st device after a write makes the
# driver write a filemark and flush the drive's buffer, so a chunk-per-dd loop
# stops and restarts the drive every chunk (measured 77 MB/s vs 151 MB/s for a
# continuous feed on 2026-09-23) -- exactly the irregular feed that costs tape
# (#323), which would distort the EOT position this run exists to measure.
# `iflag=fullblock` keeps every write a whole 512 KiB block (fixed-block mode
# refuses a short one). dd's own final "bytes copied" line is the count.
T0=$(date +%s.%N)
( while cat "$SRC"; do :; done ) 2>/dev/null \
    | dd of="$TAPE_DEV" bs=512K iflag=fullblock status=progress 2>"$RUN/dd.err"
RC=${PIPESTATUS[1]}
T1=$(date +%s.%N)
# status=progress rewrites one line with \r; the last "bytes ... copied" fragment is final.
TOTAL="$(tr '\r' '\n' < "$RUN/dd.err" | grep -E '^[0-9]+ bytes' | tail -1 | awk '{print $1}')"
TOTAL="${TOTAL:-0}"
ERRTXT="$(tr '\r' '\n' < "$RUN/dd.err" | grep -vE '^[0-9]+ bytes|^[0-9]+\+[0-9]+ records' | tail -3 | tr '\n' ' ')"
# dd's count is host-side; the drive's own page 0x0c "Native capacity from BOP
# to EOD" is the tape-side truth, so both are recorded.
log "dd stopped: $TOTAL bytes accepted, rc=$RC: ${ERRTXT}"
mt -f "$TAPE_DEV" status > "$RUN/at-eot.mt.txt" 2>&1 || true
snapshot after
mt -f "$TAPE_DEV" rewind || true
snapshot after-rewind

python3 - "$RUN" "$TOTAL" "$T0" "$T1" "$RC" "$ERRTXT" <<'PY' | tee -a "$RUN/fill.log"
import re, sys
run, total, t0, t1, rc, err = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4]), sys.argv[5], sys.argv[6]
def grab(f, key):
    for l in open(f, errors="replace"):
        if key in l:
            m = re.search(r":\s*([0-9]+)", l); return int(m.group(1)) if m else None
    return None
b_used = grab(f"{run}/before.page_0x17.txt", "Total used native capacity")
# Page 0x17's "used" is read AFTER REWIND: read at EOT it showed a partial
# counter (103,077 vs 2,513,648 after the rewind on 2026-09-24).
a_used = grab(f"{run}/after-rewind.page_0x17.txt", "Total used native capacity")
a_eod  = grab(f"{run}/after.page_0x0c.txt", "Native capacity from BOP to EOD")
b_rem  = grab(f"{run}/before.mam.txt", "Remaining capacity in partition")
a_rem  = grab(f"{run}/after-rewind.mam.txt", "Remaining capacity in partition")
b_max  = grab(f"{run}/before.mam.txt", "Maximum capacity in partition")
secs = t1 - t0
print("== lto6-fill result ==")
print(f"dd accepted {total} bytes ({total/1e12:.4f} TB) in {secs/3600:.2f} h = {total/secs/1e6:.1f} MB/s host feed; stopped rc={rc}: {err.strip()}")
print(f"page 0x17 total used native [MB]: before {b_used}, after rewind {a_used}")
print(f"page 0x0c native BOP->EOD [MB] after: {a_eod}")
print(f"MAM remaining [MiB]: before {b_rem}, after rewind {a_rem}; MAM maximum [MiB]: {b_max}")
if a_used and total:
    print(f"native used per data byte: {a_used*1e6/total:.4f}")
if b_max:
    print(f"physical EOT vs MAM maximum: data bytes / (MAM max MiB * 2^20) = {total/(b_max*1048576):.4f}; vs 2.5 TB planning figure = {total/2.5e12:.4f}")
PY
log "done; raw captures in $RUN"
