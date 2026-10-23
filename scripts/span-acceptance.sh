#!/usr/bin/env bash
# span-acceptance.sh -- the 1.7.0 acceptance run of spanning on a real drive (issue #478;
# ADR-0012 items 137 and 139). The procedure around it is docs/lto6-validation-checklist.md,
# "Spanning on the real drive (1.7.0 acceptance)".
#
# WHAT IT PROVES (item 137 as item 139 amends it), on the one test cartridge used as two
# volumes in turn:
#   1 tape 1: a filler up to the fill ceiling, then a real unit that allows spanning, written
#     with --keep-staging and ended by the drive's REAL early warning inside that unit. Its
#     write row is `range`, every written position at or before ended_after; what was written
#     past the warning reads back (item 114); File 0 and the short seal state
#     requires = ["short-seal", "spanning"] (item 124).
#   2 RESTORE.sh's default mode, read off tape 1, spools tape 1's range into --scratch as
#     ciphertext and stops at its swap prompt (item 132). The script HOLDS that run: its input
#     is a FIFO the script answers in step 5, and it waits with no timeout and no tape device
#     open (the coordinator's ruling in #478's build plan, section 9).
#   3 catalog rebuild --from-volume of tape 1 into a second home: a `range` row, the version
#     staged, the to-do naming the slices tape 1 lacks (item 130).
#   4 tape 2: the cartridge erased and initialised again in the writing catalog. That marks
#     tape 1 `erased` (binding records the displacement), so tape 2 is planned from slice 1 and
#     holds the unit WHOLE, written from the kept staging (item 139).
#   5 the held RESTORE.sh run is answered with tape 2 in the drive. It takes tape 2 because
#     its front index lists slice K1+1 (R8, its seal read first), finishes from scratch plus
#     tape 2, and the unit restores identical to a sha256 manifest of its source.
#   6 the closing text names the cleanup. Nothing is erased automatically.
#
# HOW TAPE 1 IS FILLED. As 1.6.0's run (item 115): a filler unit staged normally from a
# source a little larger than one slice, then given N slice rows naming its one real staged
# slice (examples/short_seal_filler.rs, `fill`). Here the filler is the whole unit and ends
# at the fill ceiling: with a unit that may span queued, whole units stop at the ceiling and
# the unit that may span is planned last, past the plan bound to its end (items 83 and 123),
# so the early warning ends it on the tape. The cartridge keeps the generation table's 2.5 TB
# (production's figure): the ceiling is 2.425 TB, the warning came near 2.502 TB on this
# cartridge (2026-10-21), so about 77 GB of a 100 GB unit is on tape 1. `fill --plan-end` is
# the plan's end, and the plan holds the unit that may span to its end, so the end given is
# the ceiling plus that unit's size.
#
# THE UNIT THAT MAY SPAN is a COPY of --span-source (the operator's real data on home2) in
# the work directory: `unit init` writes .tapectl-unit.toml into the directory it registers,
# and this run never writes the operator's data. Any .tapectl-unit.toml in the copy (another
# home's unit) is removed from the copy before it is registered. The restore is compared with
# a sha256 manifest of the copy taken after it was staged. `defaults.global_excludes` is
# emptied in the throwaway config, so every file of the copy is archived.
#
# THE TAPE-2 ROUTE (item 139). Tape 1 cannot be given up the 1.6.0 way: `cartridge erase`
# refuses a volume the catalog still holds, and `volume retire` meets item 128's floor (tape
# 1 is the only holder of slices 1..K1 of a version not yet current). Step 4 shows both
# refusals, then writes one filemark at the start of the tape by hand (`mt weof 1`, the short
# erase `cartridge erase` itself does), so File 0 reads EMPTY, and runs `volume init --force`
# (an EMPTY File 0 that nothing in this catalog proves blank needs --force; the consent is
# --i-will-lose-the-cartridge, or --mhvtl). The cartridge's chip serial matches its row, so
# binding marks tape 1 `erased`. `volume init --help` names this erase for a sealed cartridge.
#
# A FAIL STOPS THE RUN and leaves the cartridge as the failed step left it: no later step, no
# exit trap, ever writes the medium a failed step measured (#420's lost readback). The exit
# trap only ends the held RESTORE.sh run, which writes nothing to the medium. The run
# directory keeps every command's output, the catalog, and the drive's sysfs options and
# counters, MAM and log pages 0x02 0x03 0x0c 0x17 before and after every step.
#
# REFUSES unless every one of these holds (exit 2, nothing touched):
#   - TAPECTL_HOME is set, absent or empty, and none of production's homes: /etc/tapectl/home's
#     target, the tapectl service user's home, /srv/archive_meta/tapectl (home2.profile), or
#     this account's ~/.tapectl;
#   - the drive's serial is HUJ808A5L4 and the cartridge's chip serial is EW7VWMVKF6, named
#     again with --i-will-lose-the-cartridge (the barcode is a sticker; ADR-0012) -- or --mhvtl,
#     on an mhvtl drive, never that one;
#   - TAPECTL_TEST_EARLY_WARNING_AT is unset (a real run with the test hook proves nothing);
#   - tapectl-drive-poll.timer is not active, and the tape lock is free;
#   - on the real drive, --span-source names the unit's source, large enough to reach past the
#     drive's warning; the work directory has room for its copy, its staging, the scratch and
#     the restore, and the run directory for the rest.
#
# --mhvtl is the rehearsal on a virtual drive (vm-desk1): the same six steps, a generated
# source of 48 MiB, 1 MiB slices, the drive's capacity_override at 200 MB (the harnesses'
# way to size a virtual tape, so `volume plan` and `volume init` agree), and the store's test
# hook, TAPECTL_TEST_EARLY_WARNING_AT, inside the unit that may span for tape 1's write only:
# mhvtl loses data past its medium's end instead of warning.
#
# EXIT: 0 every step passed; 1 a step FAILED (the run stopped there; the cartridge is as that
# step left it); 2 a precondition refused (nothing was touched).
set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"

TEST_DRIVE_SERIAL="HUJ808A5L4"
TEST_CARTRIDGE="EW7VWMVKF6"
# contrib/hosts/home2.profile: SVC_HOME_WANT and STAGING_DIR, production's home and staging.
HOME2_SVC_HOME="/srv/archive_meta/tapectl"
HOME2_STAGING="/srv/acache/tapectl-staging"
# Where this cartridge's early warning came on the HP LTO-6 (docs/runs/2026-10-21-1.6.0-acceptance.md).
WARNING_AT_MEASURED=2502136692736

LABEL1="SPN-0001"
LABEL2="SPN-0002"
OPERATOR="spn-op"
TENANT="spn"
UNIT_F="spn-filler"
UNIT_S="spn-span"
PROMPT="Load the cartridge that holds slice "

usage() {
    cat <<'USAGE'
usage: TAPECTL_HOME=<fresh dir> TAPECTL_BIN=<tapectl> [FIXTURE_BIN=<short_seal_filler>] \
       scripts/span-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
           --i-will-lose-the-cartridge EW7VWMVKF6 --span-source DIR --work DIR [options]
       scripts/span-acceptance.sh --mhvtl --device /dev/tape/by-id/scsi-XYZZY_A3-nst [options]
       scripts/span-acceptance.sh --print-plan [--mhvtl]

The 1.7.0 acceptance run of spanning: a unit that may span cut by the drive's early warning
on tape 1, RESTORE.sh holding tape 1's range in scratch, a rebuild of tape 1, the same
cartridge written again as tape 2, and the held restore finished from it. OVERWRITES THE
CARTRIDGE, twice.

  --device PATH                  Required. The drive's st node, by-id spelling. No default.
  --i-will-lose-the-cartridge S  Required on the real drive: EW7VWMVKF6, checked against MAM.
  --mhvtl                        The rehearsal on an mhvtl drive (the test hook, small sizes).
  --span-source DIR              The unit that may span: a directory COPIED into the work
                                 directory (never written). Required on the real drive, about
                                 100 GB (at least ~90 GB). --mhvtl generates one by default.
  --work DIR                     Where the copy, staging, the scratch and the restore go:
                                 absent or empty, with room for about 4 x the source. Default
                                 <run dir>/work. On home2, a directory on acache.
  --out DIR                      Where the run directory goes. Default /scratch/tapectl-span
                                 where /scratch exists, else ~/.cache/tapectl/span.
  --warning-at BYTES             Where this cartridge's early warning came, for the size
                                 checks (default 2502136692736, the 2026-10-21 run).
  --hook-at BYTES                --mhvtl only: TAPECTL_TEST_EARLY_WARNING_AT for tape 1's write
                                 (default: three quarters into the unit that may span).
  --filler-dir DIR               Where the filler's one slice file is read from (default: a
                                 directory on /dev/shm when it has room, else staging).
  --print-plan                   Print what the operator must do and the drive time, and exit.

  TAPECTL_HOME (env)             Required: the throwaway home, absent or empty.
  TAPECTL_BIN (env)              Required: the tapectl under test (1.7.0 or later on the real
                                 drive).
  FIXTURE_BIN (env)              The short_seal_filler example built with it. Default:
                                 <dir of TAPECTL_BIN>/examples/short_seal_filler.
  DAR_BIN (env)                  dar for the throwaway config. Default: dar on PATH.
USAGE
}

die() { echo "span-acceptance: REFUSED: $*" >&2; exit 2; }

TAPE_DEV=""; LOSE_SERIAL=""; OUT_DIR=""; MHVTL=0; HOOK_AT=""; FILLER_DIR=""; PRINT_PLAN=0
SPAN_SOURCE=""; WORK=""; WARNING_AT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --mhvtl) MHVTL=1; shift ;;
        --span-source) SPAN_SOURCE="${2:-}"; shift 2 ;;
        --work) WORK="${2:-}"; shift 2 ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --warning-at) WARNING_AT="${2:-}"; shift 2 ;;
        --hook-at) HOOK_AT="${2:-}"; shift 2 ;;
        --filler-dir) FILLER_DIR="${2:-}"; shift 2 ;;
        --print-plan) PRINT_PLAN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "span-acceptance: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

