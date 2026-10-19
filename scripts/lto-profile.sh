#!/usr/bin/env bash
# lto-profile.sh -- profile an LTO drive on a named test cartridge and emit its
# [[backends.lto]] block (issue #479; ADR-0012 2026-10-17 items 96-98 and 100).
# Operator-facing usage, the report format and an example block: docs/lto-profiling.md.
#
# WHAT IT IS. One generation-agnostic script (LTO-5 to LTO-9, the generations
# src/media.rs knows; IBM, HP and HPE mechanisms, which differ only in the
# strings sg_inq and the log pages print) that absorbs scripts/lto6-measure.sh and
# the past-early-warning fill of scripts/lto6-fill.sh (#420). Those two remain as
# thin wrappers. It measures a drive and a cartridge, writes a run directory with
# the raw captures, and builds from them a report (profile.json, PROFILE.txt) in
# which every figure names its source. `--emit RUN_DIR...` turns one or more
# reports of one drive model into a [[backends.lto]] block that `tapectl config
# check` accepts. It stays a script, not a tapectl command, so a change to a
# measurement needs no release (item 96).
#
# WHAT IT MEASURES.
#   inventory  sg_inq vendor, product and firmware, the drive serial, the supported
#              log pages, READ BLOCK LIMITS, the cartridge's MAM (generation, maker,
#              serial, wear), and the as-found wear counters (log pages 0x02 0x03
#              0x0c 0x17) BEFORE this run writes anything
#   A  block size        512 K and 1 M: acceptance, and throughput of incompressible data
#   B  hardware compression as found (mode page 0x0f DCE; tapectl turns it off per write)
#   C  LBP               Control Data Protection page (0x0a/0xf0), MODE SENSE only
#   D  MAM capacity      remaining-capacity decrease against bytes written: the unit
#                        (MiB or MB) and the over-report
#   E  end of data       a forward read past EOD must ERROR, not return stale data
#   F  the fill          to the first write refusal (early warning), what st and the
#                        drive do after it, and the walk to the hard end (item 100)
# `--quick` runs inventory and A-E (minutes) and skips the fill (hours: about 4.5 h to
# the warning on an LTO-6, longer on later generations).
#
# THIS SCRIPT ERASES THE LOADED CARTRIDGE. It refuses unless you name the cartridge
# on the command line and the name equals the medium serial MAM reports, checked
# before anything destructive runs: ADR-0008's consent shape, naming the thing you
# destroy, the same anchored match lifecycle-suite.sh and lto6-fill.sh use.
#
# Devices are DISCOVERED, never hardcoded (issue #67): the drive is given by its
# /dev/tape/by-id/...-nst name, its sg node comes from sysfs. Nothing else may touch
# the drive: it refuses while tapectl-drive-poll.timer is active, takes the harness
# tape lock (/tmp/tapectl-tape.lock, shared with the gate and the suite), and never
# reads TapeAlert page 0x2E (read-to-clear; only tapectl's own sweep reads it).
# Every log page is read with --maxlen so each read is ONE LOG SENSE (#328).
#
# The measurement sections are ported from lto6-measure.sh, and its comments about
# the bugs they once had are kept where they apply (#116, #117, the dd fullblock).
# The fill is lto6-fill.sh's, run by scripts/lto6-fill-writer.py (one process, one
# open of the device, so st's behaviour AFTER the first refusal is what is measured).
#
# EXIT: 0 done; 1 a finding (the fill's readback FAILED, or a read past EOD returned
# data); 2 a precondition failed (nothing was touched); 3-6 the fill writer's own
# codes (3 interrupted, 4 no refusal before the runaway cap, 5 a write error that is
# not a refusal, 6 an unexpected error in the writer; partial results are kept).
set -uo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
HELPER="$SCRIPT_DIR/lto-profile-report.py"
WRITER="$SCRIPT_DIR/lto6-fill-writer.py"

