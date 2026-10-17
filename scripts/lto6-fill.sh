#!/usr/bin/env bash
# The end-of-tape fill: write a real LTO cartridge from BOT until the drive
# refuses, record where physical end-of-tape fell against MAM's native capacity
# and the generation table's planning figure (2.5 TB for LTO-6) -- and, since
# 2026-10-17 (issue #420, the short seal), measure what the Linux st driver and
# the drive do AFTER that first refusal.
#
# WHY THE SECOND HALF. #420 seals a tape short: at the drive's early warning it
# finishes the slice being written (1 GiB = 2048 blocks of 512 KiB), then writes
# a seal. Whether that fits depends on facts nothing has recorded: after the
# first `No space left on device` (ENOSPC) does st accept the next write, refuse
# it again, or return short counts? Does the drive take a whole slice, a
# filemark, a seal block, a filemark? How much room is there to the hard end?
# mhvtl does not emulate early warning faithfully (docs/lto6-validation-checklist.md),
# so only a real drive answers. This script measures; it assumes nothing.
#
# THE FIRST HALF (ruled by the CTO on 2026-09-23, ADR-0012 amendment of that
# evening, item 4): `scripts/lto6-measure.sh` deliberately leaves this drill out
# ("filling a real LTO-6 cartridge takes hours"), and tapectl's own EOT abort
# path cannot be driven here -- this host cannot stage 2.5 TB and the pre-flight
# gate never plans past the fill ceiling (97% of the planning figure by default;
# #391). It records the HARDWARE fact: bytes accepted before the first write
# error, the errno, the drive's own counters (page 0x0c native-from-BOP-to-EOD,
# page 0x17 total used native) and MAM remaining. It writes incompressible data
# (AES-CTR keystream, generated once into the out dir) in 512 KiB blocks --
# tapectl's block size -- from a file, so the feed is steady (a bursty pipe cost
# 48% more tape on this drive: issue #323). The host feed rate is recorded
# because capacity depends on it.
#
# HOW IT WRITES NOW. The 2026-09-24 fill used one dd. This one uses one python3
# process (scripts/lto6-fill-writer.py) from BOT to the end, because the question
# is what happens on the SAME open device after the first refusal: the st driver
# admits one opener, and closing it after a write makes the driver write a
# filemark, so a dd that stopped at the refusal and a second process that
# continued would measure a reopened device with a filemark of unknown outcome in
# between. The feed has the same shape (512 KiB write(2)s from a page-cached
# file); the summary prints the bytes accepted beside the 2026-09-24 figure
# (2,501,995,134,976), so any difference between the two writers shows.
# `--stop-at-warning` is the old behaviour: stop at the first refusal, write
# nothing more.
#
# THE PAST-WARNING PHASE (the default). On the first refusal, in the same process:
#   1. Record the errno, the bytes accepted, and st's own state (MTIOCGET, the
#      data `mt status` prints -- `mt` itself cannot run while the writer holds
#      the drive). Read the MAM and log pages 0x0c / 0x17 over sg; the stream
#      pauses a few seconds for that (--no-warning-snapshot skips it).
#   2. Write ONE slice (2048 blocks, one write(2) each, the way tapectl's writer
#      would), logging every attempt (index, return value or errno, timestamp)
#      until 2048 blocks are accepted or a stop condition: a non-ENOSPC error, a
#      short count, or 8 refusals in a row (--refusal-run-limit). st itself
#      remembers one state (it refuses once, then "allows the next write"), so a
#      run of 3 already shows a sticky refusal; 8 survives any alternating
#      accept/refuse pattern and costs nothing, a refused write returning in
#      microseconds.
#   3. Write a filemark, then ONE block standing in for the seal marker (a text
#      header with the time and the accepted-block count, zero-padded to
#      512 KiB), then a filemark, logging each result and the drive's block
#      address (READ POSITION) around them.
#   4. With --to-hard-end (OFF by default): keep writing blocks until a
#      non-recoverable error, capped at 110 GB past the warning. THIS WRITES INTO
#      THE DRIVE'S END-OF-PARTITION RESERVE (about 99 GB on this drive).
# The tape then holds file F = the fill and, in the same file, the slice (no
# filemark ever separates them; F is the file number at the refusal, 0 on a
# fresh tape), a filemark, the seal block as file F+1, a filemark.
#
# THE READBACK never reads the whole tape. It rewinds and LOCATEs by the logical
# block address recorded while the writer held the drive: it reads the whole
# post-warning slice (1 GiB, seconds) and compares every block with the
# keystream, checks the filemark after it, spaces ONE filemark from the last
# slice block to the seal block (fsf 1), reads the seal block and the filemark
# after it, and (with --to-hard-end) the first and last blocks of the walk. A
# filemark-count spacing from BOT cannot reach the slice -- it lies inside file
# 0 -- and its speed across 2.5 TB has never been measured, so it is not used.
# Each check prints PASS or FAIL; the drive's READ POSITION is compared with the
# writer's count, which catches a refused write that kept its data.
#
# THE SUMMARY (summary.txt in the run directory) states, in plain lines: bytes
# accepted before the first refusal; what write(2) returned for each attempt
# after it (run-length, e.g. "1 refusal (ENOSPC), then 2048 accepted"); whether a
# full slice, the filemark and the seal block were accepted; the headroom with
# --to-hard-end; each readback check; the drive counters before, at the warning
# and after; the host feed rate. Send the whole run directory back.
#
# THIS SCRIPT ERASES THE LOADED CARTRIDGE, and takes hours (about 4.5 h to the
# warning on an LTO-6, minutes after it). Consent is ADR-0008's shape: name the
# cartridge, and the name is checked against the medium serial MAM reports
# before anything destructive runs (the same anchored match lifecycle-suite.sh
# and lto6-measure.sh use).
#
# Nothing else may touch the drive: stop tapectl-drive-poll.timer first (this
# script refuses while it is active; the 06:30 poll opens the sg node and may
# read TapeAlert) and check that `tapectl-op drive status` names no holder.
#
# Never reads page 0x2E (TapeAlert may be read-to-clear; only tapectl's own
# sweep reads it, journalled). Reads 0x0c and 0x17 with --maxlen so each read
# is ONE LOG SENSE (#328).
set -uo pipefail