# ---------- the two shapes of the run ----------
MiB=$((1024 * 1024)); GiB=$((1024 * MiB))
BLOCK=524288
if [ "$MHVTL" = 1 ]; then
    SLICE_SIZE="1M"; SLICE_BYTES=$MiB
    CAPACITY=200000000            # the mhvtl drive's capacity_override, decimal
    CAP_OVERRIDE="$CAPACITY"
    POST_WARNING="64M"
    ENOSPC_BUFFER="4M"; RESERVE=$((4 * MiB))
    ALLOWANCE=$((16 * MiB))       # the volume's own files, an upper bound
    MARGIN=$((8 * MiB))           # how far the unit must reach past the warning
    SPAN_GEN_BYTES=$((48 * MiB))  # the generated source
    FRONT_EST=$((4 * MiB))        # Files 0-3 and the envelopes before the first slice
else
    SLICE_SIZE="1G"; SLICE_BYTES=$GiB
    CAPACITY=2500000000000        # the generation table's LTO-6 figure, as production
    CAP_OVERRIDE=""
    POST_WARNING="99008M"         # ADR-0012 item 118: the profile's --emit figure
    ENOSPC_BUFFER=""; RESERVE=$((50 * MiB))   # the default, 50M, as production
    ALLOWANCE=$((4 * GiB))
    MARGIN=$((8 * GiB))
    SPAN_GEN_BYTES=0
    FRONT_EST=0
fi
: "${WARNING_AT:=$WARNING_AT_MEASURED}"
# The filler's source: one full slice and a sixteenth, so its slice 1 is a full dar slice.
SIZE_F=$((SLICE_BYTES + SLICE_BYTES / 16))
# The fill ceiling as the planner computes it, (capacity x 0.97) truncated, less the reserve:
# a whole unit is committed while the plan's end through it is at or below this.
CEIL_BYTES="$(python3 -c 'import sys; print(int(float(sys.argv[1]) * 0.97))' "$CAPACITY")"
WHOLE_END=$((CEIL_BYTES - RESERVE))
case "$WARNING_AT" in ''|*[!0-9]*) die "--warning-at must be a plain decimal byte count" ;; esac
[ -z "$HOOK_AT" ] || case "$HOOK_AT" in *[!0-9]*) die "--hook-at must be a plain decimal byte count" ;; esac
[ -z "$HOOK_AT" ] || [ "$MHVTL" = 1 ] || die "--hook-at is the rehearsal's test hook: it needs --mhvtl"
# The smallest unit that reaches MARGIN past the warning when the filler ends at the ceiling
# (the real drive's; the rehearsal's source is generated to fit).
SPAN_MIN=$((WARNING_AT + MARGIN - (WHOLE_END - ALLOWANCE - 2 * SLICE_BYTES)))