usage() {
    cat <<'USAGE'
usage: lto-profile.sh --device /dev/tape/by-id/scsi-<serial>-nst \
           --i-will-lose-the-cartridge <MEDIUM_SERIAL> [options]
       lto-profile.sh --emit [--margin F] [--name N] [--device-tape P] RUN_DIR...
       lto-profile.sh --report RUN_DIR

Profiles the drive on the loaded cartridge and writes a run directory with a report.
ERASES THE CARTRIDGE. Full run: minutes of measurements, then the fill (hours).

  --device PATH                  Required. The drive's st node, by-id spelling. No default.
  --i-will-lose-the-cartridge S  Required. The medium serial MAM must report.
  --out DIR                      Where the run directory, the keystream file and the logs
                                 go. Default /scratch/tapectl-lto6 where /scratch exists,
                                 else ~/.cache/tapectl/lto6.
  --quick                        Inventory and the measurements (minutes); no fill.
  --fill-only                    The fill alone (what scripts/lto6-fill.sh runs).
  --note TEXT                    A line stored in the report (e.g. why this cartridge).
  --payload-mb N                 Per-throughput-run payload (default 2048).

  The fill (ignored with --quick):
  --chunk-gib N                  Keystream file size in GiB (default 2).
  --stop-at-warning              Stop at the first refusal; write nothing after it.
  --no-hard-end                  Skip the walk to the hard end (--to-hard-end, the old
                                 opt-in, is still accepted and changes nothing). THE WALK
                                 WRITES INTO THE DRIVE'S END-OF-PARTITION RESERVE.
  --hard-end-cap-gb N            The walk's cap (default 110).
  --refusal-run-limit N          Consecutive refusals that end a step (default 8).
  --slice-blocks N               Blocks in the post-warning slice (default 2048 = 1 GiB).
  --no-warning-snapshot          Do not pause the stream at the warning to read MAM/log pages.
  --fill-cap-bytes N             Runaway cap on the fill (default 1.1 x MAM's maximum
                                 capacity; 3 TB if unreadable).

  --emit RUN_DIR...              No tape access. Print the [[backends.lto]] block for the
                                 runs (one drive model, one medium generation). Options:
                                 --margin F (0.02), --name N, --device-tape P,
                                 --allow-mixed-firmware, --trust-emulated (rehearsals).
  --report RUN_DIR               Rebuild profile.json and PROFILE.txt from the raw captures.

Everything goes to <out>/profile-<timestamp>-<serial>/ ; PROFILE.txt there is the page to read.
USAGE
}

die() { echo "lto-profile: $*" >&2; exit 2; }

if [ "${1:-}" = "--emit" ]; then
    shift
    command -v python3 >/dev/null || die "required binary missing: python3"
    exec python3 "$HELPER" emit "$@"
fi
if [ "${1:-}" = "--report" ]; then
    [ -n "${2:-}" ] || die "--report needs a run directory"
    command -v python3 >/dev/null || die "required binary missing: python3"
    exec python3 "$HELPER" build "$2"
fi

TAPE_DEV=""; LOSE_SERIAL=""; OUT_DIR=""; NOTE=""
DO_MEASURE=1; DO_FILL=1; MODE="full"
PAYLOAD_MB=2048; CHUNK_GIB=2
STOP_AT_WARNING=0; TO_HARD_END=1; HARD_END_CAP_GB=110; RUN_LIMIT=8; SLICE_BLOCKS=2048
WARNING_SNAPSHOT=1; FILL_CAP=""
while [ $# -gt 0 ]; do
    case "$1" in
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --note) NOTE="${2:-}"; shift 2 ;;
        --quick) DO_FILL=0; MODE="quick"; shift ;;
        --fill-only) DO_MEASURE=0; MODE="fill-only"; shift ;;
        --payload-mb) PAYLOAD_MB="${2:-}"; shift 2 ;;
        --chunk-gib) CHUNK_GIB="${2:-}"; shift 2 ;;
        --stop-at-warning) STOP_AT_WARNING=1; shift ;;
        --to-hard-end) TO_HARD_END=1; shift ;;
        --no-hard-end) TO_HARD_END=0; shift ;;
        --hard-end-cap-gb) HARD_END_CAP_GB="${2:-}"; shift 2 ;;
        --refusal-run-limit) RUN_LIMIT="${2:-}"; shift 2 ;;
        --slice-blocks) SLICE_BLOCKS="${2:-}"; shift 2 ;;
        --no-warning-snapshot) WARNING_SNAPSHOT=0; shift ;;
        --fill-cap-bytes) FILL_CAP="${2:-}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "lto-profile: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
if [ "$MODE" = "quick" ] && [ "$DO_MEASURE" = "0" ]; then die "--quick and --fill-only exclude each other"; fi
# A dropped terminal must not end a multi-hour fill (run it inside tmux anyway); children inherit "ignored".
trap '' HUP

[ -n "$TAPE_DEV" ] || die "--device is required; there is no default (/dev/nstN numbering moves)"
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
[ -n "$LOSE_SERIAL" ] || { usage >&2; die "--i-will-lose-the-cartridge SERIAL is required: this run erases the loaded cartridge"; }
for n in "$PAYLOAD_MB" "$CHUNK_GIB" "$HARD_END_CAP_GB" "$RUN_LIMIT" "$SLICE_BLOCKS"; do
    case "$n" in ''|*[!0-9]*|0) die "numeric options must be positive integers (got '$n')" ;; esac
