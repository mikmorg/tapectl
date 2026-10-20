#!/usr/bin/env bash
# short-seal-acceptance.sh -- the 1.6.0 acceptance run on a real drive (issue #420;
# ADR-0012 items 110, 114, 115 and 117). The procedure around it is
# docs/lto6-validation-checklist.md, "The short seal on the real drive (1.6.0 acceptance)".
#
# WHAT IT PROVES. A write that meets the drive's real early warning seals short, and what it
# wrote past the warning (at most one slice and the seal) reads back (item 110: a failed
# readback stops #420). Around it, with real units: the cut is recorded, every whole unit
# restores through tapectl and through RESTORE.sh, the catalog rebuilds from the short tape to
# the rows the write recorded, and the cut unit is then written whole onto the same cartridge,
# erased and initialised again as a second volume (home2 has one test cartridge).
#
# HOW THE TAPE IS FILLED (item 115). Three small real units and, sorting between them, one
# filler unit: staged normally from a source a little larger than one slice, then given N slice
# rows that all name its one real staged slice (examples/short_seal_filler.rs, `fill`, which
# says how that passes `stage create`'s and `volume write`'s checks). Nothing but the filler's
# rows is synthetic, and the filler is cut, so nothing of it is ever restored.
#
# THE CAPACITY. Item 113 lets a plan reach 100% of the cartridge's capacity and no further, and
# the HP LTO-6's first refusal came at 2,502,136,168,448 bytes on this cartridge (2026-10-20
# profile; 2,501,995,134,976 on 2026-09-23): 1.0009 x the 2.5 TB the generation table gives
# LTO-6. A plan bounded at 2.5 TB ends about 2 GB BEFORE the warning, seals full and proves
# nothing. So the run registers the test cartridge in its throwaway catalog with the capacity
# it measured past the warning, `cartridge register --capacity` (the cartridge row ADR-0010
# provides for a cartridge whose capacity differs from its generation's figure; the drive's
# capacity_override is for virtual drives only). The default, 2.55 TB, puts the fill ceiling
# (97%, 2.47 TB) below the warning and the plan bound above it, so the filler is planned past
# the ceiling (item 113) and the warning lands inside it. Production keeps the table figure.
#
# A FAIL STOPS THE RUN and leaves the cartridge as the failed step left it: no later step, no
# exit trap, ever writes the medium a failed step measured (#420's lost readback, 2026-10-20).
# The run directory keeps every command's output, the catalog, and the drive's sysfs options
# and counters, MAM and log pages 0x02 0x03 0x0c 0x17 before and after every step.
#
# REFUSES unless every one of these holds (exit 2, nothing touched):
#   - TAPECTL_HOME is set, absent or empty, and none of production's homes: /etc/tapectl/home's
#     target, the tapectl service user's home, /srv/archive_meta/tapectl (home2.profile), or
#     this account's ~/.tapectl;
#   - the drive's serial is HUJ808A5L4 and the cartridge's chip serial is EW7VWMVKF6, named
#     again with --i-will-lose-the-cartridge (the barcode is a sticker; ADR-0012) -- or --mhvtl,
#     on an mhvtl drive, never that one;
#   - TAPECTL_TEST_EARLY_WARNING_AT is unset (a real run with the test hook proves nothing);
#   - tapectl-drive-poll.timer is not active, and the tape lock is free.
#
# --mhvtl is the rehearsal on a virtual drive (vm-desk1). mhvtl loses data past its medium's
# capacity instead of refusing it, so the rehearsal sets the store's test hook,
# TAPECTL_TEST_EARLY_WARNING_AT, inside the filler and below the medium's end, for the first
# write only; the sizes shrink to fit mhvtl's 500 MB cartridges (CAPACITY=500).
#
# EXIT: 0 every step passed; 1 a step FAILED (the run stopped there; the cartridge is as that
# step left it); 2 a precondition refused (nothing was touched).
set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"

TEST_DRIVE_SERIAL="HUJ808A5L4"
TEST_CARTRIDGE="EW7VWMVKF6"
# contrib/hosts/home2.profile, SVC_HOME_WANT: the service user's home on home2.
HOME2_SVC_HOME="/srv/archive_meta/tapectl"

LABEL1="SSA-0001"
LABEL2="SSA-0002"
OPERATOR="ssa-op"
TENANT="ssa"
# `volume write` orders the batch by unit name, so the filler sorts between the real units and
# the one after it is cut with it (item 112): that is "the cut real unit" of step 7.
UNIT_A="ssa-a-docs"
UNIT_B="ssa-b-photos"
UNIT_F="ssa-m-filler"
UNIT_Z="ssa-z-tail"

usage() {
    cat <<'USAGE'
usage: TAPECTL_HOME=<fresh dir> TAPECTL_BIN=<tapectl> [FIXTURE_BIN=<short_seal_filler>] \
       scripts/short-seal-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
           --i-will-lose-the-cartridge EW7VWMVKF6 [options]
       scripts/short-seal-acceptance.sh --mhvtl --device /dev/tape/by-id/scsi-XYZZY_A3-nst [options]
       scripts/short-seal-acceptance.sh --print-plan [--mhvtl]

The 1.6.0 acceptance run: fill the test cartridge past the drive's early warning, prove the
short seal, then restore, rebuild and rewrite around it. OVERWRITES THE CARTRIDGE.

  --device PATH                  Required. The drive's st node, by-id spelling. No default.
  --i-will-lose-the-cartridge S  Required on the real drive: EW7VWMVKF6, checked against MAM.
  --mhvtl                        The rehearsal on an mhvtl drive (the test hook, small sizes).
  --out DIR                      Where the run directory goes. Default /scratch/tapectl-short-seal
                                 where /scratch exists, else ~/.cache/tapectl/short-seal.
  --capacity BYTES               The capacity registered for the cartridge, a plain decimal
                                 byte count (default 2550000000000; --mhvtl 200000000).
  --hook-at BYTES                --mhvtl only: TAPECTL_TEST_EARLY_WARNING_AT for the first
                                 write (default: 98% of the way through the filler).
  --filler-dir DIR               Where the filler's one slice file is read from (default: a
                                 directory on /dev/shm when it has room, else staging).
  --print-plan                   Print what the operator must do and the drive time, and exit.

  TAPECTL_HOME (env)             Required: the throwaway home, absent or empty.
  TAPECTL_BIN (env)              Required: the tapectl under test (1.6.0 or later on the real
                                 drive).
  FIXTURE_BIN (env)              The short_seal_filler example built with it. Default:
                                 <dir of TAPECTL_BIN>/examples/short_seal_filler.
  DAR_BIN (env)                  dar for the throwaway config. Default: dar on PATH.