usage() {
    cat <<'USAGE'
usage: lto6-fill.sh --device /dev/tape/by-id/scsi-<serial>-nst \
           --i-will-lose-the-cartridge <MEDIUM_SERIAL> [options]

Fills the loaded cartridge from BOT to the drive's first write refusal (early
warning), then measures what st and the drive do after it (issue #420) and
reads the result back. ERASES THE CARTRIDGE; about 4.5 hours to the warning,
minutes after it.

  --device PATH                  Required. The drive's st node, by-id spelling. No default.
  --i-will-lose-the-cartridge S  Required. The medium serial MAM must report.
  --out DIR                      Where the run directory, the keystream file and the logs
                                 go. Default /scratch/tapectl-lto6 where /scratch exists,
                                 else ~/.cache/tapectl/lto6. Refuses if its filesystem
                                 lacks room for the keystream plus 512 MiB.
  --chunk-gib N                  Keystream file size in GiB (default 2).
  --stop-at-warning              The old behaviour: stop at the first refusal, write nothing more.
  --to-hard-end                  After the seal, keep writing until a non-recoverable error
                                 or 110 GB past the warning. OFF by default. THIS WRITES
                                 INTO THE DRIVE'S END-OF-PARTITION RESERVE.
  --hard-end-cap-gb N            The --to-hard-end cap (default 110).
  --refusal-run-limit N          Consecutive refusals that end a step (default 8).
  --slice-blocks N               Blocks in the post-warning slice (default 2048 = 1 GiB).
                                 Change only for rehearsals on emulated media.
  --no-warning-snapshot          Do not pause the stream at the warning to read the MAM and
                                 log pages (they are then read only before and after).
  --fill-cap-bytes N             Runaway cap on the fill (default 1.1 x MAM's maximum
                                 capacity; 3 TB if unreadable). A real drive refuses far
                                 below it; an emulator that never refuses stops here.

Everything goes to <out>/fill-<timestamp>/ ; summary.txt there is the page to send back.
USAGE
}