done
[ -z "$FILL_CAP" ] || case "$FILL_CAP" in *[!0-9]*) die "--fill-cap-bytes must be a number" ;; esac
for bin in mt dd sg_inq sg_read_attr sg_logs sg_modes python3 flock df; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
[ "$DO_FILL" = "0" ] || command -v openssl >/dev/null || die "required binary missing: openssl (the fill's keystream)"
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 7) else 1)' || die "python3 3.7 or newer is required (the writer uses os.preadv)"
[ -r "$HELPER" ] || die "the report helper is missing: $HELPER"
[ "$DO_FILL" = "0" ] || [ -r "$WRITER" ] || die "the writer helper is missing: $WRITER"
# The log-page reads below use these sg_logs options (home2's sg3-utils is older than the dev VM's).
SG_LOGS_HELP="$(sg_logs --help 2>&1)"
for opt in '--maxlen' '--in=' '--pdt' '--raw' '--page'; do
    case "$SG_LOGS_HELP" in *"$opt"*) ;; *) die "this sg_logs does not list the $opt option; the log-page reads would fail" ;; esac
done

# Nothing else on the host may open the drive. The daily poll opens the sg node and may read
# TapeAlert; refuse rather than race it.
if command -v systemctl >/dev/null && systemctl is-active --quiet tapectl-drive-poll.timer 2>/dev/null; then
    die "tapectl-drive-poll.timer is active: run 'sudo systemctl stop tapectl-drive-poll.timer' first (and start it again afterwards)"
fi

# Single-drive rule (#9): one tape user at a time, across processes (shared with the gate and the suite).
exec 9>/tmp/tapectl-tape.lock
flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock)"

# ---------- the drive, through the canonical node (issue #334 idiom) ----------
TAPE_REAL="$(readlink -f "$TAPE_DEV")"; ST_NODE="$(basename "$TAPE_REAL")"
[ -d "/sys/class/scsi_tape/$ST_NODE" ] || die "$TAPE_DEV ($TAPE_REAL) is not an st tape node"
case "$ST_NODE" in nst[0-9]*) ;; *) die "$TAPE_DEV is $TAPE_REAL, a REWINDING node: every position in the measurements assumes the non-rewinding one (the by-id name ending -nst)" ;; esac
SG_ENTRIES=(); for e in "/sys/class/scsi_tape/$ST_NODE/device/scsi_generic"/sg*; do [ -e "$e" ] && SG_ENTRIES+=("$(basename "$e")"); done
[ "${#SG_ENTRIES[@]}" -eq 1 ] || die "cannot resolve exactly one sg node for $TAPE_DEV (got ${SG_ENTRIES[*]:-none})"
DRIVE_SG="/dev/${SG_ENTRIES[0]}"
DRIVE_SERIAL="$(tail -c +5 "/sys/class/scsi_tape/$ST_NODE/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
[ -n "$DRIVE_SERIAL" ] || die "cannot read the drive serial from sysfs"

# ---------- consent: the named cartridge is the loaded one ----------
# The match is ANCHORED and label-specific on purpose. A loose `grep -i 'barcode|serial'` matches
# "Density vendor/serial number at last load" FIRST (verified against a real sg_read_attr dump)
# and would compare the operator's cartridge name against the string "IBM XYZZY_A1", refusing
# every legitimate run. A consent check that misfires is a consent check that gets bypassed.
MAM_TXT="$(sg_read_attr "$DRIVE_SG" 2>&1)" || die "sg_read_attr failed on $DRIVE_SG (is a cartridge loaded?)"
SERIAL="$(echo "$MAM_TXT" | grep -iE '^[[:space:]]*medium serial number[[:space:]]*:' | head -1 | sed 's/^[^:]*: *//' | tr -d ' \r')"
[ -n "$SERIAL" ] || die "sg_read_attr reported no medium serial number -- cannot verify consent"
[ "$SERIAL" = "$LOSE_SERIAL" ] || die "loaded cartridge reports serial '$SERIAL' but you named '$LOSE_SERIAL' -- refusing"

# ---------- the out dir and the run dir ----------
if [ -z "$OUT_DIR" ]; then
    if [ -d /scratch ]; then OUT_DIR="/scratch/tapectl-lto6"; else OUT_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl/lto6"; fi
fi
mkdir -p "$OUT_DIR" || die "cannot create the out dir $OUT_DIR"
SRC="${OUT_DIR}/lto-profile-src.bin"
SRC_BYTES=$((CHUNK_GIB * 1024 * 1024 * 1024))
NEED=$((512 * 1024 * 1024))
if [ "$DO_FILL" = "1" ]; then
    { [ -s "$SRC" ] && [ "$(stat -c %s "$SRC")" -eq "$SRC_BYTES" ]; } || NEED=$((NEED + SRC_BYTES))
fi
AVAIL="$(df --output=avail -B1 "$OUT_DIR" | tail -n 1 | tr -d ' ')"
case "$AVAIL" in ''|*[!0-9]*) die "cannot read the free space of $OUT_DIR" ;; esac
[ "$AVAIL" -ge "$NEED" ] || die "$OUT_DIR has $((AVAIL / 1048576)) MiB free but this run needs $((NEED / 1048576)) MiB (the ${CHUNK_GIB} GiB keystream file plus 512 MiB for logs); pass --out DIR on a filesystem with room"