USAGE
}

die() { echo "short-seal-acceptance: REFUSED: $*" >&2; exit 2; }

TAPE_DEV=""; LOSE_SERIAL=""; OUT_DIR=""; MHVTL=0; CAPACITY=""; HOOK_AT=""; FILLER_DIR=""
PRINT_PLAN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --mhvtl) MHVTL=1; shift ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --capacity) CAPACITY="${2:-}"; shift 2 ;;
        --hook-at) HOOK_AT="${2:-}"; shift 2 ;;
        --filler-dir) FILLER_DIR="${2:-}"; shift 2 ;;
        --print-plan) PRINT_PLAN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "short-seal-acceptance: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

# ---------- the two shapes of the run ----------
MiB=$((1024 * 1024)); GiB=$((1024 * MiB))
if [ "$MHVTL" = 1 ]; then
    SLICE_SIZE="1M"; SLICE_BYTES=$MiB
    : "${CAPACITY:=200000000}"
    POST_WARNING="64M"
    ENOSPC_BUFFER="4M"
    ALLOWANCE=$((16 * MiB))
    SIZE_A=$((600 * 1024)); SIZE_B=$((2 * MiB + 512 * 1024)); SIZE_Z=$((1536 * 1024))
else
    SLICE_SIZE="1G"; SLICE_BYTES=$GiB
    : "${CAPACITY:=2550000000000}"
    POST_WARNING="99008M"   # ADR-0012 item 118: the profile's --emit figure
    ENOSPC_BUFFER=""        # the default, 50M, as production
    ALLOWANCE=$((512 * MiB))
    SIZE_A=$((64 * MiB)); SIZE_B=$((1536 * MiB)); SIZE_Z=$((256 * MiB))
fi
# The filler's source: one full slice and a sixteenth, so its slice 1 is a full dar slice.
SIZE_F=$((SLICE_BYTES + SLICE_BYTES / 16))
case "$CAPACITY" in ''|*[!0-9]*) die "--capacity must be a plain decimal byte count (got '$CAPACITY')" ;; esac
[ -z "$HOOK_AT" ] || case "$HOOK_AT" in *[!0-9]*) die "--hook-at must be a plain decimal byte count" ;; esac
[ -z "$HOOK_AT" ] || [ "$MHVTL" = 1 ] || die "--hook-at is the rehearsal's test hook: it needs --mhvtl"

print_plan() {
    local cap_tb
    cap_tb="$(awk -v b="$CAPACITY" 'BEGIN { printf "%.3f", b / 1e12 }')"
    cat <<PLAN
=== The 1.6.0 short-seal acceptance run (ADR-0012 items 110, 114, 115, 117) ===

BEFORE YOU START (the operator):
  1. Load the test cartridge EW7VWMVKF6 into the HP LTO-6 (HUJ808A5L4). Nothing on it is kept:
     the run writes a new volume over it, twice.
  2. Nothing else may use the drive for the whole run:
       sudo systemctl stop tapectl-drive-poll.timer      (start it again afterwards)
     no production tapectl command running or scheduled, and the host quiet (vm-desk1's heavy
     jobs paused, docs/operator-guide.md "A quiet host while the tape runs").
  3. Run it inside tmux: it takes most of a day. A dropped terminal does not stop it.
  4. A FAIL stops the run and leaves the cartridge as it is. Do not reuse or erase it until the
     failure is read (docs/lto6-validation-checklist.md).

THE STEPS and the drive time each takes on the HP LTO-6 (estimates: the write at ~158 MB/s, the
reads at the 140 MiB/s production verify ran at, RECORD #326):
  1 setup        no tape. A throwaway home and config (post_warning_bytes = $POST_WARNING,
                 min_copies = 1), the cartridge registered at $cap_tb TB, four units staged
                 (~3 GiB through dar and age, about 5 min), the filler's rows.        ~10 min
  2 volume init  $LABEL1.                                                              ~2 min
  3 volume write the plan reaches past the 2.47 TB fill ceiling to ~$cap_tb TB; the drive's
                 warning comes near 2.502 TB, inside the filler: a short seal, exit 0, the
                 filler and $UNIT_Z cut, and item 114's readback of the slice
                 past the warning and the seal.                                    ~4.5-5 h
  4 verify       volume verify --full, then RESTORE.sh --info and --verify with the real
                 tools: two whole-tape reads.                                       ~9-10 h
  5 restore      $UNIT_B (1.5 GiB, 2 slices) by tapectl and by RESTORE.sh, each
                 diffed against its source; the cut unit refused by RESTORE.sh.     ~15 min
  6 rebuild      catalog rebuild --from-volume into a second home, compared row by row.
                                                                                   ~15 min
  7 rewrite      the filler released, $LABEL1 retired, cartridge erase (short), $LABEL2
                 initialised without --force, the cut unit written whole onto it,
                 verified and restored.                                            ~20 min
  In all about 14-16 hours of drive time; step 4's two whole-tape reads are most of it.

THE COMMAND (operator's login on home2):
  TAPECTL_HOME=\$HOME/.cache/tapectl/ssa-home-\$(date +%Y%m%d) \\
  TAPECTL_BIN=<the 1.6.0 candidate> FIXTURE_BIN=<its short_seal_filler> \\
    scripts/short-seal-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \\
      --i-will-lose-the-cartridge EW7VWMVKF6

Afterwards the cartridge holds $LABEL2 (the cut unit only) and belongs to the throwaway home:
erase it from that home (cartridge erase --confirm-label $LABEL2) before it is a test cartridge
again, and start the drive-poll timer.
PLAN
}

if [ "$PRINT_PLAN" = 1 ]; then print_plan; exit 0; fi

# ---------- preconditions: nothing below touches the drive until every one holds ----------
[ -n "$TAPE_DEV" ] || { usage >&2; die "--device is required; there is no default (/dev/nstN numbering moves)"; }
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
for bin in mt dd tr sg_read_attr sg_logs python3 flock age dar tar sha256sum diff awk tee df; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
SG_LOGS_HELP="$(sg_logs --help 2>&1 || true)"
for opt in '--maxlen' '--in=' '--pdt' '--raw' '--page'; do
    case "$SG_LOGS_HELP" in *"$opt"*) ;; *) die "this sg_logs does not list the $opt option; the log-page reads would fail" ;; esac
done

# A real write with the test hook seals where the hook says, not where the medium does.
if [ -n "${TAPECTL_TEST_EARLY_WARNING_AT+set}" ]; then
    [ "$MHVTL" = 1 ] || die "TAPECTL_TEST_EARLY_WARNING_AT is set: a real-drive run with the test hook proves nothing. Unset it"
    die "TAPECTL_TEST_EARLY_WARNING_AT is set in the environment: the rehearsal sets it itself, for the first write only (--hook-at). Unset it"