TAPE_DEV=""; LOSE_SERIAL=""; CHUNK_GIB=2; OUT_DIR=""
STOP_AT_WARNING=0; TO_HARD_END=0; HARD_END_CAP_GB=110; RUN_LIMIT=8; SLICE_BLOCKS=2048
WARNING_SNAPSHOT=1; FILL_CAP=""
while [ $# -gt 0 ]; do
    case "$1" in
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --chunk-gib) CHUNK_GIB="${2:-}"; shift 2 ;;
        --stop-at-warning) STOP_AT_WARNING=1; shift ;;
        --to-hard-end) TO_HARD_END=1; shift ;;
        --hard-end-cap-gb) HARD_END_CAP_GB="${2:-}"; shift 2 ;;
        --refusal-run-limit) RUN_LIMIT="${2:-}"; shift 2 ;;
        --slice-blocks) SLICE_BLOCKS="${2:-}"; shift 2 ;;
        --no-warning-snapshot) WARNING_SNAPSHOT=0; shift ;;
        --fill-cap-bytes) FILL_CAP="${2:-}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "lto6-fill: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
die() { echo "lto6-fill: $*" >&2; exit 2; }

[ -n "$TAPE_DEV" ] || die "--device is required; there is no default (/dev/nstN numbering moves)"
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
[ -n "$LOSE_SERIAL" ] || die "--i-will-lose-the-cartridge SERIAL is required: this fill erases the loaded cartridge"
for n in "$CHUNK_GIB" "$HARD_END_CAP_GB" "$RUN_LIMIT" "$SLICE_BLOCKS"; do
    case "$n" in ''|*[!0-9]*|0) die "numeric options must be positive integers (got '$n')" ;; esac
done
[ -z "$FILL_CAP" ] || case "$FILL_CAP" in *[!0-9]*) die "--fill-cap-bytes must be a number" ;; esac
for bin in mt sg_read_attr sg_logs openssl python3 flock df; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 7) else 1)' || die "python3 3.7 or newer is required (the writer uses os.preadv)"
WRITER="$(dirname "$(readlink -f "$0")")/lto6-fill-writer.py"
[ -r "$WRITER" ] || die "the writer helper is missing: $WRITER"

# Nothing else on the host may open the drive for the hours this takes. The daily
# poll opens the sg node and may read TapeAlert; refuse rather than race it.
if command -v systemctl >/dev/null && systemctl is-active --quiet tapectl-drive-poll.timer 2>/dev/null; then
    die "tapectl-drive-poll.timer is active: run 'sudo systemctl stop tapectl-drive-poll.timer' first (and start it again afterwards)"
fi

# One tape user at a time, across processes (shared with the gate and the suite).
exec 9>/tmp/tapectl-tape.lock
flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock)"

# ---------- the out dir: it must exist somewhere real and have room ----------
# /scratch is the dev VM's; the production host (home2) has none.
if [ -z "$OUT_DIR" ]; then
    if [ -d /scratch ]; then OUT_DIR="/scratch/tapectl-lto6"; else OUT_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl/lto6"; fi
fi
mkdir -p "$OUT_DIR" || die "cannot create the out dir $OUT_DIR"
SRC="${OUT_DIR}/lto6-fill-src.bin"
SRC_BYTES=$((CHUNK_GIB * 1024 * 1024 * 1024))
NEED=$((512 * 1024 * 1024))
{ [ -s "$SRC" ] && [ "$(stat -c %s "$SRC")" -eq "$SRC_BYTES" ]; } || NEED=$((NEED + SRC_BYTES))
AVAIL="$(df --output=avail -B1 "$OUT_DIR" | tail -n 1 | tr -d ' ')"
case "$AVAIL" in ''|*[!0-9]*) die "cannot read the free space of $OUT_DIR" ;; esac
[ "$AVAIL" -ge "$NEED" ] || die "$OUT_DIR has $((AVAIL / 1048576)) MiB free but this run needs $((NEED / 1048576)) MiB (the ${CHUNK_GIB} GiB keystream file plus 512 MiB for logs); pass --out DIR on a filesystem with room"

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
log "options: stop-at-warning=$STOP_AT_WARNING to-hard-end=$TO_HARD_END (cap ${HARD_END_CAP_GB} GB) refusal-run-limit=$RUN_LIMIT slice-blocks=$SLICE_BLOCKS warning-snapshot=$WARNING_SNAPSHOT; out dir has $((AVAIL / 1048576)) MiB free"

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
# In the out dir, NOT /dev/shm: a 2 GiB tmpfs file is 2 GiB of RAM on a 9.9 GB VM,
# and the 2026-09-23 fill was killed for memory pressure with it in place. The
# page cache keeps the hot file in RAM when it can and gives it back when it
# must; /scratch reads at 200+ MB/s cold, above LTO-6's 160 MB/s anyway.
if [ ! -s "$SRC" ] || [ "$(stat -c %s "$SRC")" -ne "$SRC_BYTES" ]; then
    log "generating $CHUNK_GIB GiB AES-CTR keystream into $SRC"
    head -c "$SRC_BYTES" /dev/zero \
        | openssl enc -aes-128-ctr -K 000102030405060708090a0b0c0d0e0f -iv 00000000000000000000000000000000 -nosalt \
        > "$SRC" 2>>"$RUN/fill.log" || die "keystream generation failed"