STAMP="$(date +%Y%m%d-%H%M%S)"
SAFE_SERIAL="$(printf '%s' "$SERIAL" | tr -c 'A-Za-z0-9_.-' '_')"
RUN="$OUT_DIR/profile-$STAMP-$SAFE_SERIAL"; mkdir -p "$RUN" || die "cannot create $RUN"
# A gone terminal must not stop the log file being written: tee keeps going past a failed stdout where it can.
TEE_OPT=(); if tee --output-error=warn </dev/null >/dev/null 2>&1; then TEE_OPT=(--output-error=warn); fi
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${TEE_OPT[@]}" "$RUN/profile.log"; }
meta() { python3 "$HELPER" meta "$RUN" "$@"; }
fact() { printf '%s\t%s\n' "$1" "$2" >> "$RUN/facts.tsv"; }
# raw <name> <command...>: keep every raw command output. A summary without the raw capture cannot be
# re-examined once the tape is unloaded. The caller usually inspects the OUTPUT FILE afterwards, so a
# probe that is allowed to come back empty is called with `|| true`; that does not hide a failure that
# matters, because the file it wrote is always there to go check.
raw() {
    local name="$1"; shift
    echo "\$ $*" > "$RUN/$name.txt"
    "$@" >> "$RUN/$name.txt" 2>&1
    return $?
}
# One LOG SENSE per page (#328); never 0x2E here. page <dir> <tag> <page>
page() {
    sg_logs --page="$3" --maxlen=65532 --raw "$DRIVE_SG" > "$1/$2.page_$3.bin" 2>"$1/$2.page_$3.err" \
        && sg_logs --in="$1/$2.page_$3.bin" --raw --pdt=1 > "$1/$2.page_$3.txt" 2>&1
}
# snapshot <dir> <tag>: MAM + pages 0x02 0x03 0x0c 0x17 + mt status, all read-only.
snapshot() {
    sg_read_attr "$DRIVE_SG" > "$1/$2.mam.txt" 2>&1 || true
    for pg in 0x02 0x03 0x0c 0x17; do page "$1" "$2" "$pg" || true; done
    mt -f "$TAPE_DEV" status > "$1/$2.mt.txt" 2>&1 || true
    log "$2: MAM remaining = $(grep -F 'Remaining capacity in partition' "$1/$2.mam.txt" | head -1 | sed 's/^[^:]*: *//')" \
        "| 0x17 used native [MB] = $(grep -F 'Total used native capacity' "$1/$2.page_0x17.txt" 2>/dev/null | head -1 | sed 's/^[^:]*: *//')"
}

meta --set started="$(date -u +%Y-%m-%dT%H:%M:%SZ)" --set mode="$MODE" --set device="$TAPE_DEV" --set device_real="$TAPE_REAL" \
     --set sg="$DRIVE_SG" --set drive_serial="$DRIVE_SERIAL" --set medium_serial="$SERIAL" --set host="$(hostname)" \
     --set note="$NOTE" --set tapectl_commit="$(git -C "$SCRIPT_DIR" describe --always --dirty 2>/dev/null || echo unknown)"
python3 - "$RUN/meta.json" "$STOP_AT_WARNING" "$TO_HARD_END" "$HARD_END_CAP_GB" "$RUN_LIMIT" "$SLICE_BLOCKS" "$WARNING_SNAPSHOT" "$PAYLOAD_MB" "$CHUNK_GIB" <<'PYEOF'
import json, sys
p = sys.argv[1]
m = json.load(open(p))
k = ["stop_at_warning", "to_hard_end", "hard_end_cap_gb", "refusal_run_limit", "slice_blocks", "warning_snapshot", "payload_mb", "chunk_gib"]
m["options"] = dict(zip(k, (int(x) for x in sys.argv[2:])))
json.dump(m, open(p, "w"), indent=1, sort_keys=True)
PYEOF
log "drive $DRIVE_SERIAL at $TAPE_DEV ($TAPE_REAL, $DRIVE_SG); cartridge $SERIAL verified; run $RUN; mode $MODE"

# ---------- inventory: who and what is being profiled ----------
raw mam-preflight sg_read_attr "$DRIVE_SG" || true
raw inq           sg_inq "$DRIVE_SG" || log "sg_inq failed (see inq.txt)"
raw mt-status     mt -f "$TAPE_DEV" status || log "mt status failed"
sg_logs --page=0x00 --maxlen=65532 "$DRIVE_SG" > "$RUN/logs-00.txt" 2>&1 || log "sg_logs page 0x00 unavailable"
if command -v sg_read_block_limits >/dev/null; then raw block-limits sg_read_block_limits "$DRIVE_SG" || true; fi
BL_MAX="$(grep -i 'maximum block size' "$RUN/block-limits.txt" 2>/dev/null | grep -oE '[0-9]+' | head -1)"
snapshot "$RUN" snap-asfound   # the wear counters as found, before this run writes anything (item 98)