fi

[ -n "${TAPECTL_BIN:-}" ] || die "TAPECTL_BIN is required: the tapectl under test (this script never builds one)"
[ -x "$TAPECTL_BIN" ] || die "TAPECTL_BIN=$TAPECTL_BIN is not an executable file"
TAPECTL_BIN="$(readlink -f "$TAPECTL_BIN")"
: "${FIXTURE_BIN:=$(dirname "$TAPECTL_BIN")/examples/short_seal_filler}"
[ -x "$FIXTURE_BIN" ] || die "FIXTURE_BIN=$FIXTURE_BIN is not an executable file (cargo build --example short_seal_filler, built with TAPECTL_BIN)"
: "${DAR_BIN:=$(command -v dar)}"
[ -x "$DAR_BIN" ] || die "DAR_BIN=$DAR_BIN is not an executable file"
TAPECTL_VERSION="$("$TAPECTL_BIN" --version 2>&1 | head -1)"
if [ "$MHVTL" = 0 ]; then
    # The run is the 1.6.0 candidate's: a binary before 1.6.0 cannot seal short at all.
    python3 - "$TAPECTL_VERSION" <<'PY' || die "TAPECTL_BIN reports '$TAPECTL_VERSION': the acceptance run is for tapectl 1.6.0 or later"
import re, sys
m = re.search(r"(\d+)\.(\d+)\.(\d+)", sys.argv[1])
sys.exit(0 if m and tuple(map(int, m.groups())) >= (1, 6, 0) else 1)
PY
fi

# ---------- the throwaway home; production's must be unreachable ----------
[ -n "${TAPECTL_HOME:-}" ] || die "TAPECTL_HOME is unset: name a fresh throwaway home (it must not exist yet, or be empty)"
case "$TAPECTL_HOME" in /*) ;; *) die "TAPECTL_HOME must be an absolute path (got '$TAPECTL_HOME')" ;; esac
canon() { readlink -m "$1"; }
HOME_C="$(canon "$TAPECTL_HOME")"
PROD_HOMES=("$HOME2_SVC_HOME" "$HOME2_SVC_HOME/.tapectl" "$HOME/.tapectl")
if [ -e /etc/tapectl/home ]; then
    POINTED="$(head -1 /etc/tapectl/home 2>/dev/null | tr -d '\r')" || die "/etc/tapectl/home exists but cannot be read: refusing without knowing production's home"
    [ -n "$POINTED" ] || die "/etc/tapectl/home is empty: refusing without knowing production's home"
    PROD_HOMES+=("$POINTED")
fi
SVC_DIR="$(getent passwd tapectl 2>/dev/null | cut -d: -f6 || true)"
[ -z "$SVC_DIR" ] || PROD_HOMES+=("$SVC_DIR" "$SVC_DIR/.tapectl")
for p in "${PROD_HOMES[@]}"; do
    pc="$(canon "$p")"
    case "$HOME_C/" in
        "$pc"/*) die "TAPECTL_HOME=$TAPECTL_HOME is (or is inside) production's home $p" ;;
    esac
    case "$pc/" in
        "$HOME_C"/*) die "TAPECTL_HOME=$TAPECTL_HOME contains production's home $p" ;;
    esac
done
if [ -e "$TAPECTL_HOME" ]; then
    [ -d "$TAPECTL_HOME" ] || die "TAPECTL_HOME=$TAPECTL_HOME exists and is not a directory"
    [ -z "$(ls -A "$TAPECTL_HOME")" ] || die "TAPECTL_HOME=$TAPECTL_HOME is not empty: every run starts from a fresh home"
fi

# ---------- nothing else on the drive ----------
if command -v systemctl >/dev/null && systemctl is-active --quiet tapectl-drive-poll.timer 2>/dev/null; then
    die "tapectl-drive-poll.timer is active: run 'sudo systemctl stop tapectl-drive-poll.timer' first (and start it again afterwards)"
fi

# ---------- the drive, through the canonical node (issue #334 idiom) ----------
TAPE_REAL="$(readlink -f "$TAPE_DEV")"; ST_NODE="$(basename "$TAPE_REAL")"
[ -d "/sys/class/scsi_tape/$ST_NODE" ] || die "$TAPE_DEV ($TAPE_REAL) is not an st tape node"
case "$ST_NODE" in nst[0-9]*) ;; *) die "$TAPE_DEV is $TAPE_REAL, a REWINDING node: give the by-id name ending -nst" ;; esac
SYS_ST="/sys/class/scsi_tape/$ST_NODE"
SG_ENTRIES=(); for e in "$SYS_ST/device/scsi_generic"/sg*; do [ -e "$e" ] && SG_ENTRIES+=("$(basename "$e")"); done
[ "${#SG_ENTRIES[@]}" -eq 1 ] || die "cannot resolve exactly one sg node for $TAPE_DEV (got ${SG_ENTRIES[*]:-none})"
DRIVE_SG="/dev/${SG_ENTRIES[0]}"
DRIVE_SERIAL="$(tail -c +5 "$SYS_ST/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
[ -n "$DRIVE_SERIAL" ] || die "cannot read the drive serial from sysfs ($SYS_ST/device/vpd_pg80)"
SYS_MODEL="$(sed 's/[[:space:]]*$//' "$SYS_ST/device/model" 2>/dev/null || true)"
GEN_N="$(echo "$SYS_MODEL" | sed -n 's/.*Ultrium \([0-9]\{1,2\}\).*/\1/p; s/.*TD\([0-9]\{1,2\}\).*/\1/p' | head -1)"
[ "$GEN_N" = 6 ] || die "the drive's model '$SYS_MODEL' is not an LTO-6 drive: this run is the LTO-6 acceptance run"
if [ "$MHVTL" = 1 ]; then
    [ "$DRIVE_SERIAL" != "$TEST_DRIVE_SERIAL" ] || die "--mhvtl names the REAL drive $TEST_DRIVE_SERIAL: the rehearsal runs on mhvtl only"
    case "$DRIVE_SERIAL" in XYZZY_*) ;; *) die "--mhvtl: $TAPE_DEV has serial '$DRIVE_SERIAL', not an mhvtl drive (XYZZY_*)" ;; esac