print_plan() { # the real drive's plan, whichever mode prints it
    local min_gb note=""
    min_gb="$(awk -v w="$WARNING_AT_MEASURED" -v g="$GiB" \
        'BEGIN { printf "%.0f", (w + 8 * g - (2425000000000 - 50 * 1048576 - 4 * g - 2 * g)) / 1e9 + 0.5 }')"
    [ "$MHVTL" = 0 ] || note=$'\n(--mhvtl: the rehearsal runs the same steps on a virtual drive, at small sizes and with\nthe test hook; the figures below are the real drive\'s.)'
    cat <<PLAN
=== The 1.7.0 spanning acceptance run (ADR-0012 items 137 and 139) ===${note}

BEFORE YOU START (the operator):
  1. Load the test cartridge EW7VWMVKF6 into the HP LTO-6 (HUJ808A5L4). Nothing on it is kept:
     the run writes a new volume over it, twice.
  2. Nothing else may use the drive for the whole run:
       sudo systemctl stop tapectl-drive-poll.timer      (start it again afterwards)
     no production tapectl command running or scheduled, and the host quiet while the tape
     moves (vm-desk1's heavy jobs paused, docs/operator-guide.md "A quiet host while the tape
     runs").
  3. Choose the unit's source: a directory of real data, about 100 GB (at least $min_gb GB),
     --span-source. It is only read: the run copies it into --work.
  4. Choose --work on a disk with room for about 4 x the source (the copy, its staging, the
     restore's scratch and the restored tree): on home2 a directory on acache, since ~/.cache
     (/srv/home) has about 80 GB.
  5. Run it inside tmux: it takes most of a day. A dropped terminal does not stop it.
  6. A FAIL stops the run and leaves the cartridge as it is. Do not reuse or erase it until the
     failure is read (docs/lto6-validation-checklist.md).

THE STEPS, and the time each takes on home2 (the write at ~152 MiB/s and the reads at
~108 MiB/s, as 1.6.0's run measured them):
  1a setup       no tape. A throwaway home (post_warning_bytes = $POST_WARNING, min_copies = 1,
                 no global excludes), the source copied, both units staged, a sha256 manifest
                 of the copy, the filler's rows. stage create of 100 GB takes about 7.5 h with
                 a debug build (1.6.0's run encrypted at ~3.9 MiB/s); a release build is
                 faster. The copy and the manifest take about 30 min; a source file you
                 cannot read fails the copy here, before any tape moves.
  1b init        whatever the cartridge holds erased by this home's cartridge erase, then
                 $LABEL1 initialised.                                                ~5 min
  1c write       $LABEL1 with --keep-staging: the filler to the 2.425 TB ceiling, then
                 $UNIT_S until the drive's warning near 2.502 TB: a short seal, $UNIT_S a
                 range of slices 1..K1, item 114's readback of the slice past the warning.
                                                                                 ~4.5 h
  2  spool       RESTORE.sh off tape 1 reads the range (~80 GB) into scratch, still
                 encrypted, and waits at its prompt for the next cartridge.          ~15 min
  3  rebuild     catalog rebuild --from-volume of $LABEL1 into a second home.        ~5 min
  4a re-init     the two refusals (retire, cartridge erase), the filler released, one
                 filemark written at the start of the tape, $LABEL2 initialised with --force:
                 $LABEL1 is marked erased.                                           ~5 min
  4b write       $LABEL2: $UNIT_S whole, from the kept staging.                     ~15 min
  5  answer      the held RESTORE.sh takes $LABEL2 by hash, reads slices K1+1..N, restores
                 the unit (~100 GB) and the manifest is compared.                   ~1 h
  6  close       the result and the cleanup commands.
  About 5.5 hours of drive time; staging is most of the rest.

THE COMMAND (operator's login on home2):
  TAPECTL_HOME=\$HOME/.cache/tapectl/spn-home-\$(date +%Y%m%d) \\
  TAPECTL_BIN=<the 1.7.0 candidate> FIXTURE_BIN=<its short_seal_filler> \\
    scripts/span-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \\
      --i-will-lose-the-cartridge EW7VWMVKF6 \\
      --span-source /path/to/about-100-GB --work /srv/acache/<a new directory>

Afterwards the cartridge holds $LABEL2 ($UNIT_S whole) and belongs to the throwaway home: the
closing text names the commands that return it to the test pool; then start the drive-poll
timer.
PLAN
}

if [ "$PRINT_PLAN" = 1 ]; then print_plan; exit 0; fi

# ---------- preconditions: nothing below touches the drive until every one holds ----------
[ -n "$TAPE_DEV" ] || { usage >&2; die "--device is required; there is no default (/dev/nstN numbering moves)"; }
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
for bin in mt dd tr sg_read_attr sg_logs python3 flock age dar tar sha256sum awk tee df du \
    cp find mkfifo setsid; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
SG_LOGS_HELP="$(sg_logs --help 2>&1 || true)"
for opt in '--maxlen' '--in=' '--pdt' '--raw' '--page'; do
    case "$SG_LOGS_HELP" in *"$opt"*) ;; *) die "this sg_logs does not list the $opt option; the log-page reads would fail" ;; esac
done
# RESTORE.sh restores a unit across tapes only with dar 2.7.9 or later (2.7.21 without the
# unit's catalogue in the envelope); it would refuse after tape 1's hours of writing.
HEIR_DAR="$(dar -V 2>&1 </dev/null | sed -n 's/.*dar version \([0-9][0-9.]*\).*/\1/p' | sed -n 1p)"
python3 - "$HEIR_DAR" <<'PY' || die "the dar on PATH is '${HEIR_DAR:-unknown}': RESTORE.sh restores a unit across tapes with dar 2.7.9 or later"
import sys
v = tuple(int(x) for x in (sys.argv[1] or "0").split(".")[:3] if x.isdigit())
sys.exit(0 if v >= (2, 7, 9) else 1)
PY

# A real write with the test hook seals where the hook says, not where the medium does.
if [ -n "${TAPECTL_TEST_EARLY_WARNING_AT+set}" ]; then
    [ "$MHVTL" = 1 ] || die "TAPECTL_TEST_EARLY_WARNING_AT is set: a real-drive run with the test hook proves nothing. Unset it"
    die "TAPECTL_TEST_EARLY_WARNING_AT is set in the environment: the rehearsal sets it itself, for tape 1's write only (--hook-at). Unset it"
fi

[ -n "${TAPECTL_BIN:-}" ] || die "TAPECTL_BIN is required: the tapectl under test (this script never builds one)"
[ -x "$TAPECTL_BIN" ] || die "TAPECTL_BIN=$TAPECTL_BIN is not an executable file"
TAPECTL_BIN="$(readlink -f "$TAPECTL_BIN")"
: "${FIXTURE_BIN:=$(dirname "$TAPECTL_BIN")/examples/short_seal_filler}"
[ -x "$FIXTURE_BIN" ] || die "FIXTURE_BIN=$FIXTURE_BIN is not an executable file (cargo build --example short_seal_filler, built with TAPECTL_BIN)"
: "${DAR_BIN:=$(command -v dar)}"
[ -x "$DAR_BIN" ] || die "DAR_BIN=$DAR_BIN is not an executable file"
TAPECTL_VERSION="$("$TAPECTL_BIN" --version 2>&1 | sed -n 1p)"
if [ "$MHVTL" = 0 ]; then
    # The run is the 1.7.0 candidate's: a binary before 1.7.0 cannot span at all.
    python3 - "$TAPECTL_VERSION" <<'PY' || die "TAPECTL_BIN reports '$TAPECTL_VERSION': the acceptance run is for tapectl 1.7.0 or later"
import re, sys
m = re.search(r"(\d+)\.(\d+)\.(\d+)", sys.argv[1])
sys.exit(0 if m and tuple(map(int, m.groups())) >= (1, 7, 0) else 1)
PY
fi

# ---------- the throwaway home; production's must be unreachable ----------
[ -n "${TAPECTL_HOME:-}" ] || die "TAPECTL_HOME is unset: name a fresh throwaway home (it must not exist yet, or be empty)"
case "$TAPECTL_HOME" in /*) ;; *) die "TAPECTL_HOME must be an absolute path (got '$TAPECTL_HOME')" ;; esac
canon() { readlink -m "$1"; }
inside() { # <path> <dir>: path is dir or inside it
    case "$(canon "$1")/" in "$(canon "$2")"/*) return 0 ;; esac
    return 1
}
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
    inside "$HOME_C" "$p" && die "TAPECTL_HOME=$TAPECTL_HOME is (or is inside) production's home $p"
    inside "$p" "$HOME_C" && die "TAPECTL_HOME=$TAPECTL_HOME contains production's home $p"
done
if [ -e "$TAPECTL_HOME" ]; then
    [ -d "$TAPECTL_HOME" ] || die "TAPECTL_HOME=$TAPECTL_HOME exists and is not a directory"
    [ -z "$(ls -A "$TAPECTL_HOME")" ] || die "TAPECTL_HOME=$TAPECTL_HOME is not empty: every run starts from a fresh home"
fi

# ---------- the unit's source: given, readable, and large enough to reach past the warning ----------
if [ -n "$SPAN_SOURCE" ]; then
    if ! { [ -d "$SPAN_SOURCE" ] && [ -r "$SPAN_SOURCE" ] && [ -x "$SPAN_SOURCE" ]; }; then
        die "--span-source $SPAN_SOURCE is not a readable directory"
    fi
    SPAN_SOURCE="$(canon "$SPAN_SOURCE")"
    [ -n "$(ls -A "$SPAN_SOURCE")" ] || die "--span-source $SPAN_SOURCE is empty"
    inside "$SPAN_SOURCE" "$HOME_C" && die "--span-source is inside TAPECTL_HOME"
    inside "$HOME_C" "$SPAN_SOURCE" && die "TAPECTL_HOME is inside --span-source: the run would copy its own home"
    SRC_BYTES="$(du -sb "$SPAN_SOURCE" 2>/dev/null | cut -f1)" || die "cannot measure --span-source with du"
    case "$SRC_BYTES" in ''|*[!0-9]*) die "cannot measure --span-source with du (got '$SRC_BYTES')" ;; esac
elif [ "$MHVTL" = 1 ]; then
    SRC_BYTES=$SPAN_GEN_BYTES
else
    die "--span-source DIR is required on the real drive: the unit that may span is the operator's real data, about 100 GB"
fi
if [ "$MHVTL" = 0 ] && [ "$SRC_BYTES" -lt "$SPAN_MIN" ]; then
    die "--span-source holds $SRC_BYTES bytes: with the filler at the $WHOLE_END-byte fill ceiling and the warning near $WARNING_AT, the unit must hold at least $SPAN_MIN bytes ($((SPAN_MIN / GiB)) GiB) to reach $((MARGIN / GiB)) GiB past the warning; about 100 GB is the ruling's figure (item 137)"
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
GEN_N="$(echo "$SYS_MODEL" | sed -n 's/.*Ultrium \([0-9]\{1,2\}\).*/\1/p; s/.*TD\([0-9]\{1,2\}\).*/\1/p' | sed -n 1p)"
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
        | sed -n 1p | sed 's/^[^:]*: *//' | tr -d ' \r'
}
SERIAL="$(read_serial || true)"
[ -n "$SERIAL" ] || die "sg_read_attr reported no medium serial number on $DRIVE_SG (is a cartridge loaded?)"
if [ "$MHVTL" = 0 ]; then
    [ "$SERIAL" = "$TEST_CARTRIDGE" ] || die "the loaded cartridge's chip serial is '$SERIAL', not the test cartridge $TEST_CARTRIDGE"
elif [ -n "$LOSE_SERIAL" ]; then
    [ "$SERIAL" = "$LOSE_SERIAL" ] || die "the loaded cartridge's chip serial is '$SERIAL', not '$LOSE_SERIAL'"
fi

# ---------- the run directory and the work directory ----------
if [ -z "$OUT_DIR" ]; then
    if [ -d /scratch ]; then OUT_DIR="/scratch/tapectl-span"; else OUT_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl/span"; fi
fi
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
SAFE_SERIAL="$(printf '%s' "$SERIAL" | tr -c 'A-Za-z0-9_.-' '_')"
RUN="$OUT_DIR/spn-$STAMP-$SAFE_SERIAL"
: "${WORK:=$RUN/work}"
case "$WORK" in /*) ;; *) die "--work must be an absolute path (got '$WORK')" ;; esac
for p in "${PROD_HOMES[@]}" "$HOME2_STAGING"; do
    inside "$WORK" "$p" && die "--work $WORK is (or is inside) production's $p"
done
inside "$WORK" "$HOME_C" && die "--work is inside TAPECTL_HOME"
if [ -n "$SPAN_SOURCE" ]; then
    inside "$WORK" "$SPAN_SOURCE" && die "--work is inside --span-source: the copy would copy itself"
    inside "$SPAN_SOURCE" "$WORK" && die "--span-source is inside --work"
fi
if [ -e "$WORK" ]; then
    [ -d "$WORK" ] || die "--work $WORK exists and is not a directory"
    [ -z "$(ls -A "$WORK")" ] || die "--work $WORK is not empty: every run starts from a fresh work directory"
fi
mkdir -p "$RUN" || die "cannot create $RUN"
mkdir -p "$WORK" || die "cannot create $WORK"
# The work directory: the copy, its staged ciphertext (kept to tape 2), the restore's scratch
# (the version's whole ciphertext, RESTORE.sh's own check) and the restored tree, about the
# source's size each, and the filler's source and slice. The run directory: the catalogs,
# the logs, and RESTORE.sh's work directory of decrypted envelopes.
NEED_WORK=$((4 * SRC_BYTES + SRC_BYTES / 50 + 3 * SLICE_BYTES + 256 * MiB))
NEED_RUN=$((2 * GiB))
[ "$MHVTL" = 0 ] || NEED_RUN=$((256 * MiB))
free_of() { df --output=avail -B1 "$1" | tail -n 1 | tr -d ' '; }
AVAIL_WORK="$(free_of "$WORK")"; AVAIL_RUN="$(free_of "$RUN")"
if [ "$(stat -c %d "$WORK")" = "$(stat -c %d "$RUN")" ]; then
    [ "$AVAIL_WORK" -ge $((NEED_WORK + NEED_RUN)) ] || die "$WORK and $RUN share a disk with $((AVAIL_WORK / GiB)) GiB free; the run needs $(((NEED_WORK + NEED_RUN) / GiB)) GiB (give --work on a larger disk; on home2, acache)"
else
    [ "$AVAIL_WORK" -ge "$NEED_WORK" ] || die "--work $WORK has $((AVAIL_WORK / GiB)) GiB free; the copy, staging, scratch and restore need $((NEED_WORK / GiB)) GiB (on home2, a directory on acache)"
    [ "$AVAIL_RUN" -ge "$NEED_RUN" ] || die "$RUN has $((AVAIL_RUN / MiB)) MiB free; the run directory needs $((NEED_RUN / MiB)) MiB"
fi
mkdir -p "$TAPECTL_HOME"
REBUILD_HOME="$RUN/rebuild-home"
STAGING="$WORK/staging"; SRC="$WORK/src"; SPAN_DIR="$WORK/src/span"; FILLER_SRC="$WORK/src/filler"
SCRATCH="$WORK/scratch"; RESTORE_TO="$WORK/restored"
mkdir -p "$STAGING" "$SRC" "$SCRATCH" "$RUN/tmp"
CFG="$TAPECTL_HOME/config.toml"
DB="$TAPECTL_HOME/tapectl.db"
FIFO="$RUN/heir-answer.fifo"

# The filler's one file is read N times, each read evicted behind it (util::DropBehind); on a
# tmpfs that costs no disk.
if [ -z "$FILLER_DIR" ] && [ -d /dev/shm ]; then
    SHM_FREE="$(df --output=avail -B1 /dev/shm 2>/dev/null | tail -n 1 | tr -d ' ')"
    if [ -n "$SHM_FREE" ] && [ "$SHM_FREE" -ge $((3 * SLICE_BYTES)) ] 2>/dev/null; then
        FILLER_DIR="/dev/shm/tapectl-spn-$STAMP"
    fi
fi

# Every tapectl call: this run's home and config, the system home pointer switched off, and the
# test hook unset (the rehearsal's tape 1 write sets it on its own command line).
export TAPECTL_HOME_POINTER="$RUN/no-home-pointer"
TEE_OPT=(); if tee --output-error=warn </dev/null >/dev/null 2>&1; then TEE_OPT=(--output-error=warn); fi
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${TEE_OPT[@]}" "$RUN/run.log"; }
trap '' HUP

# ---------- the held RESTORE.sh run (step 2 starts it, step 5 answers it) ----------
# Started with setsid, so it is its own session and process group: $HELD_PID is that group.
HELD_PID=""
held_state() { # the held run's process state letter, or nothing when it is gone
    [ -n "$HELD_PID" ] || return 0
    local stat
    stat="$(cat "/proc/$HELD_PID/stat" 2>/dev/null)" || return 0
    stat="${stat##*) }"
    printf '%s' "${stat%% *}"
}
held_alive() {
    local s
    s="$(held_state)"
    [ -n "$s" ] && [ "$s" != Z ] && [ "$s" != X ]
}
# Ends the held run: never the medium. TERM to its process group, then KILL. Its scratch
# directory is left for the operator (RESTORE.sh: "a run that is killed leaves its scratch
# directory"), holding only ciphertext.
end_held() {
    held_alive || return 0
    echo "span-acceptance: ending the held RESTORE.sh run (pid $HELD_PID); its scratch directory under $SCRATCH stays (encrypted slices only): remove it by hand" >&2
    kill -TERM -- "-$HELD_PID" 2>/dev/null || kill -TERM "$HELD_PID" 2>/dev/null || true
    local n
    for n in 1 2 3 4 5 6 7 8 9 10; do
        held_alive || return 0
        sleep 1
    done
    echo "span-acceptance: the held run did not end after TERM (${n}s): KILL" >&2
    kill -KILL -- "-$HELD_PID" 2>/dev/null || kill -KILL "$HELD_PID" 2>/dev/null || true
}
trap 'end_held' EXIT

STEP=""; STEP_DIR="$RUN"
fail() {
    log "FAIL (step $STEP): $*"
    local held_note=""
    if held_alive; then
        held_note="The held RESTORE.sh run (pid $HELD_PID) was ended; its scratch directory under $SCRATCH holds encrypted slices only: remove it once the failure is read."
    fi
    end_held || true
    {
        echo "RESULT: FAIL"
        echo "step: $STEP"
        echo "why: $*"
        echo "The run stopped here. The cartridge is as this step left it: read the failure"
        echo "before anything writes it again. Everything is in $RUN; the copy, staging and"
        echo "restore are in $WORK."
        [ -z "$held_note" ] || echo "$held_note"
        [ -z "$FILLER_DIR" ] || [ ! -e "$FILLER_DIR" ] || echo "The filler's slice file is kept in $FILLER_DIR (RAM, on a tmpfs): remove it once the failure is read."
    } > "$RUN/RESULT.txt"
    snapshot_drive "$STEP_DIR" "at-fail" || true
    cp -a "$DB" "$RUN/catalog-at-fail.db" 2>/dev/null || true
    echo "span-acceptance: FAIL at step $STEP: $*" >&2
    echo "span-acceptance: the run directory is $RUN" >&2
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
tctl_hooked() { # the rehearsal's tape 1 write: the test hook on this one command line
    env TAPECTL_TEST_EARLY_WARNING_AT="$HOOK_AT" "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null
}
rctl() { # the rebuild's second home (no drive configured: the heir's case)
    env -u TAPECTL_TEST_EARLY_WARNING_AT "$TAPECTL_BIN" --home "$REBUILD_HOME" "$@" </dev/null
}

# Whether a write announced the store's test hook must match the run: the rehearsal's tape 1
# write announces it (the regex's positive control) and no other write does; on the real drive,
# none (a run with the hook proves nothing). grep's 2, an unreadable file, is never "no".
HOOK_RE='TAPECTL_TEST_EARLY_WARNING_AT=[0-9]+ is set'
assert_hook() { # <err file> <1: announced | 0: not>
    local rc=0
    grep -E "$HOOK_RE" "$1" >/dev/null || rc=$?
    if [ "$2" = 1 ]; then
        [ "$rc" = 0 ] || fail "the hooked write does not announce TAPECTL_TEST_EARLY_WARNING_AT ($1, grep rc=$rc)"
    else
        [ "$rc" = 1 ] || fail "a write run without the test hook announces it, or could not be searched ($1, grep rc=$rc)"
    fi
}

begin_step() { # <id> <name> <what>
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
# The stage-set status of each unit, "unit=status ...".
stage_statuses() {
    dbq "$DB" "SELECT u.name || '=' || ss.status FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
        JOIN units u ON u.id = s.unit_id ORDER BY u.name" | tr '\n' ' '
}

# A sha256 manifest of a tree: one line per entry, sorted by path, `type <TAB> path (JSON) <TAB>
# sha256 | link target | -`. Content and shape only (no modes or times): what "restores
# identical" means here. Files are hashed four at a time.
manifest() { # <dir> <out>
    python3 - "$1" "$2" <<'PY'
import hashlib, json, os, stat, sys
from concurrent.futures import ThreadPoolExecutor
root, out = sys.argv[1], sys.argv[2]
entries = []
for dirpath, dirnames, filenames in os.walk(root):
    for name in dirnames + filenames:
        p = os.path.join(dirpath, name)
        entries.append((os.path.relpath(p, root), p, os.lstat(p)))
def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            b = f.read(4 << 20)
            if not b:
                break
            h.update(b)
    return h.hexdigest()
files = [p for _, p, st in entries if stat.S_ISREG(st.st_mode)]
with ThreadPoolExecutor(max_workers=4) as pool:
    sums = dict(zip(files, pool.map(digest, files)))
lines, nbytes = [], 0
for rel, p, st in entries:
    m = st.st_mode
    if stat.S_ISREG(m):
        kind, value = "f", sums[p]
        nbytes += st.st_size
    elif stat.S_ISLNK(m):
        kind, value = "l", os.readlink(p)
    elif stat.S_ISDIR(m):
        kind, value = "d", "-"
    else:
        kind, value = "o", "%o" % stat.S_IFMT(m)
    lines.append((rel, "%s\t%s\t%s\n" % (kind, json.dumps(rel), json.dumps(value))))
lines.sort()
with open(out, "w") as f:
    f.writelines(l for _, l in lines)
print("%d entries, %d files, %d bytes" % (len(lines), len(files), nbytes))
PY
}

# One whole tape file, NULs dropped (the block padding), from the loaded tape.
tape_file() { # <position> <out>
    mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
    mt -f "$TAPE_DEV" setblk "$BLOCK" || fail "mt setblk failed"
    if [ "$1" -gt 0 ]; then mt -f "$TAPE_DEV" fsf "$1" || fail "mt fsf $1 failed"; fi
    dd if="$TAPE_DEV" bs=512k 2>"$2.dd.err" | tr -d '\0' > "$2" || fail "dd of tape file $1 failed"
}
# The `requires` line of the tape file at <position> must be item 124's normative spelling.
assert_requires_spanning() { # <position> <what>
    local f="$STEP_DIR/tape-file-$1.txt" line
    tape_file "$1" "$f"
    line="$(grep -E '^requires = ' "$f" | sed -n 1p || true)"
    [ "$line" = 'requires = ["short-seal", "spanning"]' ] \
        || fail "$2 (tape file $1) says '${line:-no requires line}', want requires = [\"short-seal\", \"spanning\"] (item 124)"
    log "  $2 (tape file $1): $line"
}

extract_restore_sh() { # <dir>: RESTORE.sh off the loaded tape, as File 1 tells an heir to
    mkdir -p "$1"
    tape_file 2 "$1/RESTORE.sh"
    chmod +x "$1/RESTORE.sh"
    bash -n "$1/RESTORE.sh" || fail "the RESTORE.sh read off the tape does not parse"
}

# The held run's tree and every descriptor it has open: FAIL on any tape or SCSI generic
# device, and on not finding its input on the FIFO (the positive control: the scan read the
# held run's descriptors, so an absence of the drive among them means something).
assert_no_device_open() { # <out>
    python3 - "$HELD_PID" "$FIFO" "$1" <<'PY' || fail "the held RESTORE.sh run has a tape device open, or its descriptors could not be read ($1)"
import os, re, stat, sys
root, fifo, out = int(sys.argv[1]), os.path.realpath(sys.argv[2]), sys.argv[3]
children = {}
for name in os.listdir("/proc"):
    if not name.isdigit():
        continue
    try:
        s = open("/proc/%s/stat" % name, "rb").read().decode("utf-8", "replace")
    except OSError:
        continue
    fields = s[s.rfind(")") + 2:].split()
    if len(fields) > 1:
        children.setdefault(int(fields[1]), []).append(int(name))
procs, todo = [root], [root]
while todo:
    for c in children.get(todo.pop(), []):
        procs.append(c)
        todo.append(c)
dev = re.compile(r"^/dev/(n?st[0-9]+[lma]?|sg[0-9]+|tape/.*)$")
held, stdin_fifo, seen = [], False, 0
lines = []
for pid in procs:
    try:
        fds = os.listdir("/proc/%d/fd" % pid)
    except OSError as e:
        lines.append("pid %d: cannot list descriptors: %s" % (pid, e))
        continue
    for fd in fds:
        path = "/proc/%d/fd/%s" % (pid, fd)
        try:
            target = os.readlink(path)
        except OSError:
            continue
        seen += 1
        lines.append("pid %d fd %s -> %s" % (pid, fd, target))
        if pid == root and fd == "0" and os.path.realpath(target) == fifo:
            stdin_fifo = True
        try:
            st = os.stat(path)
            chr_tape = stat.S_ISCHR(st.st_mode) and os.major(st.st_rdev) in (9, 21)
        except OSError:
            chr_tape = False
        if dev.match(target) or chr_tape:
            held.append("pid %d fd %s -> %s" % (pid, fd, target))
with open(out, "w") as f:
    f.write("\n".join(lines) + "\n")
print("held run: %d process(es), %d descriptor(s); input on the FIFO: %s; tape devices open: %s"
      % (len(procs), seen, "yes" if stdin_fifo else "NO", ", ".join(held) or "none"))
sys.exit(0 if stdin_fifo and not held else 1)
PY
}

# ============================================================
print_plan | tee "$RUN/PLAN.txt"
log "run $RUN; work $WORK"
log "drive $DRIVE_SERIAL ($SYS_MODEL) at $TAPE_DEV ($TAPE_REAL, $DRIVE_SG); cartridge chip serial $SERIAL"
log "mode: $([ "$MHVTL" = 1 ] && echo "mhvtl rehearsal" || echo "REAL DRIVE")"
log "tapectl: $TAPECTL_BIN ($TAPECTL_VERSION); fixture: $FIXTURE_BIN"
log "the unit's source: ${SPAN_SOURCE:-generated} ($SRC_BYTES bytes); the filler file in: ${FILLER_DIR:-staging}"
{
    echo "started=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "host=$(hostname)"
    echo "mode=$([ "$MHVTL" = 1 ] && echo mhvtl || echo real)"
    echo "device=$TAPE_DEV"; echo "device_real=$TAPE_REAL"; echo "sg=$DRIVE_SG"
    echo "drive_serial=$DRIVE_SERIAL"; echo "drive_model=$SYS_MODEL"; echo "cartridge_serial=$SERIAL"
    echo "tapectl=$TAPECTL_BIN"; echo "tapectl_version=$TAPECTL_VERSION"
    echo "tapectl_sha256=$(sha256sum "$TAPECTL_BIN" | awk '{print $1}')"
    echo "fixture=$FIXTURE_BIN"
    echo "dar_config=$("$DAR_BIN" -V 2>&1 </dev/null | grep -m1 'dar version' || true)"
    echo "dar_heir=$HEIR_DAR"
    echo "age=$(age --version 2>&1 | sed -n 1p)"
    echo "tar=$(tar --version 2>&1 | sed -n 1p)"
    echo "script_commit=$(git -C "$SCRIPT_DIR" describe --always --dirty 2>/dev/null || echo unknown)"
    echo "capacity=$CAPACITY"; echo "capacity_override=${CAP_OVERRIDE:-none}"
    echo "post_warning_bytes=$POST_WARNING"; echo "slice_size=$SLICE_SIZE"
    echo "span_source=${SPAN_SOURCE:-generated}"; echo "span_source_bytes=$SRC_BYTES"
    echo "warning_at_expected=$WARNING_AT"; echo "work=$WORK"
} > "$RUN/meta.txt"
: > "$RUN/steps.tsv"

# ------------------------------------------------------------
begin_step 1a setup "a throwaway home, the unit's copy and the filler staged, the filler's rows (no tape)"
run init 0 tctl init --operator "$OPERATOR" --no-escrow
run escrow 0 tctl key generate --escrow
ESCROW_SECRET="$(grep -oE 'AGE-SECRET-KEY-1[A-Z0-9]+' "$STEP_DIR/escrow.out" "$STEP_DIR/escrow.err" | sed -n 1p | sed 's/^[^:]*://')"
[ -n "$ESCROW_SECRET" ] || fail "key generate --escrow printed no secret"
ESCROW_PUB="$(grep -ohE 'age1[a-z0-9]+' "$STEP_DIR/escrow.out" "$STEP_DIR/escrow.err" | sed -n 1p)"
[ -n "$ESCROW_PUB" ] || fail "key generate --escrow printed no public key"
mkdir -p "$RUN/keys"; umask_old="$(umask)"; umask 077
printf '%s\n' "$ESCROW_SECRET" > "$RUN/keys/escrow.key"
umask "$umask_old"
printf '%s\n' "$ESCROW_PUB" > "$RUN/keys/escrow.pub"
chmod 600 "$STEP_DIR/escrow.out" "$STEP_DIR/escrow.err"
run cfg-dar 0 tctl config set dar.binary "\"$DAR_BIN\""
run cfg-slice 0 tctl config set defaults.slice_size "\"$SLICE_SIZE\""
run cfg-copies 0 tctl config set defaults.min_copies 1
# Every file of the copy is archived, so the restore can be compared with all of it.
run cfg-excludes 0 tctl config set defaults.global_excludes '[]'
run cfg-staging 0 tctl config set staging.directory "\"$STAGING\""
run backend 0 tctl backend add --name spn --device-tape "$TAPE_DEV" --generation LTO-6
# Item 123: a unit that may span is planned past the fill ceiling only on a drive that declares it.
run cfg-pwb 0 tctl config set 'backends.lto[0].post_warning_bytes' "\"$POST_WARNING\""
[ -z "$ENOSPC_BUFFER" ] || run cfg-reserve 0 tctl config set 'backends.lto[0].enospc_buffer' "\"$ENOSPC_BUFFER\""
[ -z "$CAP_OVERRIDE" ] || run cfg-capacity 0 tctl config set 'backends.lto[0].capacity_override' "\"$CAP_OVERRIDE\""
run config-check 0 tctl config check
cp "$CFG" "$RUN/config.toml"
run tenant 0 tctl tenant add "$TENANT"

mkdir -p "$FILLER_SRC" "$SPAN_DIR"
head -c "$SIZE_F" /dev/urandom > "$FILLER_SRC/filler.bin"
if [ -n "$SPAN_SOURCE" ]; then
    log "  copying $SPAN_SOURCE ($SRC_BYTES bytes) to $SPAN_DIR (the source is only read)"
    run copy 0 cp -a "$SPAN_SOURCE/." "$SPAN_DIR/"
    # Another home's unit dotfile in the copy would make `unit init` refuse it (an existing
    # unit, or one nested inside it): the copy loses them; the source keeps its own.
    run copy-dotfiles 0 find "$SPAN_DIR" -name .tapectl-unit.toml -type f -print -delete
    [ ! -s "$STEP_DIR/copy-dotfiles.out" ] || log "  removed from the copy (another home's unit dotfiles): $(wc -l < "$STEP_DIR/copy-dotfiles.out")"
else
    log "  generating the unit's source in $SPAN_DIR ($SRC_BYTES bytes)"
    mkdir -p "$SPAN_DIR/raw/nested/deeper" "$SPAN_DIR/docs"
    echo "a unit that may span, $STAMP" > "$SPAN_DIR/docs/plain.txt"
    printf '\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e content\n' > "$SPAN_DIR/docs/ünïcödé 日本語.txt"
    : > "$SPAN_DIR/docs/empty.bin"
    echo "deep leaf" > "$SPAN_DIR/raw/nested/deeper/leaf.txt"
    ln -s plain.txt "$SPAN_DIR/docs/link-ok"
    for i in 1 2 3 4 5 6; do head -c $((SRC_BYTES / 6)) /dev/urandom > "$SPAN_DIR/raw/blob-$i.bin"; done
fi
run unit-f 0 tctl unit init "$FILLER_SRC" --tenant "$TENANT" --name "$UNIT_F"
run unit-s 0 tctl unit init "$SPAN_DIR" --tenant "$TENANT" --name "$UNIT_S"
# Item 82: the unit allows spanning, in its dotfile's [policy], as an operator sets it.
printf '\n[policy]\nallow_spanning = true\n' >> "$SPAN_DIR/.tapectl-unit.toml"
cp "$SPAN_DIR/.tapectl-unit.toml" "$STEP_DIR/span-dotfile.toml"
for u in "$UNIT_F" "$UNIT_S"; do
    run "snapshot-$u" 0 tctl snapshot create "$u"
done
# Both are staged before the filler has its rows: stage create never sees the filler's size.
for u in "$UNIT_F" "$UNIT_S"; do
    run "stage-$u" 0 tctl --yes stage create "$u"
done
log "  the manifest of $UNIT_S's source, as staged"
run manifest-source 0 manifest "$SPAN_DIR" "$RUN/manifest-source.tsv"
# The unit's slices as staged: N, and their size on tape (block-padded).
N="$(dbq "$DB" "SELECT COUNT(*) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id WHERE u.name = ?" "$UNIT_S")"
SPAN_PADDED="$(dbq "$DB" "SELECT SUM((sl.encrypted_bytes + ? - 1) / ? * ?) FROM stage_slices sl
    JOIN stage_sets ss ON ss.id = sl.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE u.name = ?" "$BLOCK" "$BLOCK" "$BLOCK" "$UNIT_S")"
case "$N:$SPAN_PADDED" in *[!0-9:]*|:*|*:) fail "no staged slices for $UNIT_S (N='$N', bytes '$SPAN_PADDED')" ;; esac
log "  $UNIT_S: $N slices, $SPAN_PADDED bytes on tape"

# The filler: its plan end is the fill ceiling's whole-unit end plus every other staged set,
# which `fill` subtracts (the unit that may span is planned to its end after it).
FILL_ARGS=(--home "$TAPECTL_HOME" --unit "$UNIT_F" --allowance "$ALLOWANCE")
run filler-probe 0 "$FIXTURE_BIN" fill "${FILL_ARGS[@]}" --plan-end "$CAPACITY" --dry-run
while IFS='=' read -r k v; do
    case "$k" in [A-Z_]*) printf -v "FX_$k" '%s' "$v" ;; esac
done < "$STEP_DIR/filler-probe.out"
case "${FX_UNITS_BEFORE_PADDED_BYTES:-}:${FX_UNITS_AFTER_PADDED_BYTES:-}" in
    *[!0-9:]*|:*|*:) fail "fill --dry-run printed no UNITS_BEFORE/AFTER_PADDED_BYTES ($STEP_DIR/filler-probe.out)" ;;
esac
OTHERS=$((FX_UNITS_BEFORE_PADDED_BYTES + FX_UNITS_AFTER_PADDED_BYTES))
[ "$OTHERS" = "$SPAN_PADDED" ] || fail "the filler counts $OTHERS bytes of other staged sets, the catalog $SPAN_PADDED for $UNIT_S"
PLAN_END=$((WHOLE_END + OTHERS))
[ -z "$FILLER_DIR" ] || FILL_ARGS+=(--move-to "$FILLER_DIR")
run filler 0 "$FIXTURE_BIN" fill "${FILL_ARGS[@]}" --plan-end "$PLAN_END"
# KEY=VALUE lines: FILLER_SLICES, TEMPLATE_PADDED_BYTES, FILLER_PADDED_BYTES, ...
while IFS='=' read -r k v; do
    case "$k" in [A-Z_]*) printf -v "FX_$k" '%s' "$v" ;; esac
done < "$STEP_DIR/filler.out"
FILLER_PADDED="${FX_FILLER_PADDED_BYTES:-}"
case "$FILLER_PADDED" in ''|*[!0-9]*) fail "fill printed no FILLER_PADDED_BYTES" ;; esac
log "  filler: ${FX_FILLER_SLICES:-?} slices, $FILLER_PADDED bytes on tape; the whole units end by $WHOLE_END (the fill ceiling less the reserve)"
# Where the unit that may span sits: after the filler and the volume's own files before it
# (at most the allowance), to its end. The warning must come inside it, MARGIN before its end.
START_MIN=$FILLER_PADDED
START_MAX=$((ALLOWANCE + FILLER_PADDED))
END_MIN=$((START_MIN + SPAN_PADDED))
if [ "$MHVTL" = 1 ]; then
    if [ -z "$HOOK_AT" ]; then HOOK_AT=$((FRONT_EST + FILLER_PADDED + SPAN_PADDED * 3 / 4)); fi
    WARNING_AT=$HOOK_AT
    log "  rehearsal: TAPECTL_TEST_EARLY_WARNING_AT=$HOOK_AT for tape 1's write only"
fi
[ $((START_MAX + RESERVE)) -le "$CEIL_BYTES" ] || fail "the filler and the allowance end at $START_MAX, past the fill ceiling's $WHOLE_END: the filler would not be committed"
[ $((START_MAX + 2 * SLICE_BYTES)) -le "$WARNING_AT" ] || fail "$UNIT_S may start at $START_MAX, too near the warning at $WARNING_AT for a range"
[ "$END_MIN" -ge $((WARNING_AT + MARGIN)) ] || fail "$UNIT_S ends by $END_MIN at the earliest, not $MARGIN bytes past the warning at $WARNING_AT: it would be written whole"
[ "$END_MIN" -gt "$CAPACITY" ] || fail "$UNIT_S ends by $END_MIN, within the plan bound $CAPACITY: it would not be the span's tail (item 123)"
log "  $UNIT_S starts between $START_MIN and $START_MAX and ends past $END_MIN; the warning expected at $WARNING_AT"
run staging-status any tctl --json staging status
# The plan, before any tape: the filler whole under the ceiling, the unit that may span the
# tail, planned to its end (items 83 and 123), nothing deferred or refused.
run plan 0 tctl --json volume plan
python3 - "$STEP_DIR/plan.out" "$UNIT_F" "$UNIT_S" "$N" <<'PY' > "$STEP_DIR/assert-plan.txt" 2>&1 || fail "volume plan does not plan the filler under the ceiling and $UNIT_S as the span's tail ($STEP_DIR/assert-plan.txt)"
import json, sys
d = json.load(open(sys.argv[1]))["spanning"]
f, s, n = sys.argv[2], sys.argv[3], int(sys.argv[4])
assert d is not None, "volume plan shows no spanning plan"
got = [(u["unit"], u["class"], u["placed"], u["first_slice"], u["last_slice"]) for u in d["units"]]
want = [(f, "whole", "committed", 1, got[0][4] if got else 0), (s, "may span", "tail", 1, n)]
assert got == want, f"the plan: {got}, want {want}"
assert d["deferred"] == [] and d["refused"] == [], d
print(f"volume plan: {f} whole under the fill ceiling, then {s} slices 1-{n}, the tail")
PY
log "  $(tail -n 1 "$STEP_DIR/assert-plan.txt")"
end_step

# ------------------------------------------------------------
begin_step 1b init "the cartridge erased from this home, then volume init $LABEL1"
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
[ "$GOT_CAP" = "$CAPACITY" ] || fail "volume init stored capacity $GOT_CAP, not $CAPACITY: the plan's figures would not hold"
end_step

# ------------------------------------------------------------
begin_step 1c write "volume write $LABEL1 --keep-staging to the early warning: a range of $UNIT_S"
if [ "$MHVTL" = 1 ]; then
    run write 0 tctl_hooked --json --yes volume write "$LABEL1" --device "$TAPE_DEV" --keep-staging
else
    run write 0 tctl --json --yes volume write "$LABEL1" --device "$TAPE_DEV" --keep-staging
fi
K="$(json_get "$STEP_DIR/write.out" 'd["ended_after"]')"
case "$K" in ''|*[!0-9]*) fail "the write reported ended_after '$K': it did not seal short" ;; esac
grep -F "sealed short" "$STEP_DIR/write.err" >/dev/null || fail "the write's stderr does not report the short seal"
assert_hook "$STEP_DIR/write.err" "$MHVTL"
# The write's report, the rows, and the staging it kept.
python3 - "$DB" "$STEP_DIR/write.out" "$LABEL1" "$UNIT_F" "$UNIT_S" "$K" "$N" <<'PY' > "$STEP_DIR/assert-write.txt" 2>&1 || fail "tape 1's write, rows or staging are not what item 137 expects ($STEP_DIR/assert-write.txt)"
import json, os, sqlite3, sys
db, out, label, uf, us, k, n = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], int(sys.argv[6]), int(sys.argv[7])
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=30)
j = json.load(open(out))
def q(sql, *a): return c.execute(sql, a).fetchall()
assert j["ending"] == "short" and j["ended_after"] == k, j
assert j["cut_units"] == [] and j["deferred_units"] == [] and j["keep_together"] == [], j
assert len(j["range_units"]) == 1, j["range_units"]
r = j["range_units"][0]
k1 = r["last_slice"]
assert r == {"unit": us, "version": 1, "first_slice": 1, "last_slice": k1} and 1 <= k1 < n, f"range_units {j['range_units']} of {n} slices"
assert j["keep_staging"] is True and j["staging_released"] is False, "--keep-staging did not keep the staging"
assert all(s["slices"] == [] for s in j["slices_released"]), f"slices released under --keep-staging: {j['slices_released']}"
assert q("SELECT status, ended_after FROM volumes WHERE label = ?", label) == [("sealed", k)], q("SELECT status, ended_after FROM volumes WHERE label = ?", label)
rows = {u: (st, ms) for u, st, ms in q("""SELECT u.name, w.status, w.may_span FROM writes w
    JOIN volumes v ON v.id = w.volume_id JOIN snapshots s ON s.id = w.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE v.label = ?""", label)}
assert rows == {uf: ("completed", 0), us: ("range", 1)}, f"tape 1's rows (status, may_span): {rows} (items 121, 124)"
pos = q("""SELECT sl.slice_number, CAST(wp.position AS INTEGER), wp.status FROM write_positions wp
    JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
    JOIN stage_slices sl ON sl.id = wp.stage_slice_id JOIN snapshots s ON s.id = w.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE v.label = ? AND u.name = ? ORDER BY sl.slice_number""", label, us)
written = [(sn, p) for sn, p, st in pos if st == "written"]
assert [sn for sn, _ in written] == list(range(1, k1 + 1)), f"written slices {[sn for sn, _ in written]}, want 1-{k1}"
ps = [p for _, p in written]
assert ps == list(range(ps[0], ps[0] + k1)), f"the range's positions are not consecutive: {ps}"
assert ps[-1] == k, f"the range's last written position is {ps[-1]}, not ended_after {k}"
assert all(p <= k for p in ps), "a written position past ended_after"
past = [(sn, p, st) for sn, p, st in pos if st != "written"]
assert all(p > k for _, p, _ in past), f"an unwritten position at or before ended_after: {past}"
f_last = q("""SELECT MAX(CAST(wp.position AS INTEGER)) FROM write_positions wp JOIN writes w ON w.id = wp.write_id
    JOIN volumes v ON v.id = w.volume_id JOIN snapshots s ON s.id = w.snapshot_id JOIN units u ON u.id = s.unit_id
    WHERE v.label = ? AND u.name = ? AND wp.status = 'written'""", label, uf)[0][0]
assert f_last is not None and f_last < ps[0], f"the filler ends at {f_last}, not before {us} at {ps[0]} (item 83)"
st = q("SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = ?", us)[0][0]
assert st == "staged", f"{us} is {st} after tape 1: a range alone is not a copy (item 128)"
sl = q("""SELECT sl.slice_number, sl.released_at, sl.staging_path FROM stage_slices sl
    JOIN stage_sets ss ON ss.id = sl.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE u.name = ? ORDER BY sl.slice_number""", us)
assert len(sl) == n and all(r is None for _, r, _ in sl), "a slice of the unit was released under --keep-staging (item 129)"
missing = [sn for sn, _, p in sl if not (p and os.path.isfile(p))]
assert not missing, f"staged files missing for slices {missing}"
print(f"K1={k1}")
print(f"tape 1: sealed short after file {k}; {uf} completed; {us} a range of slices 1-{k1} of {n} at files {ps[0]}-{ps[-1]}, every one written at or before ended_after; staged, every slice kept")
PY
K1="$(sed -n 's/^K1=//p' "$STEP_DIR/assert-write.txt")"
case "$K1" in ''|*[!0-9]*) fail "no K1 from tape 1's assertions ($STEP_DIR/assert-write.txt)" ;; esac
log "  $(tail -n 1 "$STEP_DIR/assert-write.txt")"
# Item 114: the confirm read back what was written past the warning, the slice at file K and
# the seal, whatever the tier. The confirm phase's bytes (phase_timings, the session's own
# count) hold at least that slice, block-padded, and the seal's block.
CONFIRM_BYTES="$(dbq "$DB" "SELECT pt.bytes FROM phase_timings pt JOIN volumes v ON v.id = pt.volume_id
    WHERE v.label = ? AND pt.operation = 'volume write' AND pt.phase = 'confirm' ORDER BY pt.id DESC LIMIT 1" "$LABEL1")"
K_BYTES="$(dbq "$DB" "SELECT sl.encrypted_bytes FROM write_positions wp JOIN writes w ON w.id = wp.write_id
    JOIN volumes v ON v.id = w.volume_id JOIN stage_slices sl ON sl.id = wp.stage_slice_id
    WHERE v.label = ? AND wp.position = ?" "$LABEL1" "$K" | sed -n 1p)"
case "$CONFIRM_BYTES" in ''|*[!0-9]*) fail "no byte count for the write's confirm phase (phase_timings)" ;; esac
case "$K_BYTES" in ''|*[!0-9]*) fail "no slice recorded at file $K" ;; esac
K_PADDED=$(( (K_BYTES + BLOCK - 1) / BLOCK * BLOCK ))
[ "$CONFIRM_BYTES" -ge $(( K_PADDED + BLOCK )) ] || fail "the confirm read $CONFIRM_BYTES bytes, less than the slice at file $K ($K_PADDED padded) and the seal: item 114's readback past the warning did not run"
log "  item 114: the confirm read back $CONFIRM_BYTES bytes, including the slice at file $K ($K_PADDED bytes on tape) and the seal"
dbq "$DB" "SELECT e.timestamp, e.field, e.new_value, e.details FROM events e JOIN volumes v ON v.id = e.entity_id
    WHERE e.action = 'write_early_warning' AND e.entity_type = 'volume' AND v.label = ?" "$LABEL1" > "$STEP_DIR/early-warning-event.tsv"
[ -s "$STEP_DIR/early-warning-event.tsv" ] || fail "no write_early_warning event for $LABEL1"
[ "$(cut -f3 "$STEP_DIR/early-warning-event.tsv" | tail -1)" = "$K" ] || fail "the write_early_warning event's ended_after is not $K"
log "  write_early_warning: $(tail -1 "$STEP_DIR/early-warning-event.tsv")"
dbq "$DB" "SELECT action, details FROM events WHERE action IN ('write_feed_ratio', 'write_capacity_used', 'write_completed')
    ORDER BY id" > "$STEP_DIR/write-events.tsv" || true
log "  feed ratio: $(grep '^write_feed_ratio' "$STEP_DIR/write-events.tsv" | sed -n 1p | cut -f2 || true)"
# Item 124: File 0 and the short seal (the last file, at K+1) state both features.
assert_requires_spanning 0 "File 0"
assert_requires_spanning $((K + 1)) "the short seal"
grep -E "^ended_after = $K\$" "$STEP_DIR/tape-file-$((K + 1)).txt" >/dev/null || fail "the short seal at file $((K + 1)) does not state ended_after = $K"
run volume-info 0 tctl volume info "$LABEL1"
cp -a "$DB" "$RUN/catalog-after-tape1.db"
end_step

# ------------------------------------------------------------
begin_step 2 spool "RESTORE.sh's default mode spools tape 1's range into scratch and is held at its prompt"
HEIR_DIR="$RUN/heir"
extract_restore_sh "$HEIR_DIR"
TENANT_KEY="$TAPECTL_HOME/keys/$TENANT-primary.age.key"
[ -f "$TENANT_KEY" ] || fail "the tenant key $TENANT_KEY is missing"
RANGE_BYTES="$(dbq "$DB" "SELECT SUM(sl.encrypted_bytes) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
    WHERE u.name = ? AND sl.slice_number <= ?" "$UNIT_S" "$K1")"
# The answer comes from the script, through a FIFO it holds open read-write (so RESTORE.sh's
# read waits rather than meeting an end of input), in step 5. Started in its own session.
mkfifo "$FIFO"
exec 8<>"$FIFO"
HELD_OUT="$STEP_DIR/held.out"
HELD_CMD=(env TAPE_DEVICE="$TAPE_DEV" TMPDIR="$RUN/tmp" setsid "$HEIR_DIR/RESTORE.sh" --restore
    --unit "$UNIT_S" --key "$TENANT_KEY" --to "$RESTORE_TO" --scratch "$SCRATCH")
printf '+ %q' "${HELD_CMD[@]}" >> "$RUN/commands.log"; echo " < $FIFO (held)" >> "$RUN/commands.log"
log "  \$ ${HELD_CMD[*]} < $FIFO   (held: answered in step 5)"
"${HELD_CMD[@]}" <"$FIFO" >"$HELD_OUT" 2>&1 8>&- &
HELD_PID=$!
echo "$HELD_PID" > "$STEP_DIR/held.pid"
sleep 2
HELD_SID="$(python3 -c 'import os, sys; print(os.getsid(int(sys.argv[1])))' "$HELD_PID" 2>/dev/null || true)"
[ "$HELD_SID" = "$HELD_PID" ] || fail "the held run (pid $HELD_PID) is not its own session (sid '${HELD_SID:-gone}'): setsid forked, or it ended at once (held.out)"
# Until its prompt: RESTORE.sh reads the range, ~$((RANGE_BYTES / 1000000)) MB; a bound of
# half an hour plus 10 MB/s, so a hung read ends the run rather than holding it for ever.
SPOOL_BOUND=$((1800 + RANGE_BYTES / 10000000))
T0="$(date +%s)"; NEXT_NOTE=$((T0 + 300))
while ! grep -F "$PROMPT" "$HELD_OUT" >/dev/null; do
    held_alive || fail "RESTORE.sh ended before its swap prompt ($HELD_OUT)"
    NOW="$(date +%s)"
    [ $((NOW - T0)) -le "$SPOOL_BOUND" ] || fail "RESTORE.sh has not reached its swap prompt after $((NOW - T0)) s (the bound is $SPOOL_BOUND s; $HELD_OUT)"
    if [ "$NOW" -ge "$NEXT_NOTE" ]; then
        log "  spooling: $(find "$SCRATCH" -name 'slice.*.enc' 2>/dev/null | wc -l) of $K1 slices in scratch after $((NOW - T0)) s"
        NEXT_NOTE=$((NOW + 300))
    fi
    sleep 10
done
sleep 5
held_alive || fail "RESTORE.sh ended after its swap prompt, without an answer ($HELD_OUT)"
[ "$(grep -cF "$PROMPT" "$HELD_OUT")" = 1 ] || fail "RESTORE.sh prompted more than once before an answer ($HELD_OUT)"
log "  at the prompt after $(( $(date +%s) - T0 )) s: $(grep -F "$PROMPT" "$HELD_OUT")"
for needle in "'$UNIT_S' (version 1) is held across tapes: this tape holds slices 1 to $K1 of $N." \
    "'$UNIT_S' (version 1) needs slice $((K1 + 1)) of $N next" \
    "No index this key opened names a tape that holds slice $((K1 + 1))" \
    "${PROMPT}$((K1 + 1)) into "; do
    grep -F -- "$needle" "$HELD_OUT" >/dev/null || fail "RESTORE.sh's output lacks: $needle ($HELD_OUT)"
done
# The spool: slices 1..K1, each still age ciphertext, each the size the catalog records.
python3 - "$DB" "$SCRATCH" "$UNIT_S" "$K1" <<'PY' > "$STEP_DIR/assert-spool.txt" 2>&1 || fail "the scratch directory does not hold tape 1's range as ciphertext ($STEP_DIR/assert-spool.txt)"
import glob, os, sqlite3, sys
db, scratch, us, k1 = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=30)
size = dict(c.execute("""SELECT sl.slice_number, sl.encrypted_bytes FROM stage_slices sl
    JOIN stage_sets ss ON ss.id = sl.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE u.name = ?""", (us,)).fetchall())
dirs = glob.glob(os.path.join(scratch, ".tapectl-restore.*"))
assert len(dirs) == 1, f"scratch directories under {scratch}: {dirs}"
files = sorted(os.listdir(dirs[0]))
want = sorted(f"slice.{i}.enc" for i in range(1, k1 + 1))
assert files == want, f"{dirs[0]} holds {files}, want slice.1.enc to slice.{k1}.enc"
for name in files:
    p = os.path.join(dirs[0], name)
    n = int(name.split(".")[1])
    with open(p, "rb") as f:
        head = f.read(21)
    assert head == b"age-encryption.org/v1", f"{name} is not age ciphertext at rest: {head!r}"
    assert os.path.getsize(p) == size[n], f"{name} is {os.path.getsize(p)} bytes, the slice {size[n]}"
print(f"{dirs[0]}: slices 1-{k1}, each age ciphertext of the slice's recorded size")
PY
log "  $(tail -n 1 "$STEP_DIR/assert-spool.txt")"
assert_no_device_open "$STEP_DIR/held-fds.txt" > "$STEP_DIR/held-fds.summary"
log "  $(cat "$STEP_DIR/held-fds.summary")"
end_step

# ------------------------------------------------------------
begin_step 3 rebuild "catalog rebuild --from-volume of $LABEL1 into a second home, while the restore is held"
held_alive || fail "the held RESTORE.sh run ended before step 3 ($HELD_OUT)"
OPERATOR_KEY="$TAPECTL_HOME/keys/$OPERATOR-primary.age.key"
[ -f "$OPERATOR_KEY" ] || fail "the operator key $OPERATOR_KEY is missing"
mkdir -p "$REBUILD_HOME"
# The DR recipe (ADR-0012, #139): a new home adopting the original escrow recipient, then the
# rebuild with the operator key. No drive is configured there, as on an heir's machine.
run rebuild-init 0 rctl init --operator "$OPERATOR" --escrow-public-key "$ESCROW_PUB"
run rebuild 0 rctl --json catalog rebuild --from-volume --device "$TAPE_DEV" --key "$OPERATOR_KEY" --label "$LABEL1"
python3 - "$REBUILD_HOME/tapectl.db" "$STEP_DIR/rebuild.out" "$LABEL1" "$UNIT_F" "$UNIT_S" "$K" "$K1" "$N" <<'PY' > "$STEP_DIR/assert-rebuild.txt" 2>&1 || fail "the rebuild of $LABEL1 is not what item 130 expects ($STEP_DIR/assert-rebuild.txt)"
import json, sqlite3, sys
db, out, label, uf, us = sys.argv[1:6]
k, k1, n = int(sys.argv[6]), int(sys.argv[7]), int(sys.argv[8])
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=30)
j = json.load(open(out))
def q(sql, *a): return c.execute(sql, a).fetchall()
assert j["ended_after"] == k, f"the rebuild read ended_after {j['ended_after']}, not {k}"
assert j["range_units"] == [{"unit": us, "version": 1, "first_slice": 1, "last_slice": k1, "num_slices": n}], j["range_units"]
assert j["versions_promoted"] == [], j["versions_promoted"]
sp = j["spans_in_progress"]
assert len(sp) == 1 and sp[0]["unit"] == us and sp[0]["version"] == 1 and sp[0]["num_slices"] == n, sp
assert sp[0]["missing_slices"] == list(range(k1 + 1, n + 1)), f"missing {sp[0]['missing_slices']}, want {k1 + 1}-{n}"
assert sp[0]["holders"] == [], f"tape 1's index names holders {sp[0]['holders']}: a tape never names one written after it"
rows = sorted(q("""SELECT v.label, v.status, u.name, w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
    JOIN snapshots s ON s.id = w.snapshot_id JOIN units u ON u.id = s.unit_id"""))
assert rows == sorted([(label, "sealed", uf, "completed"), (label, "sealed", us, "range")]), f"the rebuilt rows: {rows}"
sl = q("""SELECT sl.slice_number, wp.status FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
    LEFT JOIN write_positions wp ON wp.stage_slice_id = sl.id WHERE u.name = ? ORDER BY sl.slice_number""", us)
assert sorted({s for s, _ in sl}) == list(range(1, n + 1)), f"the manifest lists {n} slices; the rebuild recorded {sorted({s for s, _ in sl})}"
written = sorted(s for s, st in sl if st == "written")
assert written == list(range(1, k1 + 1)), f"written {written}, want only tape 1's 1-{k1}"
st = q("SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = ?", us)[0][0]
assert st == "staged", f"{us} is {st} from tape 1 alone, want staged (0 copies until its other tapes, item 130)"
print(f"rebuild of {label}: {us} a range 1-{k1} of {n} slice rows, staged; slices {k1 + 1}-{n} named missing, no holder named")
PY
log "  $(tail -n 1 "$STEP_DIR/assert-rebuild.txt")"
run compare 0 "$FIXTURE_BIN" compare --home "$TAPECTL_HOME" --rebuilt "$REBUILD_HOME" --label "$LABEL1"
tee -a "$RUN/run.log" < "$STEP_DIR/compare.out"
held_alive || fail "the held RESTORE.sh run ended during the rebuild ($HELD_OUT)"
end_step

# ------------------------------------------------------------
begin_step 4a reinit "$LABEL1 given up by re-initialising the cartridge as $LABEL2 (item 139)"
held_alive || fail "the held RESTORE.sh run ended before step 4 ($HELD_OUT)"
# The 1.6.0 doors are closed, as item 139 says. Retire: item 128's floor, tape 1 the only
# holder of slices 1..K1 of a version not yet current.
run retire-dry 0 tctl --json --dry-run volume retire "$LABEL1"
LAST="$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
print(" ".join(str(v) for u in d["affected_units"] if u["unit"] == sys.argv[2] for v in u["last_copy_versions"]) or "none")' \
    "$STEP_DIR/retire-dry.out" "$UNIT_S" 2>/dev/null || echo error)"
[ "$LAST" = 1 ] || fail "the dry run of retiring $LABEL1 says '$LAST' of $UNIT_S's versions lose their last copy, want 1 (item 128)"
run retire any tctl --yes volume retire "$LABEL1"
[ "$RC" != 0 ] || fail "$LABEL1 was retired holding the only copy of $UNIT_S's slices 1-$K1: item 128's floor did not hold"
for needle in "\"$LABEL1\" holds the LAST eligible copy" \
    "Finish the span first: unit \"$UNIT_S\" v1 is a version still being written across cartridges"; do
    grep -F -- "$needle" "$STEP_DIR/retire.out" "$STEP_DIR/retire.err" >/dev/null || fail "the refused retire does not say: $needle"
done
[ "$(dbq "$DB" "SELECT status FROM volumes WHERE label = ?" "$LABEL1")" = sealed ] || fail "the refused retire changed $LABEL1"
# cartridge erase: the catalog still holds tape 1 (item 51), so it is refused, naming retire.
run erase-refused any tctl cartridge erase --device "$TAPE_DEV" --dry-run
[ "$RC" != 0 ] || fail "cartridge erase --dry-run accepted $LABEL1, a volume the catalog holds"
grep -F "tapectl volume retire $LABEL1" "$STEP_DIR/erase-refused.out" "$STEP_DIR/erase-refused.err" >/dev/null \
    || fail "cartridge erase's refusal does not name 'tapectl volume retire $LABEL1'"
# The filler leaves staging now, while tape 1 still gives it its copy: `volume write` writes
# everything staged, and a 2.4 TB filler on tape 2 would end $UNIT_S in a range again.
run release-filler 0 tctl --yes staging clean --unit "$UNIT_F"
STAGED="$(stage_statuses)"
log "  stage sets before tape 2: $STAGED"
[ "$STAGED" = "$UNIT_F=cleaned $UNIT_S=staged " ] || fail "stage sets before tape 2 are '$STAGED', expected the filler released and $UNIT_S staged"
# The erase by hand: one filemark at the start of the tape, as `cartridge erase`'s short
# erase writes it. File 0 then reads EMPTY, which no record of this catalog proves blank, so
# the init takes --force (the consent this run was given).
log "  erasing by hand: one filemark at the start of the tape (File 0 then reads EMPTY)"
run mt-rewind 0 mt -f "$TAPE_DEV" rewind
run mt-weof 0 mt -f "$TAPE_DEV" weof 1
run mt-rewind-2 0 mt -f "$TAPE_DEV" rewind
run init2 0 tctl --json volume init "$LABEL2" --device "$TAPE_DEV" --force
GOT_CAP="$(json_get "$STEP_DIR/init2.out" 'd["medium"]["capacity_bytes"]')"
[ "$GOT_CAP" = "$CAPACITY" ] || fail "volume init stored capacity $GOT_CAP for $LABEL2, not $CAPACITY"
grep -F "previously held volume \"$LABEL1\"; it is now marked erased" "$STEP_DIR/init2.err" >/dev/null \
    || fail "volume init $LABEL2 does not warn that $LABEL1 is now marked erased (binding's displacement)"
log "  $(grep -F "previously held volume" "$STEP_DIR/init2.err" | sed -n 1p)"
[ "$(dbq "$DB" "SELECT status FROM volumes WHERE label = ?" "$LABEL1")" = erased ] || fail "$LABEL1 is not erased after $LABEL2's init (item 139)"
[ -n "$(dbq "$DB" "SELECT id FROM events WHERE action = 'displaced' AND entity_type = 'cartridge'")" ] || fail "no displaced event for the cartridge"
held_alive || fail "the held RESTORE.sh run ended during the re-init ($HELD_OUT)"
end_step

# ------------------------------------------------------------
begin_step 4b write "volume write $LABEL2: $UNIT_S whole, from the kept staging"
run write2 0 tctl --json --yes volume write "$LABEL2" --device "$TAPE_DEV"
assert_hook "$STEP_DIR/write2.err" 0
python3 - "$DB" "$STEP_DIR/write2.out" "$LABEL2" "$UNIT_S" "$N" <<'PY' > "$STEP_DIR/assert-write2.txt" 2>&1 || fail "tape 2 is not a whole copy of $UNIT_S ($STEP_DIR/assert-write2.txt)"
import json, sqlite3, sys
db, out, label, us, n = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=30)
j = json.load(open(out))
def q(sql, *a): return c.execute(sql, a).fetchall()
assert j["ending"] == "full" and j["ended_after"] is None, j
assert j["cut_units"] == [] and j["range_units"] == [] and j["deferred_units"] == [] and j["keep_together"] == [], j
rows = q("""SELECT u.name, w.status, w.may_span FROM writes w JOIN volumes v ON v.id = w.volume_id
    JOIN snapshots s ON s.id = w.snapshot_id JOIN units u ON u.id = s.unit_id WHERE v.label = ?""", label)
assert rows == [(us, "completed", 1)], f"tape 2's rows (unit, status, may_span): {rows} (item 139)"
written = [sn for (sn,) in q("""SELECT sl.slice_number FROM write_positions wp JOIN writes w ON w.id = wp.write_id
    JOIN volumes v ON v.id = w.volume_id JOIN stage_slices sl ON sl.id = wp.stage_slice_id
    WHERE v.label = ? AND wp.status = 'written' ORDER BY sl.slice_number""", label)]
assert written == list(range(1, n + 1)), f"tape 2 holds slices {written}, want 1-{n}, planned from slice 1"
st = q("SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = ?", us)[0][0]
assert st == "current", f"{us} is {st} with every slice on sealed {label} (item 128)"
assert q("SELECT status FROM volumes WHERE label = ?", label) == [("sealed",)], "tape 2 is not sealed"
print(f"tape 2: {us} completed, slices 1-{n}, planned from slice 1; the version current")
PY
log "  $(tail -n 1 "$STEP_DIR/assert-write2.txt")"
# Item 124: any tape whose plan holds a slice of a spannable set states both features.
assert_requires_spanning 0 "$LABEL2's File 0"
run volume-info2 0 tctl volume info "$LABEL2"
cp -a "$DB" "$RUN/catalog-after-tape2.db"
held_alive || fail "the held RESTORE.sh run ended during tape 2's write ($HELD_OUT)"
end_step

# ------------------------------------------------------------
begin_step 5 answer "the held RESTORE.sh run answered with $LABEL2 in the drive, and the unit restored"
held_alive || fail "the held RESTORE.sh run ended before its answer ($HELD_OUT)"
[ "$(grep -cF "$PROMPT" "$HELD_OUT")" = 1 ] || fail "RESTORE.sh prompted again while held ($HELD_OUT)"
assert_no_device_open "$STEP_DIR/held-fds.txt" > "$STEP_DIR/held-fds.summary"
log "  $(cat "$STEP_DIR/held-fds.summary")"
log "  answering the held run (ENTER) with $LABEL2 loaded, after $(( $(date +%s) - T0 )) s held"
printf '\n' >&8
T5="$(date +%s)"
while held_alive; do
    if [ "$(grep -cF "$PROMPT" "$HELD_OUT" || true)" -gt 1 ]; then
        printf 'quit\n' >&8
        sleep 5
        fail "RESTORE.sh did not take $LABEL2 and asked again ($HELD_OUT)"
    fi
    sleep 10
done
set +e
wait "$HELD_PID"
HELD_RC=$?
set -e
HELD_PID=""
exec 8>&-
echo "$HELD_RC" > "$STEP_DIR/held.rc"
cp "$HELD_OUT" "$STEP_DIR/held-final.out"
log "  RESTORE.sh exited $HELD_RC, $(( $(date +%s) - T5 )) s after the answer"
[ "$HELD_RC" = 0 ] || fail "the held RESTORE.sh run exited $HELD_RC ($HELD_OUT)"
for needle in "This cartridge identifies as: $LABEL2" \
    "It gives slice(s) $((K1 + 1)) to $N of '$UNIT_S'" \
    "positions its seal marker covers." \
    "Every slice of '$UNIT_S' is in the scratch directory" \
    "RESTORE COMPLETE"; do
    grep -F -- "$needle" "$HELD_OUT" >/dev/null || fail "RESTORE.sh's output lacks: $needle ($HELD_OUT)"
done
[ "$(grep -cF "$PROMPT" "$HELD_OUT")" = 1 ] || fail "RESTORE.sh asked for more than one cartridge ($HELD_OUT)"
if compgen -G "$SCRATCH/.tapectl-restore.*" >/dev/null; then fail "the scratch directory was left in $SCRATCH"; fi
log "  the manifest of the restored tree"
run manifest-restored 0 manifest "$RESTORE_TO" "$RUN/manifest-restored.tsv"
if ! cmp -s "$RUN/manifest-source.tsv" "$RUN/manifest-restored.tsv"; then
    diff "$RUN/manifest-source.tsv" "$RUN/manifest-restored.tsv" > "$STEP_DIR/manifest.diff" || true
    fail "the restored tree differs from the source's manifest ($STEP_DIR/manifest.diff)"
fi
log "  $UNIT_S restored identical to its source: $(sed -n 1p "$STEP_DIR/manifest-restored.out") (manifest sha256 $(sha256sum "$RUN/manifest-source.tsv" | awk '{print $1}'))"
end_step

# ------------------------------------------------------------
begin_step 6 close "the result, and the cleanup (nothing is erased here)"
cp -a "$DB" "$RUN/catalog-at-end.db"
{
    echo "RESULT: PASS"
    echo "every step passed: $LABEL1 sealed short after file $K with $UNIT_S a range of slices 1-$K1 of $N;"
    echo "RESTORE.sh held that range in scratch, took $LABEL2 by hash and restored $UNIT_S identical"
    echo "the cartridge now holds $LABEL2 ($UNIT_S whole) and belongs to $TAPECTL_HOME"
} > "$RUN/RESULT.txt"
[ -z "$FILLER_DIR" ] || rm -rf "$FILLER_DIR"
log "RESULT: PASS. $RUN"
log "The cartridge holds $LABEL2 ($UNIT_S whole), in the throwaway home. Before it is a test"
log "cartridge again, with THIS tapectl (an older installed one refuses this home's catalog):"
log "  $TAPECTL_BIN --home $TAPECTL_HOME snapshot mark-reclaimable $UNIT_S --version 1 --force"
log "  ($LABEL2 holds $UNIT_S's only copy, so retire is refused until then), then"
log "  $TAPECTL_BIN --home $TAPECTL_HOME volume retire $LABEL2, then"
log "  $TAPECTL_BIN --home $TAPECTL_HOME cartridge erase --device $TAPE_DEV --confirm-label $LABEL2"
log "The work directory holds the unit's copy and the restored tree: rm -rf $WORK"
log "once the run is read. The operator's source ${SPAN_SOURCE:-(generated)} was only read."
[ "$MHVTL" = 1 ] || log "Start the drive-poll timer again: sudo systemctl start tapectl-drive-poll.timer"
end_step