DRIVE_VENDOR=""; DRIVE_PRODUCT=""; DRIVE_FIRMWARE=""; DRIVE_GENERATION=""; MEDIUM_GENERATION=""; MEDIUM_NOMINAL_BYTES=""; MEDIUM_MAKER=""; EMULATED=0
PROBLEMS=()
while IFS='=' read -r k v; do
    case "$k" in
        DRIVE_VENDOR) DRIVE_VENDOR="$v" ;;
        DRIVE_PRODUCT) DRIVE_PRODUCT="$v" ;;
        DRIVE_FIRMWARE) DRIVE_FIRMWARE="$v" ;;
        DRIVE_GENERATION) DRIVE_GENERATION="$v" ;;
        MEDIUM_GENERATION) MEDIUM_GENERATION="$v" ;;
        MEDIUM_NOMINAL_BYTES) MEDIUM_NOMINAL_BYTES="$v" ;;
        MEDIUM_MAKER) MEDIUM_MAKER="$v" ;;
        EMULATED) EMULATED="$v" ;;
        PROBLEM) PROBLEMS+=("$v") ;;
    esac
done < <(python3 "$HELPER" identify "$RUN")
log "drive: $DRIVE_VENDOR $DRIVE_PRODUCT firmware $DRIVE_FIRMWARE ($DRIVE_GENERATION); medium: $MEDIUM_GENERATION by $MEDIUM_MAKER, nominal ${MEDIUM_NOMINAL_BYTES:-?} bytes"
if [ "$EMULATED" = "1" ]; then
    log "NOTE: this is an EMULATED device (mhvtl). It does not emulate early warning, the log pages or MAM capacity faithfully; the report marks those figures as fiction."
fi
[ -n "$DRIVE_GENERATION" ] && [ -n "$MEDIUM_GENERATION" ] || PROBLEMS+=("the drive's or the medium's generation could not be identified (identity.json, inq.txt, mam-preflight.txt)")
if [ "${#PROBLEMS[@]}" -gt 0 ]; then
    for p in "${PROBLEMS[@]}"; do log "PROBLEM: $p"; done
    die "the drive/medium pair cannot be profiled (see above); the cartridge was not touched. Raw captures: $RUN"
fi

PAYLOAD=""
cleanup() { [ -z "$PAYLOAD" ] || rm -f "$PAYLOAD"; }
trap 'cleanup' EXIT

# BLOCK SIZE is DRIVER state that outlives this process; the sections below set it and the close restores it.
BS=524288
FINAL_RC=0