else
    [ "$DRIVE_SERIAL" = "$TEST_DRIVE_SERIAL" ] || die "the drive's serial is '$DRIVE_SERIAL', not the validation drive $TEST_DRIVE_SERIAL (give --mhvtl for the rehearsal)"
    [ -n "$LOSE_SERIAL" ] || die "--i-will-lose-the-cartridge $TEST_CARTRIDGE is required: this run overwrites the loaded cartridge"
    [ "$LOSE_SERIAL" = "$TEST_CARTRIDGE" ] || die "--i-will-lose-the-cartridge names '$LOSE_SERIAL': this run is for the test cartridge $TEST_CARTRIDGE only"
fi

# Single-drive rule (#9). The real drive shares the harness lock with the gate, the suite and
# the profile; an mhvtl drive takes its own, so a rehearsal never waits on the gate's other drives.
if [ "$MHVTL" = 1 ]; then TAPE_LOCK="/tmp/tapectl-tape.lock.$ST_NODE"; else TAPE_LOCK="/tmp/tapectl-tape.lock"; fi
exec 9>"$TAPE_LOCK"
flock -n 9 || die "another process holds the tape lock ($TAPE_LOCK)"

# ---------- consent: the cartridge is known by its chip serial, never its sticker ----------
# The ANCHORED match on purpose (lto-profile.sh, lifecycle-suite.sh): a loose grep matches
# "Density vendor/serial number at last load" first.
read_serial() {
    sg_read_attr "$DRIVE_SG" 2>/dev/null | grep -iE '^[[:space:]]*medium serial number[[:space:]]*:' \
        | head -1 | sed 's/^[^:]*: *//' | tr -d ' \r'
}
SERIAL="$(read_serial || true)"
[ -n "$SERIAL" ] || die "sg_read_attr reported no medium serial number on $DRIVE_SG (is a cartridge loaded?)"
if [ "$MHVTL" = 0 ]; then
    [ "$SERIAL" = "$TEST_CARTRIDGE" ] || die "the loaded cartridge's chip serial is '$SERIAL', not the test cartridge $TEST_CARTRIDGE"
elif [ -n "$LOSE_SERIAL" ]; then
    [ "$SERIAL" = "$LOSE_SERIAL" ] || die "the loaded cartridge's chip serial is '$SERIAL', not '$LOSE_SERIAL'"
fi

# ---------- the run directory ----------
if [ -z "$OUT_DIR" ]; then
    if [ -d /scratch ]; then OUT_DIR="/scratch/tapectl-short-seal"; else OUT_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl/short-seal"; fi
fi
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
SAFE_SERIAL="$(printf '%s' "$SERIAL" | tr -c 'A-Za-z0-9_.-' '_')"
RUN="$OUT_DIR/ssa-$STAMP-$SAFE_SERIAL"
mkdir -p "$RUN" || die "cannot create $RUN"
NEED=$((4 * SLICE_BYTES + SIZE_A + 2 * SIZE_B + 2 * SIZE_Z + 2 * SIZE_F + 256 * MiB))
AVAIL="$(df --output=avail -B1 "$RUN" | tail -n 1 | tr -d ' ')"
[ "$AVAIL" -ge "$NEED" ] || die "$RUN has $((AVAIL / MiB)) MiB free; the sources, staging and restores need $((NEED / MiB)) MiB"
mkdir -p "$TAPECTL_HOME"
REBUILD_HOME="$RUN/rebuild-home"
STAGING="$RUN/staging"; SRC="$RUN/src"; WORK="$RUN/work"
mkdir -p "$STAGING" "$SRC" "$WORK" "$RUN/tmp"
CFG="$TAPECTL_HOME/config.toml"
DB="$TAPECTL_HOME/tapectl.db"

# The filler's one file is read N times, each read evicted behind it (util::DropBehind); on a
# tmpfs that costs no disk.
if [ -z "$FILLER_DIR" ] && [ -d /dev/shm ]; then
    SHM_FREE="$(df --output=avail -B1 /dev/shm 2>/dev/null | tail -n 1 | tr -d ' ')"
    if [ -n "$SHM_FREE" ] && [ "$SHM_FREE" -ge $((3 * SLICE_BYTES)) ] 2>/dev/null; then
        FILLER_DIR="/dev/shm/tapectl-ssa-$STAMP"
    fi
fi

# Every tapectl call: this run's home and config, the system home pointer switched off, and the
# test hook unset (the rehearsal's first write sets it on its own command line).
export TAPECTL_HOME_POINTER="$RUN/no-home-pointer"
TEE_OPT=(); if tee --output-error=warn </dev/null >/dev/null 2>&1; then TEE_OPT=(--output-error=warn); fi
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${TEE_OPT[@]}" "$RUN/run.log"; }
trap '' HUP

STEP=""; STEP_DIR="$RUN"
fail() {
    log "FAIL (step $STEP): $*"
    {
        echo "RESULT: FAIL"
        echo "step: $STEP"
        echo "why: $*"
        echo "The run stopped here. The cartridge is as this step left it: read the failure"
        echo "before anything writes it again. Everything is in $RUN."
        [ -z "$FILLER_DIR" ] || echo "The filler's slice file is kept in $FILLER_DIR (RAM, on a tmpfs): remove it once the failure is read."
    } > "$RUN/RESULT.txt"
    snapshot_drive "$STEP_DIR" "at-fail" || true
    cp -a "$DB" "$RUN/catalog-at-fail.db" 2>/dev/null || true
    echo "short-seal-acceptance: FAIL at step $STEP: $*" >&2
    echo "short-seal-acceptance: the run directory is $RUN" >&2
    exit 1
}