fi

# ---------- truncate at BOT, then fill ----------
BUSY="(is another process holding the drive? 'tapectl-op drive status' names a holder; stop tapectl-drive-poll.timer)"
mt -f "$TAPE_DEV" rewind || die "rewind failed $BUSY"
mt -f "$TAPE_DEV" setblk 524288 || die "setblk failed"
mt -f "$TAPE_DEV" weof 1 || die "truncate at BOT failed (weof)"
mt -f "$TAPE_DEV" rewind || die "truncate at BOT failed (rewind)"
snapshot before

# The runaway cap: a real drive refuses at 0.955 of MAM's maximum capacity; an
# emulator that never refuses would otherwise fill the disk under its media file.
if [ -z "$FILL_CAP" ]; then
    MAX_MIB="$(grep -F 'Maximum capacity in partition' "$RUN/before.mam.txt" | head -1 | sed 's/^[^:]*: *//' | tr -dc '0-9')"
    if [ -n "$MAX_MIB" ]; then FILL_CAP=$((MAX_MIB * 1048576 * 11 / 10)); else FILL_CAP=3000000000000; fi
fi
log "runaway cap on the fill: $FILL_CAP bytes (1.1 x MAM maximum capacity)"

WARGS=(--device "$TAPE_DEV" --src "$SRC" --run "$RUN" --max-fill-bytes "$FILL_CAP"
       --slice-blocks "$SLICE_BLOCKS" --refusal-run-limit "$RUN_LIMIT" --hard-end-cap "$((HARD_END_CAP_GB * 1000000000))")
[ "$WARNING_SNAPSHOT" -eq 1 ] && WARGS+=(--sg "$DRIVE_SG")
[ "$STOP_AT_WARNING" -eq 1 ] && WARGS+=(--stop-at-warning)
[ "$TO_HARD_END" -eq 1 ] && WARGS+=(--to-hard-end)

if [ "$STOP_AT_WARNING" -eq 1 ]; then NEXT="and stopping"; else NEXT="then the past-warning phase"; fi
log "writing: ONE process, one open of the device, 512 KiB write(2)s from BOT until the first refusal, $NEXT"
# The writer handles SIGINT / SIGTERM / SIGHUP itself (it stops after the current
# write and closes the device); this shell and tee must not die first, so they
# ignore SIGINT meanwhile.
trap '' INT
python3 "$WRITER" write "${WARGS[@]}" 2>&1 | tee -a "$RUN/fill.log"
WRC=${PIPESTATUS[0]}
trap - INT
log "writer exited rc=$WRC (0 done, 3 interrupted, 4 no refusal before the runaway cap, 5 stopped on a write error that is not a refusal)"

# The device is closed now (st writes a filemark on close if the last operation was a write).
mt -f "$TAPE_DEV" status > "$RUN/after-close.mt.txt" 2>&1 || true
log "mt status after the writer closed: $(tr '\n' ' ' < "$RUN/after-close.mt.txt")"
snapshot after
mt -f "$TAPE_DEV" rewind || true
snapshot after-rewind

VRC=0
if [ "$STOP_AT_WARNING" -eq 0 ] && [ "$WRC" -eq 0 ]; then
    log "readback: LOCATE to the post-warning slice and the seal block (the whole tape is never read)"
    python3 "$WRITER" verify --device "$TAPE_DEV" --src "$SRC" --run "$RUN" 2>&1 | tee -a "$RUN/fill.log"
    VRC=${PIPESTATUS[0]}
    mt -f "$TAPE_DEV" rewind || true
fi

python3 "$WRITER" summary --run "$RUN" | tee -a "$RUN/fill.log"
log "done; raw captures and summary.txt in $RUN -- send that whole directory back"
[ "$WRC" -eq 0 ] || exit "$WRC"
[ "$VRC" -eq 0 ] || exit 1
exit 0