if [ "$DO_MEASURE" = "1" ]; then
    # ---------- B. compression as found, C. LBP: read-only ----------
    log "B/C: mode pages 0x0f (compression) and 0x0a/0xf0 (LBP), MODE SENSE only"
    raw modes-0f sg_modes --page=0x0f "$DRIVE_SG" || true
    raw modes-0a-f0 sg_modes --page=0x0a,0xf0 "$DRIVE_SG" || true

    # ---------- A. block size acceptance + throughput ----------
    log "A: block size acceptance and throughput ($PAYLOAD_MB MiB per run)"
    # Payload lives in tmpfs when there's room, so every throughput run reads from RAM rather than the
    # source disk (issue #117): the original harness generated one payload file on a disk and read it
    # once per block size, so the first run read it cold and later runs read it warm from page cache --
    # it measured the source disk, not the tape. Falls back to $RUN (still on disk) when /dev/shm is
    # missing or too small; the source read rate recorded below makes a disk-bound fallback self-evident.
    NEED_MB=$((PAYLOAD_MB + 64))
    SHM_FREE_MB=""
    if [ -d /dev/shm ]; then SHM_FREE_MB="$(df -m /dev/shm 2>/dev/null | awk 'NR==2{print $4}')"; fi
    if [ -n "$SHM_FREE_MB" ] && [ "$SHM_FREE_MB" -ge "$NEED_MB" ] 2>/dev/null; then
        PAYLOAD="/dev/shm/tapectl-lto-profile-payload-$STAMP.bin"; fact payload_location "/dev/shm (tmpfs)"
    else
        PAYLOAD="$RUN/payload.bin"; fact payload_location "$RUN (disk): throughput may be source-disk-bound"
        [ "$AVAIL" -ge $((PAYLOAD_MB * 1048576 + 256 * 1048576)) ] || die "neither /dev/shm nor $RUN has room for the ${PAYLOAD_MB} MiB payload; lower --payload-mb"
    fi
    fact payload_mib "$PAYLOAD_MB"
    # Incompressible on purpose: tapectl writes age ciphertext, so a compressible payload would measure
    # the drive's compressor rather than the medium and report a throughput the real write path can
    # never reach.
    dd if=/dev/urandom of="$PAYLOAD" bs=1M count="$PAYLOAD_MB" status=none 2>/dev/null \
        || die "could not generate a ${PAYLOAD_MB} MiB payload at $PAYLOAD"
    SRC_T0=$(date +%s.%N)
    if dd if="$PAYLOAD" of=/dev/null bs=1M status=none 2>"$RUN/dd-source-read.txt"; then
        SRC_T1=$(date +%s.%N)
        fact source_read_mib_s "$(python3 -c "print(f'{$PAYLOAD_MB / max(1e-6, $SRC_T1 - $SRC_T0):.1f}')")"
    fi

    # READ BLOCK LIMITS is the authoritative statement of what the DRIVE accepts: without it, a failed
    # write is ambiguous between "the drive refuses this size" and "this host's st driver could not
    # buffer it". Those have completely different remedies.
    measure_blocksize() { # measure_blocksize <bytes> <label>
        local bs="$1" label="$2" t0 t1 secs rate
        mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
        if ! mt -f "$TAPE_DEV" setblk "$bs" >"$RUN/setblk-$label.txt" 2>&1; then
            printf '%s\t%s\trejected\t\t\t%s\n' "$label" "$bs" "setblk refused (see setblk-$label.txt)" >> "$RUN/blocksize.tsv"
            return
        fi
        if ! mt -f "$TAPE_DEV" status 2>/dev/null | grep -q "block size $bs\|blocksize $bs\|Tape block size $bs"; then
            raw "mt-status-$label" mt -f "$TAPE_DEV" status || true
            log "setblk $bs returned success but mt status does not report it (mt-status-$label.txt)"
        fi
        mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
        t0=$(date +%s.%N)
        if ! dd if="$PAYLOAD" of="$TAPE_DEV" bs="$bs" status=none 2>"$RUN/dd-$label.txt"; then
            # Distinguish the two very different reasons a write at this size can fail. If the drive's own
            # READ BLOCK LIMITS says the size is within range, the refusal came from THIS HOST's st driver
            # (EBUSY on write is the usual shape) -- a host-side buffer limit, not a property of the drive
            # or the medium.
            local why="write failed (see dd-$label.txt)"
            if grep -qi 'busy' "$RUN/dd-$label.txt" 2>/dev/null; then
                if [ -n "${BL_MAX:-}" ] && [ "$bs" -le "${BL_MAX:-0}" ] 2>/dev/null; then
                    why="host st driver refused (EBUSY); the DRIVE advertises ${BL_MAX} B, so this is a host limit, not a drive limit"
                else
                    why="EBUSY; the drive's max block size is ${BL_MAX:-unknown} B"
                fi
            fi
            printf '%s\t%s\twrite-failed\t\t\t%s\n' "$label" "$bs" "$why" >> "$RUN/blocksize.tsv"
            return
        fi
        t1=$(date +%s.%N)
        secs=$(python3 -c "print(max(1e-6, $t1 - $t0))")
        rate=$(python3 -c "print(f'{$PAYLOAD_MB / $secs:.1f}')")
        printf '%s\t%s\taccepted\t%s\t%.1f\t\n' "$label" "$bs" "$rate" "$secs" >> "$RUN/blocksize.tsv"
    }
    # Two passes in alternating order (issue #117): a real block-size effect looks the same regardless of
    # order; a page-cache/warm-up artifact shows up as a trend across the passes instead of disorder.
    # This is what exposed the original "1 M is 3x faster" artifact -- it was the source disk warming up.
    measure_blocksize 524288 "512K (pass 1)"
    measure_blocksize 1048576 "1M (pass 1)"
    measure_blocksize 1048576 "1M (pass 2)"
    measure_blocksize 524288 "512K (pass 2)"

    # ---------- D. MAM capacity unit and over-report ----------
    log "D: MAM remaining capacity against bytes written"
    # weof at BOT truncates EOD back to BOT, which is what makes the snapshot a clean baseline
    # (docs/lto6-session-journal-2026-09-10.md, Phase 6). Do not simplify this sequence away.
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
    mt -f "$TAPE_DEV" weof 1 >/dev/null 2>&1
    snapshot "$RUN" snap-dbefore
    # Section A's last measure_blocksize call may have left the st driver in 1 M fixed-block mode. This probe
    # writes fixed 512 K blocks and must not inherit that state independently of section A (issue #116):
    # writing 512 K blocks to a driver set for 1 M is EINVAL, and the old code threw both the dd exit status
    # and stderr away, sampling the "after" MAM pair after a zero-byte write and reporting a confident,
    # wrong "no change". Kept independent of measure_blocksize's own setblk on purpose.
    if mt -f "$TAPE_DEV" setblk "$BS" >"$RUN/setblk-mam.txt" 2>&1; then
        MAM_OK=1
        dd if="$PAYLOAD" of="$TAPE_DEV" bs="$BS" status=none 2>"$RUN/dd-mam.txt" || MAM_OK=0
        # Captured either way: even on a failed write the before/after pair is worth having on file (it is
        # what exposed the original bug). Only the derived figures are withheld on failure.
        mt -f "$TAPE_DEV" weof 1 >/dev/null 2>&1
        snapshot "$RUN" snap-dafter
        fact mam_write_ok "$MAM_OK"
        fact mam_payload_bytes "$((PAYLOAD_MB * 1048576))"
    else
        log "D: NOT MEASURED -- could not set block size (setblk-mam.txt)"
        fact mam_write_ok 0
    fi

    # ---------- E. EOD semantics ----------
    log "E: a forward read past end of data must error, not return stale data (the chain walk's physics assumption)"
    # The v2 chain walk assumes a forward read past end-of-data ERRORS. If a drive instead returns stale data
    # from a previous write, a truncated session could read as complete -- the one failure mode the front
    # index and seal marker cannot detect, because both would look internally consistent.
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
    EOD_OK=1
    if ! mt -f "$TAPE_DEV" setblk "$BS" >"$RUN/setblk-eod.txt" 2>&1; then EOD_OK=0; log "E: could not set block size for the EOD probe (setblk-eod.txt)"; fi
    # iflag=fullblock is required: dd reads from a PIPE here, pipe reads return short (~64K), and without
    # fullblock dd writes each short read as its own block -- EINVAL in 512K fixed-block mode, the same
    # silent-failure class as issue #116. These three writes are load-bearing: the probe is meaningless if
    # fewer than three real files ended up on the tape, so a failure is recorded, not skipped.
    EOD_WRITES=0
    for i in 1 2 3; do
        if head -c 524288 /dev/urandom | dd of="$TAPE_DEV" bs=524288 iflag=fullblock status=none 2>"$RUN/dd-eod-$i.txt"; then
            EOD_WRITES=$((EOD_WRITES + 1))
        fi
        mt -f "$TAPE_DEV" weof 1 >/dev/null 2>&1
    done
    fact eod_writes_ok "$EOD_WRITES"
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
    # Skip past all three files, then past EOD. Left silent deliberately: a failed fsf here could leave the
    # head short of EOD, so the read below would return real file-1 data and be misreported as stale data
    # past EOD -- a known gap in this probe, not a write failure.
    mt -f "$TAPE_DEV" fsf 3 >/dev/null 2>&1
    EOD_OUT="$RUN/eod-read.txt"
    dd if="$TAPE_DEV" of=/dev/null bs=524288 count=1 >"$EOD_OUT" 2>&1
    EOD_RC=$?
    READ_BYTES="$(grep -oE '[0-9]+ bytes' "$EOD_OUT" | head -1 | awk '{print $1}')"
    fact eod_read_exit "$EOD_RC"
    fact eod_read_bytes "${READ_BYTES:-0}"
    fact eod_error_text "$(grep -m1 -E '^dd:' "$EOD_OUT" | tr '\t' ' ')"
    if [ "$EOD_OK" -eq 0 ] || [ "$EOD_WRITES" -lt 3 ]; then
        fact eod_verdict not-measured
    elif [ "$EOD_RC" -ne 0 ]; then
        fact eod_verdict errored
    elif [ "${READ_BYTES:-0}" -gt 0 ] 2>/dev/null; then
        fact eod_verdict stale-data
        log "E: FAIL -- a read past EOD returned ${READ_BYTES} bytes instead of erroring. File this against the chain-walk design before writing production data on this drive."
        FINAL_RC=1
    else
        fact eod_verdict no-data
    fi

    # Leave the drive at the block size tapectl actually uses. This run may have set 1 M partway through,
    # and block size is DRIVER state that outlives the process -- leaving it there would make the next
    # mhvtl gate run fail in a way that looks like a code regression.
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
    mt -f "$TAPE_DEV" setblk "$BS" >/dev/null 2>&1 \
        || log "WARNING: could not restore the 512 K block size; run 'mt -f $TAPE_DEV setblk 524288' before the next gate run."
    cleanup; PAYLOAD=""