# ---------- the drive's state, read-only (lto-profile.sh's snapshot and page) ----------
# One LOG SENSE per page (#328); never 0x2E (read-to-clear TapeAlert: only tapectl's sweep
# reads it).
page() { # <dir> <tag> <page>
    sg_logs --page="$3" --maxlen=65532 --raw "$DRIVE_SG" > "$1/$2.page_$3.bin" 2>"$1/$2.page_$3.err" \
        && sg_logs --in="$1/$2.page_$3.bin" --raw --pdt=1 > "$1/$2.page_$3.txt" 2>&1
}
snapshot_drive() { # <dir> <tag>
    local d="$1/drive-$2" f
    mkdir -p "$d"
    cat "$SYS_ST/options" > "$d/options.txt" 2>&1 || true
    for f in "$SYS_ST"/stats/*; do
        [ -f "$f" ] && printf '%s\t%s\n' "$(basename "$f")" "$(cat "$f" 2>/dev/null)" >> "$d/stats.tsv"
    done
    sg_read_attr "$DRIVE_SG" > "$d/mam.txt" 2>&1 || true
    local pg
    for pg in 0x02 0x03 0x0c 0x17; do page "$d" snap "$pg" || true; done
    mt -f "$TAPE_DEV" status > "$d/mt-status.txt" 2>&1 || true
}

# run <name> <expected rc> <cmd...>: stdout to <name>.out, stderr to <name>.err and the terminal
# (tapectl's progress), the command line to commands.log. Sets RC; FAILs on any other rc.
RC=0
run() {
    local name="$1" want="$2"; shift 2
    printf '+ %q' "$@" >> "$RUN/commands.log"; echo >> "$RUN/commands.log"
    log "  \$ $*"
    set +e
    { "$@" 2>&1 1>&3 3>&- | tee "${TEE_OPT[@]}" "$STEP_DIR/$name.err" >&2; } 3>"$STEP_DIR/$name.out"
    RC="${PIPESTATUS[0]}"
    set -e
    echo "$RC" > "$STEP_DIR/$name.rc"
    if [ "$want" != "any" ] && [ "$RC" != "$want" ]; then
        fail "$name exited $RC, expected $want (see $STEP_DIR/$name.err and .out)"
    fi
}
tctl() {
    env -u TAPECTL_TEST_EARLY_WARNING_AT "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null
}
tctl_hooked() { # the rehearsal's first write: the test hook on this one command line
    env TAPECTL_TEST_EARLY_WARNING_AT="$HOOK_AT" "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null
}
rctl() { # the rebuild's bare home (no config, no drive configured: the heir's case)
    env -u TAPECTL_TEST_EARLY_WARNING_AT "$TAPECTL_BIN" --home "$REBUILD_HOME" "$@" </dev/null
}

begin_step() { # <number> <name> <what>
    STEP="$1 $2"
    STEP_DIR="$RUN/$1-$2"
    mkdir -p "$STEP_DIR"
    log "=== step $1: $2 -- $3"
    snapshot_drive "$STEP_DIR" before
    STEP_T0="$(date +%s)"
}
end_step() {
    snapshot_drive "$STEP_DIR" after
    local secs=$(( $(date +%s) - STEP_T0 ))
    log "=== step $STEP: PASS ($((secs / 3600))h $(((secs % 3600) / 60))m $((secs % 60))s)"
    printf '%s\tPASS\t%s\n' "$STEP" "$secs" >> "$RUN/steps.tsv"
}

# ---------- read-only catalog queries (python's sqlite3, ?mode=ro; the suite's idiom) ----------
dbq() { # <db> <sql> [params...]: rows, tab-separated
    python3 - "$@" <<'PY'
import sqlite3, sys
db, sql, args = sys.argv[1], sys.argv[2], sys.argv[3:]
con = sqlite3.connect("file:%s?mode=ro" % db, uri=True, timeout=30)
for row in con.execute(sql, args):
    print("\t".join("" if v is None else str(v) for v in row))
PY
}
json_get() { # <file> <python expression on d>
    python3 - "$1" "$2" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
v = eval(sys.argv[2], {"d": d})
print("" if v is None else (json.dumps(v) if isinstance(v, (list, dict)) else v))
PY
}

# assert_identical <src> <restored>: lifecycle-suite.sh's. `diff -r` without --no-dereference
# follows symlinks and fails on a dangling one; both the diff and the tree checksum must agree.
tree_checksum() {
    (
        cd "$1" || exit 1
        find . \( -type f -o -type l \) | LC_ALL=C sort | while IFS= read -r p; do
            if [ -L "$p" ]; then
                printf 'LINK\t%s\t%s\n' "$p" "$(readlink "$p")"
            else
                printf 'FILE\t%s\t%s\n' "$p" "$(sha256sum "$p" | awk '{print $1}')"
            fi
        done | sha256sum | awk '{print $1}'
    )
}
assert_identical() { # <src> <restored> <what>
    diff -r --no-dereference "$1" "$2" > "$STEP_DIR/diff-$3.txt" 2>&1 \
        || fail "$3: the restored tree differs from its source (diff-$3.txt)"
    local h1 h2
    h1="$(tree_checksum "$1")"; h2="$(tree_checksum "$2")"
    [ "$h1" = "$h2" ] || fail "$3: tree checksum $h2 restored, $h1 at the source"
    log "  $3: identical to its source (tree checksum $h1)"
}

# ---------- the sources: real, small, and one filler ----------
random_file() { head -c "$2" /dev/urandom > "$1"; }
make_sources() {
    mkdir -p "$SRC/a/nested/deeper" "$SRC/b/raw" "$SRC/z" "$SRC/m"
    echo "plain content $STAMP" > "$SRC/a/plain.txt"
    printf '\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e content\n' > "$SRC/a/nested/ünïcödé 日本語.txt"
    : > "$SRC/a/empty.bin"
    echo "deep leaf" > "$SRC/a/nested/deeper/leaf.txt"
    ln -s plain.txt "$SRC/a/link-ok"
    random_file "$SRC/a/blob.bin" "$SIZE_A"
    local i n=6 each=$((SIZE_B / 6))
    for i in $(seq 1 "$n"); do random_file "$SRC/b/raw/img-$i.raw" "$each"; done
    echo "photos manifest $STAMP" > "$SRC/b/index.txt"
    random_file "$SRC/z/tail.bin" "$SIZE_Z"
    echo "the unit after the filler, cut with it $STAMP" > "$SRC/z/README.txt"
    random_file "$SRC/m/filler.bin" "$SIZE_F"
}

extract_restore_sh() { # <dir>: RESTORE.sh off the loaded tape, as File 1 tells an heir to
    mkdir -p "$1"
    mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
    mt -f "$TAPE_DEV" setblk 524288 || fail "mt setblk failed"
    mt -f "$TAPE_DEV" fsf 2 || fail "mt fsf 2 failed"
    dd if="$TAPE_DEV" bs=512k 2>"$1/dd.err" | tr -d '\0' > "$1/RESTORE.sh" || fail "dd of File 2 failed"
    chmod +x "$1/RESTORE.sh"
    bash -n "$1/RESTORE.sh" || fail "the RESTORE.sh read off the tape does not parse"
}
heir() { # <name> <expected rc> <RESTORE.sh args...>
    local name="$1" want="$2"; shift 2
    run "$name" "$want" env TAPE_DEVICE="$TAPE_DEV" TMPDIR="$RUN/tmp" "$HEIR_DIR/RESTORE.sh" "$@"
}

# ============================================================
print_plan | tee "$RUN/PLAN.txt"
log "run $RUN"
log "drive $DRIVE_SERIAL ($SYS_MODEL) at $TAPE_DEV ($TAPE_REAL, $DRIVE_SG); cartridge chip serial $SERIAL"
log "mode: $([ "$MHVTL" = 1 ] && echo "mhvtl rehearsal" || echo "REAL DRIVE")"
log "tapectl: $TAPECTL_BIN ($TAPECTL_VERSION); fixture: $FIXTURE_BIN"
log "home: $TAPECTL_HOME; rebuild home: $REBUILD_HOME; staging: $STAGING; filler file in: ${FILLER_DIR:-staging}"
{
    echo "started=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "host=$(hostname)"
    echo "mode=$([ "$MHVTL" = 1 ] && echo mhvtl || echo real)"
    echo "device=$TAPE_DEV"; echo "device_real=$TAPE_REAL"; echo "sg=$DRIVE_SG"
    echo "drive_serial=$DRIVE_SERIAL"; echo "drive_model=$SYS_MODEL"; echo "cartridge_serial=$SERIAL"
    echo "tapectl=$TAPECTL_BIN"; echo "tapectl_version=$TAPECTL_VERSION"
    echo "tapectl_sha256=$(sha256sum "$TAPECTL_BIN" | awk '{print $1}')"
    echo "fixture=$FIXTURE_BIN"
    echo "dar=$("$DAR_BIN" --version 2>&1 | head -1)"
    echo "age=$(age --version 2>&1 | head -1)"
    echo "tar=$(tar --version 2>&1 | head -1)"
    echo "script_commit=$(git -C "$SCRIPT_DIR" describe --always --dirty 2>/dev/null || echo unknown)"
    echo "capacity_registered=$CAPACITY"; echo "post_warning_bytes=$POST_WARNING"; echo "slice_size=$SLICE_SIZE"
} > "$RUN/meta.txt"
: > "$RUN/steps.tsv"

# ------------------------------------------------------------
begin_step 1 setup "a throwaway home, four units staged, the filler's rows (no tape)"
run init 0 tctl init --operator "$OPERATOR" --no-escrow
run escrow 0 tctl key generate --escrow
ESCROW_SECRET="$(grep -oE 'AGE-SECRET-KEY-1[A-Z0-9]+' "$STEP_DIR/escrow.out" "$STEP_DIR/escrow.err" | head -1 | sed 's/^[^:]*://')"
[ -n "$ESCROW_SECRET" ] || fail "key generate --escrow printed no secret"
mkdir -p "$RUN/keys"; umask_old="$(umask)"; umask 077
printf '%s\n' "$ESCROW_SECRET" > "$RUN/keys/escrow.key"
umask "$umask_old"
chmod 600 "$STEP_DIR/escrow.out" "$STEP_DIR/escrow.err"
run cfg-dar 0 tctl config set dar.binary "\"$DAR_BIN\""
run cfg-slice 0 tctl config set defaults.slice_size "\"$SLICE_SIZE\""
run cfg-copies 0 tctl config set defaults.min_copies 1
run cfg-staging 0 tctl config set staging.directory "\"$STAGING\""
run backend 0 tctl backend add --name ssa --device-tape "$TAPE_DEV" --generation LTO-6
run cfg-pwb 0 tctl config set 'backends.lto[0].post_warning_bytes' "\"$POST_WARNING\""
[ -z "$ENOSPC_BUFFER" ] || run cfg-reserve 0 tctl config set 'backends.lto[0].enospc_buffer' "\"$ENOSPC_BUFFER\""
run config-check 0 tctl config check
cp "$CFG" "$RUN/config.toml"
run tenant 0 tctl tenant add "$TENANT"
log "  making the sources under $SRC"
make_sources
run unit-a 0 tctl unit init "$SRC/a" --tenant "$TENANT" --name "$UNIT_A"
run unit-b 0 tctl unit init "$SRC/b" --tenant "$TENANT" --name "$UNIT_B"
run unit-z 0 tctl unit init "$SRC/z" --tenant "$TENANT" --name "$UNIT_Z"
run unit-f 0 tctl unit init "$SRC/m" --tenant "$TENANT" --name "$UNIT_F"
for u in "$UNIT_A" "$UNIT_B" "$UNIT_Z" "$UNIT_F"; do
    run "snapshot-$u" 0 tctl snapshot create "$u"
done
# Every real unit is staged before the filler has its rows: stage create never sees its size.
for u in "$UNIT_A" "$UNIT_B" "$UNIT_Z" "$UNIT_F"; do
    run "stage-$u" 0 tctl --yes stage create "$u"
done
run cartridge 0 tctl cartridge register --barcode "$SERIAL" --generation LTO-6 \
    --capacity "$CAPACITY" --serial "$SERIAL" --notes "short-seal acceptance run $STAMP"
FILL_ARGS=(--home "$TAPECTL_HOME" --unit "$UNIT_F" --plan-end "$CAPACITY" --allowance "$ALLOWANCE")
run filler-plan 0 "$FIXTURE_BIN" fill "${FILL_ARGS[@]}" --dry-run
[ -z "$FILLER_DIR" ] || FILL_ARGS+=(--move-to "$FILLER_DIR")
run filler 0 "$FIXTURE_BIN" fill "${FILL_ARGS[@]}"
# KEY=VALUE lines: FILLER_SLICES, TEMPLATE_PADDED_BYTES, FILLER_PADDED_BYTES, ...
while IFS='=' read -r k v; do
    case "$k" in [A-Z_]*) printf -v "FX_$k" '%s' "$v" ;; esac
done < "$STEP_DIR/filler.out"
log "  filler: ${FX_FILLER_SLICES:-?} slices of ${FX_TEMPLATE_PADDED_BYTES:-?} bytes on tape, ${FX_FILLER_PADDED_BYTES:-?} in all; the plan ends near ${FX_PLAN_END_ESTIMATE_BYTES:-?} of $CAPACITY"
if [ "$MHVTL" = 1 ] && [ -z "$HOOK_AT" ]; then
    # 98% of the way through the filler, from an estimate of what precedes it (the fixed files
    # are inside the allowance): past the fill ceiling as the real warning is (2.502 TB of a
    # filler ending near 2.548), inside a 3-block slice, far below mhvtl's 500 MB medium end.
    HOOK_AT=$(( FX_UNITS_BEFORE_PADDED_BYTES + 2 * MiB + FX_FILLER_PADDED_BYTES * 98 / 100 ))
fi
[ "$MHVTL" = 0 ] || log "  rehearsal: TAPECTL_TEST_EARLY_WARNING_AT=$HOOK_AT for the first write only"
run staging-status any tctl --json staging status
run plan any tctl volume plan
end_step

# ------------------------------------------------------------
begin_step 2 init "the cartridge erased from this home, then volume init $LABEL1"
# Whatever the cartridge holds (an earlier run's volume, sealed or not) belongs to another home.
# This home's own `cartridge erase` identifies it, asks for the label File 0 names, erases and
# reads the tape back, and records it: then `volume init` needs no --force (item 57).
run erase-probe 0 tctl --json cartridge erase --device "$TAPE_DEV" --dry-run
PROBE="$(json_get "$STEP_DIR/erase-probe.out" 'd["outcome"]')"
CONFIRM="$(json_get "$STEP_DIR/erase-probe.out" 'd.get("confirm_label")')"
log "  the cartridge as found: $PROBE (its confirm label: ${CONFIRM:-none})"
INIT_FORCE=()
case "$PROBE" in
    would_erase)
        [ -n "$CONFIRM" ] || fail "cartridge erase --dry-run names no label to confirm"
        run erase 0 tctl --json cartridge erase --device "$TAPE_DEV" --confirm-label "$CONFIRM"
        [ "$(json_get "$STEP_DIR/erase.out" 'd["outcome"]')" = erased ] || fail "cartridge erase did not report erased"
        ;;
    nothing_to_erase)
        # Blank, or File 0 EMPTY: nothing to erase, and nothing this catalog recorded proves
        # it blank, so init takes the consent this run was given (--i-will-lose-the-cartridge,
        # or --mhvtl) as --force. --force never overrides a sealed tape (ADR-0003).
        INIT_FORCE=(--force)
        ;;
    *) fail "cartridge erase --dry-run reported '$PROBE'" ;;
esac
run init 0 tctl --json volume init "$LABEL1" --device "$TAPE_DEV" "${INIT_FORCE[@]}"
GOT_CAP="$(json_get "$STEP_DIR/init.out" 'd["medium"]["capacity_bytes"]')"
[ "$GOT_CAP" = "$CAPACITY" ] || fail "volume init stored capacity $GOT_CAP, not the registered $CAPACITY: the plan bound would not reach the warning"
end_step

# ------------------------------------------------------------
begin_step 3 write "volume write $LABEL1 to the early warning: the short seal and item 114's readback"
if [ "$MHVTL" = 1 ]; then
    run write 0 tctl_hooked --json --yes volume write "$LABEL1" --device "$TAPE_DEV"
else
    run write 0 tctl --json --yes volume write "$LABEL1" --device "$TAPE_DEV"
fi
ENDING="$(json_get "$STEP_DIR/write.out" 'd["ending"]')"
K="$(json_get "$STEP_DIR/write.out" 'd["ended_after"]')"
CUT="$(json_get "$STEP_DIR/write.out" '" ".join(sorted(d["cut_units"]))')"
log "  ending=$ENDING ended_after=$K cut: $CUT"
[ "$ENDING" = "short" ] || fail "the write sealed '$ENDING', not short: the warning never came inside the plan"
case "$K" in ''|*[!0-9]*) fail "the write reported ended_after '$K'" ;; esac
[ "$CUT" = "$UNIT_F $UNIT_Z" ] || fail "the cut units are '$CUT', expected '$UNIT_F $UNIT_Z'"
grep -F "sealed short" "$STEP_DIR/write.err" >/dev/null || fail "the write's stderr does not report the short seal"
VOL_ROW="$(dbq "$DB" "SELECT status, ended_after, observed_condition FROM volumes WHERE label = ?" "$LABEL1")"
[ "$VOL_ROW" = "$(printf 'sealed\t%s\tok' "$K")" ] || fail "volumes row for $LABEL1 is '$VOL_ROW', expected sealed, ended_after $K, ok"
WRITES="$(dbq "$DB" "SELECT u.name || '=' || w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
    JOIN snapshots s ON s.id = w.snapshot_id JOIN units u ON u.id = s.unit_id WHERE v.label = ? ORDER BY u.name" "$LABEL1" | tr '\n' ' ')"
[ "$WRITES" = "$UNIT_A=completed $UNIT_B=completed $UNIT_F=cut $UNIT_Z=cut " ] || fail "write rows are '$WRITES'"
dbq "$DB" "SELECT e.timestamp, e.field, e.new_value, e.details FROM events e JOIN volumes v ON v.id = e.entity_id
    WHERE e.action = 'write_early_warning' AND e.entity_type = 'volume' AND v.label = ?" "$LABEL1" > "$STEP_DIR/early-warning-event.tsv"
[ -s "$STEP_DIR/early-warning-event.tsv" ] || fail "no write_early_warning event for $LABEL1"
[ "$(cut -f3 "$STEP_DIR/early-warning-event.tsv" | tail -1)" = "$K" ] || fail "the write_early_warning event's ended_after is not $K"
log "  write_early_warning: $(tail -1 "$STEP_DIR/early-warning-event.tsv")"
dbq "$DB" "SELECT action, details FROM events WHERE action IN ('write_feed_ratio', 'write_capacity_used', 'write_completed')
    ORDER BY id" > "$STEP_DIR/write-events.tsv" || true
log "  feed ratio: $(grep '^write_feed_ratio' "$STEP_DIR/write-events.tsv" | head -1 | cut -f2 || true)"
# Item 112: the cut sets stay staged and whole. The release after the write is all-or-nothing
# over the units written (staging::release), so the cut units, with no copy, keep every unit's
# staging; step 7 releases them one by one.
STAGED="$(dbq "$DB" "SELECT u.name || '=' || ss.status FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id ORDER BY u.name" | tr '\n' ' ')"
log "  stage sets after the write: $STAGED"
[ "$STAGED" = "$UNIT_A=staged $UNIT_B=staged $UNIT_F=staged $UNIT_Z=staged " ] || fail "stage sets after the write are '$STAGED', expected all four still staged (a cut unit has no copy, so the release keeps them)"
dbq "$DB" "SELECT sl.staging_path FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id WHERE u.name IN (?, ?)
    GROUP BY sl.staging_path" "$UNIT_F" "$UNIT_Z" > "$STEP_DIR/cut-staged-paths.txt"
while IFS= read -r p; do
    [ -f "$p" ] || fail "the cut set's staged file $p is gone"
done < "$STEP_DIR/cut-staged-paths.txt"
run volume-info 0 tctl volume info "$LABEL1"
cp -a "$DB" "$RUN/catalog-after-write.db"
end_step

# ------------------------------------------------------------
begin_step 4 verify "volume verify --full, then RESTORE.sh --info and --verify with the real tools"
run verify 0 tctl volume verify "$LABEL1" --full --device "$TAPE_DEV"
HEIR_DIR="$WORK/heir"
extract_restore_sh "$HEIR_DIR"
heir info 0 --info
grep -F "Verdict: SEALED" "$STEP_DIR/info.out" >/dev/null || fail "RESTORE.sh --info does not say Verdict: SEALED"
grep -F "The tape ended early, after file $K" "$STEP_DIR/info.out" >/dev/null || fail "RESTORE.sh --info does not say the tape ended early after file $K"
heir heir-verify 0 --verify
grep -F "VERIFY: PASS" "$STEP_DIR/heir-verify.out" >/dev/null || fail "RESTORE.sh --verify did not PASS"
NOT_ON_TAPE="$(grep -c "^NOT ON TAPE" "$STEP_DIR/heir-verify.out" || true)"
[ "$NOT_ON_TAPE" -ge 1 ] || fail "RESTORE.sh --verify marks no file NOT ON TAPE on a short tape"
log "  RESTORE.sh --verify: PASS, $NOT_ON_TAPE planned file(s) NOT ON TAPE after file $K"
end_step

# ------------------------------------------------------------
begin_step 5 restore "$UNIT_B whole, by tapectl and by RESTORE.sh; the cut unit refused"
run restore-tapectl 0 tctl restore unit --unit "$UNIT_B" --from "$LABEL1" --to "$WORK/restore-tapectl-b" --device "$TAPE_DEV"
assert_identical "$SRC/b" "$WORK/restore-tapectl-b" tapectl-restore
TENANT_KEY="$TAPECTL_HOME/keys/$TENANT-primary.age.key"
[ -f "$TENANT_KEY" ] || fail "the tenant key $TENANT_KEY is missing"
heir restore-heir 0 --restore --unit "$UNIT_B" --key "$TENANT_KEY" --to "$WORK/restore-heir-b"
assert_identical "$SRC/b" "$WORK/restore-heir-b" heir-restore
heir restore-heir-cut any --restore --unit "$UNIT_Z" --key "$TENANT_KEY" --to "$WORK/restore-heir-z"
[ "$RC" != 0 ] || fail "RESTORE.sh restored the cut unit $UNIT_Z from a tape that does not hold it whole"
grep -F "not on this tape whole" "$STEP_DIR/restore-heir-cut.out" "$STEP_DIR/restore-heir-cut.err" >/dev/null \
    || fail "RESTORE.sh's refusal of $UNIT_Z does not say it is not on this tape whole"
run restore-tapectl-cut any tctl restore unit --unit "$UNIT_Z" --from "$LABEL1" --to "$WORK/restore-tapectl-z" --device "$TAPE_DEV"
[ "$RC" != 0 ] || fail "tapectl restored the cut unit $UNIT_Z from $LABEL1"
end_step

# ------------------------------------------------------------
begin_step 6 rebuild "catalog rebuild --from-volume into a second home, compared with the writer's rows"
OPERATOR_KEY="$TAPECTL_HOME/keys/$OPERATOR-primary.age.key"
[ -f "$OPERATOR_KEY" ] || fail "the operator key $OPERATOR_KEY is missing"
mkdir -p "$REBUILD_HOME"
run rebuild 0 rctl catalog rebuild --from-volume --device "$TAPE_DEV" --key "$OPERATOR_KEY"
run compare 0 "$FIXTURE_BIN" compare --home "$TAPECTL_HOME" --rebuilt "$REBUILD_HOME" --label "$LABEL1"
tee -a "$RUN/run.log" < "$STEP_DIR/compare.out"
end_step

# ------------------------------------------------------------
begin_step 7 rewrite "the cut unit written whole onto the same cartridge as $LABEL2"
# `volume write` writes every staged set, so before the next one only the cut real unit may be
# staged. The whole units have their copy (min_copies = 1): a bare clean releases them while a
# cut sibling stays (item 112). The filler, cut and with no copy anywhere, needs --force.
run release-whole 0 tctl staging clean --unit "$UNIT_A" --unit "$UNIT_B"
run release-filler 0 tctl staging clean --unit "$UNIT_F" --force
STAGED="$(dbq "$DB" "SELECT u.name || '=' || ss.status FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id ORDER BY u.name" | tr '\n' ' ')"
log "  stage sets before the second write: $STAGED"
[ "$STAGED" = "$UNIT_A=cleaned $UNIT_B=cleaned $UNIT_F=cleaned $UNIT_Z=staged " ] || fail "stage sets before the second write are '$STAGED', expected only $UNIT_Z staged"
# Erasing the cartridge gives up the only copy of the whole units on it: the retire floor
# (ADR-0008 Tier 3) names the one command that does that on purpose.
run reclaim-a 0 tctl snapshot mark-reclaimable "$UNIT_A" --version 1 --force
run reclaim-b 0 tctl snapshot mark-reclaimable "$UNIT_B" --version 1 --force
run retire 0 tctl --yes volume retire "$LABEL1"
run erase 0 tctl cartridge erase --device "$TAPE_DEV" --confirm-label "$LABEL1"
run init2 0 tctl --json volume init "$LABEL2" --device "$TAPE_DEV"
run write2 0 tctl --json --yes volume write "$LABEL2" --device "$TAPE_DEV"
ENDING2="$(json_get "$STEP_DIR/write2.out" 'd["ending"]')"
[ "$ENDING2" = "full" ] || fail "the second write sealed '$ENDING2', not full"
WRITES2="$(dbq "$DB" "SELECT u.name || '=' || w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
    JOIN snapshots s ON s.id = w.snapshot_id JOIN units u ON u.id = s.unit_id WHERE v.label = ? ORDER BY u.name" "$LABEL2" | tr '\n' ' ')"
[ "$WRITES2" = "$UNIT_Z=completed " ] || fail "$LABEL2's write rows are '$WRITES2', expected only $UNIT_Z completed"
run verify2 0 tctl volume verify "$LABEL2" --full --device "$TAPE_DEV"
run restore-z 0 tctl restore unit --unit "$UNIT_Z" --from "$LABEL2" --to "$WORK/restore-tapectl-z2" --device "$TAPE_DEV"
assert_identical "$SRC/z" "$WORK/restore-tapectl-z2" cut-unit-restore
end_step

# ------------------------------------------------------------
cp -a "$DB" "$RUN/catalog-at-end.db"
{
    echo "RESULT: PASS"
    echo "every step passed; ended_after $K on $LABEL1; the cut unit $UNIT_Z whole on $LABEL2"
    echo "the cartridge now holds $LABEL2 and belongs to $TAPECTL_HOME"
} > "$RUN/RESULT.txt"
[ -z "$FILLER_DIR" ] || rm -rf "$FILLER_DIR"
log "RESULT: PASS. $RUN"
log "The cartridge holds $LABEL2 (the cut unit only), in the throwaway home. Before it is a test"
log "cartridge again: tapectl --home $TAPECTL_HOME volume retire $LABEL2, then cartridge erase."
[ "$MHVTL" = 1 ] || log "Start the drive-poll timer again: sudo systemctl start tapectl-drive-poll.timer"