fi

# ---------- F. the fill: early warning, then what st and the drive do after it ----------
if [ "$DO_FILL" = "1" ]; then
    FILL="$RUN/fill"; mkdir -p "$FILL" || die "cannot create $FILL"
    # Source: incompressible and steady (AES-CTR keystream, a file, 512 KiB write(2)s). A bursty pipe cost
    # 48% more tape on this class of drive (issue #323). In the out dir, NOT /dev/shm: a 2 GiB tmpfs file
    # is 2 GiB of RAM, and the 2026-09-23 fill was killed for memory pressure with it in place.
    if [ ! -s "$SRC" ] || [ "$(stat -c %s "$SRC")" -ne "$SRC_BYTES" ]; then
        log "generating $CHUNK_GIB GiB AES-CTR keystream into $SRC"
        head -c "$SRC_BYTES" /dev/zero \
            | openssl enc -aes-128-ctr -K 000102030405060708090a0b0c0d0e0f -iv 00000000000000000000000000000000 -nosalt \
            > "$SRC" 2>>"$RUN/profile.log" || die "keystream generation failed"
    fi
    BUSY="(is another process holding the drive? 'tapectl-op drive status' names a holder; stop tapectl-drive-poll.timer)"
    mt -f "$TAPE_DEV" rewind || die "rewind failed $BUSY"
    mt -f "$TAPE_DEV" setblk "$BS" || die "setblk failed"
    mt -f "$TAPE_DEV" weof 1 || die "truncate at BOT failed (weof)"
    mt -f "$TAPE_DEV" rewind || die "truncate at BOT failed (rewind)"
    snapshot "$FILL" before

    # The runaway cap: a real drive refuses at about 0.955 of MAM's maximum capacity; an emulator that never
    # refuses would otherwise fill the disk under its media file.
    if [ -z "$FILL_CAP" ]; then
        MAX_MIB="$(grep -F 'Maximum capacity in partition' "$FILL/before.mam.txt" | head -1 | sed 's/^[^:]*: *//' | tr -dc '0-9')"
        if [ -n "$MAX_MIB" ]; then FILL_CAP=$((MAX_MIB * 1048576 * 11 / 10)); else FILL_CAP=3000000000000; fi
    fi
    log "F: runaway cap on the fill: $FILL_CAP bytes"
    WARGS=(--device "$TAPE_DEV" --src "$SRC" --run "$FILL" --max-fill-bytes "$FILL_CAP"
           --slice-blocks "$SLICE_BLOCKS" --refusal-run-limit "$RUN_LIMIT" --hard-end-cap "$((HARD_END_CAP_GB * 1000000000))")
    [ "$WARNING_SNAPSHOT" -eq 1 ] && WARGS+=(--sg "$DRIVE_SG")
    [ "$STOP_AT_WARNING" -eq 1 ] && WARGS+=(--stop-at-warning)
    [ "$TO_HARD_END" -eq 1 ] && WARGS+=(--to-hard-end)
    if [ "$STOP_AT_WARNING" -eq 1 ]; then NEXT="and stopping"; else NEXT="then the past-warning phase"; fi
    log "writing: ONE process, one open of the device, 512 KiB write(2)s from BOT until the first refusal, $NEXT"
    # The writer handles SIGINT / SIGTERM itself (it stops after the current write and closes the device;
    # SIGHUP is ignored); this shell and tee must not die first, so they ignore SIGINT meanwhile.
    trap '' INT
    python3 "$WRITER" write "${WARGS[@]}" 2>&1 | tee -a "${TEE_OPT[@]}" "$RUN/profile.log"
    WRC=${PIPESTATUS[0]}
    trap - INT
    fact fill_writer_rc "$WRC"
    log "writer exited rc=$WRC (0 done, 3 interrupted, 4 no refusal before the runaway cap, 5 stopped on a write error that is not a refusal, 6 unexpected error in the writer, partial results kept)"
    # The device is closed now (st writes a filemark on close if the last operation was a write).
    mt -f "$TAPE_DEV" status > "$FILL/after-close.mt.txt" 2>&1 || true
    snapshot "$FILL" after
    mt -f "$TAPE_DEV" rewind || true
    snapshot "$FILL" after-rewind
    VRC=0
    # Readback runs after a clean run, an interrupt or a writer error: it reads whatever slice result.json records.
    if [ "$STOP_AT_WARNING" -eq 0 ] && { [ "$WRC" -eq 0 ] || [ "$WRC" -eq 3 ] || [ "$WRC" -eq 6 ]; }; then
        log "readback: LOCATE to the post-warning slice and the seal block (the whole tape is never read)"
        python3 "$WRITER" verify --device "$TAPE_DEV" --src "$SRC" --run "$FILL" 2>&1 | tee -a "${TEE_OPT[@]}" "$RUN/profile.log"
        VRC=${PIPESTATUS[0]}
        fact fill_verify_rc "$VRC"
        mt -f "$TAPE_DEV" rewind || true
    fi
    # The kernel's own account of the writes and filemarks (best effort: dmesg may need root).
    { dmesg -T 2>&1 || journalctl -k --no-pager 2>&1; } | grep -iE '\bn?st[0-9]|scsi|tape|\bsg[0-9]|overflow|medium|not permitted|authoriz' | tail -n 500 > "$FILL/dmesg-tail.txt"
    python3 "$WRITER" summary --run "$FILL" --reference-bytes 0 --nominal-bytes "${MEDIUM_NOMINAL_BYTES:-2500000000000}" | tee -a "${TEE_OPT[@]}" "$RUN/profile.log"
    [ "$WRC" -eq 0 ] || FINAL_RC="$WRC"
    [ "$FINAL_RC" -ne 0 ] || [ "$VRC" -eq 0 ] || FINAL_RC=1
fi

# ---------- the report ----------
snapshot "$RUN" snap-final
meta --set finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
python3 "$HELPER" build "$RUN" | tee -a "${TEE_OPT[@]}" "$RUN/profile.log"
log "done; profile.json and PROFILE.txt in $RUN -- the whole directory is the record; scripts/lto-profile.sh --emit $RUN prints the config block"
exit "$FINAL_RC"
