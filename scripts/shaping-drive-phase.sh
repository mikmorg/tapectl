#!/usr/bin/env bash
# shaping-drive-phase.sh -- #488's real-drive measurement phase: the write-shaping note's §10 as a
# script (docs/research/2026-10-20-write-shaping.md; ADR-0012 item 117). The operator runs it on
# home2 inside tmux, with a research build the coordinator built for measuring only.
#
# WHAT IT DOES. ~20 GiB of synthetic data (incompressible) is staged once in a throwaway home, as
# two units: shp-unit and the small shp-restore (--restore-size, 1.5 GiB). Every run then writes
# both again (`--keep-staging`) onto the test cartridge as a fresh volume
# (SHP-01, SHP-02, ...): the previous volume retired and short-erased, a new one initialised,
# `host check`, and `volume write` with TAPECTL_PIPELINE_TRACE=1 and the run's TAPECTL_EXP_*
# knobs, under the run's load. The runs are §10's P0, R1, R2, R3, R5, R6-R10 and R12, with its
# stop rule. R4 (load L3, the java service) and R11 (CPU pinning) need the host owner and are
# NOT run (item 117). --print-plan says what each run is and how long it all takes.
#
# THE RESTORE-RATE LEG. After the last write, before its volume is retired: shp-restore restored
# from it by `tapectl restore unit` (the research binary is a release build, and tapectl's restore
# rate has been measured only in a debug build: 3.0 MiB/s on the 1.6.0 acceptance), then by the
# RESTORE.sh read off the tape, each restored tree checked against the source's tree checksum
# (taken at setup, the source then removed) and deleted. restore.tsv records both, with the read
# phase's line from the restore's session log.
#
# A FAIL STOPS THE RUN and leaves the cartridge as the failed command left it: nothing after it
# writes the medium. The run directory keeps every command's output, each write's session log and
# pipeline trace, its post-command sweep and feed-ratio event from the throwaway catalog, MAM and
# log pages 0x02 0x03 0x0c 0x17 0x34 0x14 before and after every write (never during one), each
# load's commands and times, and results.tsv, the derived figures.
#
# REFUSES unless every one of these holds (exit 2, nothing written):
#   - TAPECTL_HOME is set, and absent or empty (a fresh run) or a home this script made (it carries
#     SHAPING-DRIVE-PHASE: a rerun), and none of production's homes;
#   - the drive's serial is HUJ808A5L4 and the cartridge's chip serial EW7VWMVKF6, named again
#     with --i-will-lose-the-cartridge -- or --mhvtl, on an mhvtl drive, never that one;
#   - TAPECTL_TEST_EARLY_WARNING_AT and every TAPECTL_EXP_* are unset (the script sets the knobs
#     itself, per run);
#   - TAPECTL_BIN announces itself as a research build (this protocol measures one);
#   - tapectl-drive-poll.timer is not active, and the tape lock is free;
#   - the cartridge is blank, or holds a volume this home wrote. After the short-seal acceptance
#     run it holds SSA-0002, which only the acceptance's home can give up: clean it from there
#     first (--print-plan names the two commands).
#
# --mhvtl rehearses on a virtual drive: small sizes, and the loads skipped unless --loads.
#
# EXIT: 0 every run passed; 1 a run FAILED (the run stopped there); 2 a precondition refused.
set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
TEST_DRIVE_SERIAL="HUJ808A5L4"
TEST_CARTRIDGE="EW7VWMVKF6"
HOME2_SVC_HOME="/srv/archive_meta/tapectl"   # contrib/hosts/home2.profile, SVC_HOME_WANT
OPERATOR="shp-op"; TENANT="shp"; UNIT="shp-unit"; UNIT_VERSION=1
SENTINEL_NAME="SHAPING-DRIVE-PHASE"
REMOTE_DIR="/tmp/tapectl-shaping-load"       # the vm host's pidfiles for L1/L2
LADDER_THRESHOLD="1.3"                       # §10's stop rule: the first load at 1.3 or more is L*
# Every run, in the order a full run takes them (the stop rule decides which of the tail run).
ALL_RUNS="P0a P0b P0c R1 R2 R3 R5r R5w R6 R7 R8 R9 R10 R12a R12b R1-L2 R8-L2 R9-L2"

usage() {
    cat <<'USAGE'
usage: TAPECTL_HOME=<fresh dir> TAPECTL_BIN=<research tapectl> \
       scripts/shaping-drive-phase.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
           --i-will-lose-the-cartridge EW7VWMVKF6 [options]
       scripts/shaping-drive-phase.sh --mhvtl --device /dev/tape/by-id/scsi-XYZZY_A3-nst [options]
       scripts/shaping-drive-phase.sh --print-plan [--mhvtl]

#488's drive phase: about 15 writes of one staged unit onto the test cartridge, each under a
load and with the write-shaping knobs of one run of the note's §10. OVERWRITES THE CARTRIDGE.

  --device PATH                  Required. The drive's st node, by-id spelling. No default.
  --i-will-lose-the-cartridge S  Required on the real drive: EW7VWMVKF6, checked against MAM.
  --mhvtl                        The rehearsal on an mhvtl drive (small sizes, loads skipped).
  --loads                        --mhvtl only: run the loads too.
  --out DIR                      The run directory's parent. Default /scratch/tapectl-shaping
                                 where /scratch exists, else ~/.cache/tapectl/shaping.
  --staging DIR                  Where the staged unit, its source and the load files live
                                 (default /srv/google_copy/tapectl-shaping; md3, 67 GB free).
  --size SIZE                    The synthetic data, both units (default 20G; --mhvtl 96M).
  --restore-size SIZE            Of it, the unit the restore-rate leg restores (default 1536M,
                                 the acceptance's ssa-b-photos; --mhvtl 16M). 0: no such unit
                                 and no leg. A rerun keeps its home's; 0 then skips the leg.
  --load-file PATH               L5's read load, on the staging filesystem (default
                                 <staging>/load.bin), made once, reused at the right size.
  --vm-host HOST                 Where L1 and L2 run, over ssh (default vm-desk1).
  --only RUNS                    Only these runs, comma-separated (P0, R5 and R12 name their
                                 parts), in a fresh run directory; no stop rule.
  --lstar LOAD                   With --only: L* for R6-R10 and R12b (L1, L2, L5r or L5w);
                                 default: what the last full run decided.
  --winner RUN                   With --only: R12's lever (R6-R10); default: the last full run's.
  --cleanup                      After a passing run, release the staged unit and remove the
                                 load file.
  --print-plan                   Print what the operator does and the drive time, and exit.

  TAPECTL_HOME (env)             Required: the throwaway home.
  TAPECTL_BIN (env)              Required: the research build (--features research).
  DAR_BIN (env)                  dar for the throwaway config. Default: dar on PATH.
USAGE
}

die() { echo "shaping-drive-phase: REFUSED: $*" >&2; exit 2; }

TAPE_DEV=""; LOSE_SERIAL=""; OUT_DIR=""; MHVTL=0; LOADS=1; LOADS_ASKED=0; STAGING_ROOT=""
SIZE_ARG=""; RESTORE_ARG=""; LOAD_FILE=""; VM_HOST="vm-desk1"; ONLY=""; LSTAR_ARG=""; WINNER_ARG=""; CLEANUP=0
PRINT_PLAN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --mhvtl) MHVTL=1; shift ;;
        --loads) LOADS_ASKED=1; shift ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --staging) STAGING_ROOT="${2:-}"; shift 2 ;;
        --size) SIZE_ARG="${2:-}"; shift 2 ;;
        --restore-size) RESTORE_ARG="${2:-}"; shift 2 ;;
        --load-file) LOAD_FILE="${2:-}"; shift 2 ;;
        --vm-host) VM_HOST="${2:-}"; shift 2 ;;
        --only) ONLY="${2:-}"; shift 2 ;;
        --lstar) LSTAR_ARG="${2:-}"; shift 2 ;;
        --winner) WINNER_ARG="${2:-}"; shift 2 ;;
        --cleanup) CLEANUP=1; shift ;;
        --print-plan) PRINT_PLAN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "shaping-drive-phase: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[ "$LOADS_ASKED" = 0 ] || [ "$MHVTL" = 1 ] || die "--loads is for --mhvtl; the real drive's runs always run their loads"
[ "$MHVTL" = 0 ] || LOADS="$LOADS_ASKED"

# ---------- the two shapes of the run ----------
MiB=$((1024 * 1024)); GiB=$((1024 * MiB))
to_bytes() { python3 -c 'import re,sys
m = re.fullmatch(r"\s*(\d+)\s*([KMGT]?)(?:i?B)?\s*", sys.argv[1], re.I)
print(int(m.group(1)) << {"": 0, "K": 10, "M": 20, "G": 30, "T": 40}[m.group(2).upper()] if m else "")' "$1"; }
if [ -z "$OUT_DIR" ]; then
    if [ -d /scratch ]; then OUT_DIR="/scratch/tapectl-shaping"; else OUT_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl/shaping"; fi
fi
if [ "$MHVTL" = 1 ]; then
    : "${SIZE_ARG:=96M}"; : "${RESTORE_ARG:=16M}"; SLICE_SIZE="8M"; PIECE=$((8 * MiB)); LOAD_MIB=256; WRITER_MIB=256
    : "${STAGING_ROOT:=$OUT_DIR/mhvtl-staging}"
else
    : "${SIZE_ARG:=20G}"; : "${RESTORE_ARG:=1536M}"; SLICE_SIZE="1G"; PIECE=$GiB; LOAD_MIB=20000; WRITER_MIB=20000
    : "${STAGING_ROOT:=/srv/google_copy/tapectl-shaping}"
fi
SIZE="$(to_bytes "$SIZE_ARG")"
[ -n "$SIZE" ] && [ "$SIZE" -gt 0 ] || die "--size '$SIZE_ARG' is not a size (20G, 512M, bytes)"
RESTORE_SIZE="$(to_bytes "$RESTORE_ARG")"
[ -n "$RESTORE_SIZE" ] || die "--restore-size '$RESTORE_ARG' is not a size (1536M, 0)"
[ "$RESTORE_SIZE" -lt "$SIZE" ] || die "--restore-size $RESTORE_ARG is not less than --size $SIZE_ARG: it is part of it"
RESTORE_UNIT="shp-restore"
: "${LOAD_FILE:=$STAGING_ROOT/load.bin}"
WRITER_FILE="$STAGING_ROOT/w.bin"

# The runs: each one's load and knobs. "Baseline" is the note's R1, today's pipeline when it was
# written: the sha2 hash and the 256 MiB queue (1.6.0 production hashes with ring).
BASE="TAPECTL_EXP_HASH=sha2"
knobs_for() {
    case "$1" in
        P0a|P0b|P0c|R1|R2|R3|R5r|R5w|R1-L2) echo "$BASE" ;;
        R6) echo "$BASE TAPECTL_EXP_QUEUE_BYTES=4G" ;;
        R7) echo "$BASE TAPECTL_EXP_PACE_MBPS=110 TAPECTL_EXP_QUEUE_BYTES=4G TAPECTL_EXP_CROSSFILE=1" ;;
        R8|R8-L2) echo "TAPECTL_EXP_HASH=ring TAPECTL_EXP_QUEUE_BYTES=4G TAPECTL_EXP_CROSSFILE=1" ;;
        R9|R9-L2) echo "TAPECTL_EXP_HASH=ring TAPECTL_EXP_PACE_MBPS=140 TAPECTL_EXP_QUEUE_BYTES=4G TAPECTL_EXP_CROSSFILE=1" ;;
        R10) echo "TAPECTL_EXP_HASH=ring TAPECTL_EXP_QUEUE_BYTES=4G TAPECTL_EXP_CROSSFILE=1 TAPECTL_EXP_PACE_ADAPTIVE=90,150,10 TAPECTL_EXP_REFILL=0.10,0.90" ;;
        R12a|R12b) knobs_for "$WINNER" ;;
    esac
}
load_for() {
    case "$1" in
        P0a|P0c|R1|R12a) echo L0 ;;
        P0b|R3|R1-L2|R8-L2|R9-L2) echo L2 ;;
        R2) echo L1 ;;
        R5r) echo L5r ;;
        R5w) echo L5w ;;
        R6|R7|R8|R9|R10|R12b) echo "${LSTAR:-}" ;;
    esac
}

print_plan() {
    local gib rgib
    gib="$(awk -v b="$SIZE" 'BEGIN { printf "%.1f", b / 1073741824 }')"
    rgib="$(awk -v b="$RESTORE_SIZE" 'BEGIN { printf "%.2f", b / 1073741824 }')"
    cat <<PLAN
=== #488's drive phase: the write-shaping note's §10 (ADR-0012 item 117) ===

BEFORE YOU START (the operator):
  1. Read the short-seal acceptance run's result first. Then clean its cartridge FROM ITS HOME
     (it holds SSA-0002, which only that home can give up):
       tapectl --home <the acceptance home> --yes volume retire SSA-0002
       tapectl --home <the acceptance home> cartridge erase --device <this drive> --confirm-label SSA-0002
  2. Leave the test cartridge EW7VWMVKF6 loaded in the HP LTO-6 (HUJ808A5L4). Every run writes it.
  3. Nothing else on the drive for the whole run:  sudo systemctl stop tapectl-drive-poll.timer
     (start it again afterwards). vm-desk1 quiet but for the loads this script starts there: the
     coordinator tells homorg and lcsas-4c.
  4. Run it inside tmux. A FAIL stops the run and leaves the cartridge as it is.

SETUP, once (no drive; about 10 minutes): a throwaway home; $gib GiB of synthetic data from
/dev/urandom under $STAGING_ROOT as two units, $UNIT and $RESTORE_UNIT ($rgib GiB of it), staged
once in $SLICE_SIZE slices (their sources then removed, $RESTORE_UNIT's tree checksum kept);
the load file $LOAD_FILE ($LOAD_MIB MiB, made once, reused at the right size).

EACH RUN, about 8 minutes of drive time: the previous volume retired and short-erased, a new
SHP-NN initialised, host check, then volume write --keep-staging --yes with TAPECTL_PIPELINE_TRACE=1
and the run's knobs (the seal and the quick confirm are part of it). Log pages and MAM are read
just before and just after the write, never during it.

THE RUNS ("baseline" = R1's lever: TAPECTL_EXP_HASH=sha2, the 256 MiB queue)
  P0a P0b P0c  L0, L2, L0  baseline   page 0x34's semantics. The protocol's own data, written three
                                      times, not three ~7 GiB writes: volume write writes every
                                      staged set, so a smaller set would have to be staged and
                                      released around P0; the same question, about 5 more minutes.
  R1   L0   baseline                  the quiet baseline
  R2   L1   baseline                  ladder step 1 (vm-desk1, 8 x sha256sum)
  R3   L2   baseline                  ladder step 2 (vm-desk1, 16 x sha256sum)
  R5r  L5r  baseline                  ladder: staging reads (dd iflag=direct of the load file)
  R5w  L5w  baseline                  ladder: staging writes (dd oflag=direct, $WRITER_MIB MiB)
  STOP RULE: the first ladder run with a feed ratio of $LADDER_THRESHOLD or more names L*, and the
  ladder stops there. If none does: R1, R8 and R9 run under L2 (R1-L2, R8-L2, R9-L2), and the
  phase ends.
  R6   L*   sha2, queue 4 GiB                         no gain without headroom
  R7   L*   sha2, pace 110, queue 4 GiB, cross-file   pacing alone
  R8   L*   ring, queue 4 GiB, cross-file             headroom alone
  R9   L*   ring, pace 140, queue 4 GiB, cross-file   both
  R10  L*   ring, queue 4 GiB, cross-file, adaptive 90-150 MB/s in 10% steps, refill 0.10/0.90
            (the table names no queue for R10; it takes R9's)
  R12a L0 / R12b L*   the winner repeated: of R6-R10, the lowest feed ratio, then the fewest
                      repositions per 100 MB
  NOT RUN: R4 (L3, the java service) and R11 (CPU pinning): both need the host owner (item 117).
  RESTORE-RATE LEG, once, after the last write and before its volume is retired (no load, about
  10 minutes at most): $RESTORE_UNIT ($rgib GiB) restored by tapectl restore unit (a release build's
  rate; the 1.6.0 acceptance's debug build read 3.0 MiB/s), then by RESTORE.sh --restore off the
  tape; each tree checked against the source's checksum, then deleted. Into restore.tsv.
  --restore-size 0 makes no such unit and skips the leg.
  The L5 loads loop until the write ends; L1 and L2 are §10's commands, stopped when it ends.

DRIVE TIME: P0 and up to 12 runs, about 8 minutes each: about 2 hours (less if the ladder stops
early), then the restore leg's two reads of $rgib GiB. Every write is ~$gib GiB; the cartridge is
left short-erased (File 0 EMPTY) when it passes.

THE COMMAND (operator's login on home2, inside tmux):
  TAPECTL_HOME=\$HOME/.cache/tapectl/shp-home-\$(date +%Y%m%d) TAPECTL_BIN=<the research build> \\
    scripts/shaping-drive-phase.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \\
      --i-will-lose-the-cartridge EW7VWMVKF6
  Rerun a few runs later, in the same home: add --only R8,R9 (and --lstar/--winner if needed).
PLAN
}
if [ "$PRINT_PLAN" = 1 ]; then print_plan; exit 0; fi

# ---------- the runs selected ----------
SELECTED=""
if [ -n "$ONLY" ]; then
    for r in ${ONLY//,/ }; do
        case "$r" in
            P0) SELECTED+=" P0a P0b P0c" ;;
            R5) SELECTED+=" R5r R5w" ;;
            R12) SELECTED+=" R12a R12b" ;;
            R4|R11) die "--only $r: R4 and R11 are not run in this phase (ADR-0012 item 117)" ;;
            *) case " $ALL_RUNS " in *" $r "*) SELECTED+=" $r" ;; *) die "--only: no run '$r' (runs: $ALL_RUNS)" ;; esac ;;
        esac
    done
else
    [ -z "$LSTAR_ARG$WINNER_ARG" ] || die "--lstar and --winner go with --only; a full run decides them"
fi
selected() { [ -z "$ONLY" ] || case " $SELECTED " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ---------- preconditions: nothing below writes the drive until every one holds ----------
[ -n "$TAPE_DEV" ] || { usage >&2; die "--device is required; there is no default (/dev/nstN numbering moves)"; }
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
for bin in mt dd sg_read_attr sg_logs python3 flock dar sha256sum awk tee df setsid timeout head; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
SG_LOGS_HELP="$(sg_logs --help 2>&1 || true)"
for opt in '--maxlen' '--in=' '--pdt' '--raw' '--page'; do
    case "$SG_LOGS_HELP" in *"$opt"*) ;; *) die "this sg_logs does not list the $opt option; the log-page reads would fail" ;; esac
done
[ -z "${TAPECTL_TEST_EARLY_WARNING_AT+set}" ] || die "TAPECTL_TEST_EARLY_WARNING_AT is set: a measurement with the test hook measures nothing. Unset it"
if compgen -e | grep -E '^TAPECTL_EXP_' >/dev/null; then
    die "$(compgen -e | grep -E '^TAPECTL_EXP_' | tr '\n' ' ')is set: the script sets the knobs itself, per run. Unset them"
fi
[ -n "${TAPECTL_BIN:-}" ] || die "TAPECTL_BIN is required: the research build (this script never builds one)"
[ -x "$TAPECTL_BIN" ] || die "TAPECTL_BIN=$TAPECTL_BIN is not an executable file"
TAPECTL_BIN="$(readlink -f "$TAPECTL_BIN")"
: "${DAR_BIN:=$(command -v dar)}"
[ -x "$DAR_BIN" ] || die "DAR_BIN=$DAR_BIN is not an executable file"
TAPECTL_VERSION="$("$TAPECTL_BIN" --version 2>&1 | sed -n 1p)"
python3 - "$TAPECTL_VERSION" <<'PY' || die "TAPECTL_BIN reports '$TAPECTL_VERSION': this phase needs tapectl 1.6.0 or later (the trace, the 0x34 decoder)"
import re, sys
m = re.search(r"(\d+)\.(\d+)\.(\d+)", sys.argv[1])
sys.exit(0 if m and tuple(map(int, m.groups())) >= (1, 6, 0) else 1)
PY
if [ -n "$ONLY" ]; then
    for r in $SELECTED; do
        case "$r" in R6|R7|R8|R9|R10|R12b) NEED_LSTAR=1 ;; esac
        case "$r" in R12a|R12b) NEED_WINNER=1 ;; esac
    done
    [ -z "$LSTAR_ARG" ] || case "$LSTAR_ARG" in L1|L2|L5r|L5w) ;; *) die "--lstar is L1, L2, L5r or L5w (got '$LSTAR_ARG')" ;; esac
    [ -z "$WINNER_ARG" ] || case "$WINNER_ARG" in R6|R7|R8|R9|R10) ;; *) die "--winner is one of R6-R10 (got '$WINNER_ARG')" ;; esac
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
    case "$HOME_C/" in "$pc"/*) die "TAPECTL_HOME=$TAPECTL_HOME is (or is inside) production's home $p" ;; esac
    case "$pc/" in "$HOME_C"/*) die "TAPECTL_HOME=$TAPECTL_HOME contains production's home $p" ;; esac
done
CFG="$TAPECTL_HOME/config.toml"; DB="$TAPECTL_HOME/tapectl.db"; STATE="$TAPECTL_HOME/shaping-state"
FRESH=1
if [ -e "$TAPECTL_HOME" ]; then
    [ -d "$TAPECTL_HOME" ] || die "TAPECTL_HOME=$TAPECTL_HOME exists and is not a directory"
    if [ -n "$(ls -A "$TAPECTL_HOME")" ]; then
        [ -f "$TAPECTL_HOME/$SENTINEL_NAME" ] && [ -f "$DB" ] \
            || die "TAPECTL_HOME=$TAPECTL_HOME is not empty and is not a home this script made (no $SENTINEL_NAME)"
        FRESH=0
    fi
fi
# A read-only catalog query (python's sqlite3, ?mode=ro; the suite's idiom).
dbq() { # <db> <sql> [params...]: rows, tab-separated
    python3 - "$@" <<'PY'
import sqlite3, sys
db, sql, args = sys.argv[1], sys.argv[2], sys.argv[3:]
con = sqlite3.connect("file:%s?mode=ro" % db, uri=True, timeout=30)
for row in con.execute(sql, args):
    print("\t".join("" if v is None else str(v) for v in row))
PY
}
staged_sets() { dbq "$DB" "SELECT COUNT(*) FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE u.name = ? AND ss.status = 'staged'" "$1"; }
UNITS="$UNIT"; [ "$RESTORE_SIZE" = 0 ] || UNITS="$RESTORE_UNIT $UNIT"
if [ "$FRESH" = 0 ]; then
    [ "$(cat "$STATE/staging-root" 2>/dev/null)" = "$STAGING_ROOT" ] \
        || die "this home staged under '$(cat "$STATE/staging-root" 2>/dev/null)', not --staging $STAGING_ROOT"
    # The units and the restore unit's size are the home's, from its setup; --restore-size 0 only
    # skips the leg.
    HOME_RESTORE_SIZE="$(cat "$STATE/restore-size" 2>/dev/null || echo 0)"
    UNITS="$UNIT"; [ "$HOME_RESTORE_SIZE" = 0 ] || UNITS="$RESTORE_UNIT $UNIT"
    [ "$RESTORE_SIZE" = 0 ] || RESTORE_SIZE="$HOME_RESTORE_SIZE"
    for u in $UNITS; do
        [ "$(staged_sets "$u")" = 1 ] || die "this home's unit $u is no longer staged (--cleanup ran?): start again in a fresh TAPECTL_HOME"
    done
    [ "$RESTORE_SIZE" = 0 ] || [ -s "$STATE/restore-source.tree" ] || die "this home has no tree checksum of $RESTORE_UNIT's source ($STATE/restore-source.tree)"
fi
if [ "$RESTORE_SIZE" != 0 ]; then
    for bin in age tar diff tr; do   # RESTORE.sh's tools, as an heir has them
        command -v "$bin" >/dev/null || die "required binary missing for the restore-rate leg: $bin (or --restore-size 0)"
    done
fi
if [ -n "$ONLY" ]; then
    LSTAR="${LSTAR_ARG:-$(cat "$STATE/lstar" 2>/dev/null || true)}"
    WINNER="${WINNER_ARG:-$(cat "$STATE/winner" 2>/dev/null || true)}"
    [ -z "${NEED_LSTAR:-}" ] || [ -n "$LSTAR" ] || die "--only $ONLY needs L*: give --lstar (no earlier full run in this home decided it)"
    [ -z "${NEED_WINNER:-}" ] || [ -n "$WINNER" ] || die "--only $ONLY needs the winner: give --winner (no earlier full run decided it)"
fi

# ---------- the staging filesystem: room for what is not there yet ----------
existing_dir() { local d="$1"; while [ ! -d "$d" ]; do d="$(dirname "$d")"; done; echo "$d"; }
SFS="$(existing_dir "$STAGING_ROOT")"
[ -w "$SFS" ] || die "$SFS (for --staging $STAGING_ROOT) is not writable by $(id -un)"
[ "$(stat -c %d "$SFS")" = "$(stat -c %d "$(existing_dir "$(dirname "$LOAD_FILE")")")" ] \
    || die "--load-file $LOAD_FILE is not on the staging filesystem ($SFS): L5 loads the staging disks"
# The peak is the larger of setup (the source beside its staging) and R5w (staging, the load file
# and the writer's file); the source is gone before either load file exists.
if [ "$MHVTL" = 1 ]; then MARGIN=$((64 * MiB)); else MARGIN=$((2 * GiB)); fi
STAGED_NEW=0; SRC_NEW=0; LOAD_NEW=0; WRITER_NEW=0
[ "$FRESH" = 0 ] || { STAGED_NEW=$((SIZE / 100 * 105)); SRC_NEW=$SIZE; }
[ "$(stat -c %s "$LOAD_FILE" 2>/dev/null || echo 0)" = $((LOAD_MIB * MiB)) ] || LOAD_NEW=$((LOAD_MIB * MiB))
! selected R5w || WRITER_NEW=$((WRITER_MIB * MiB))
NEED=$((SRC_NEW + STAGED_NEW)); [ $((STAGED_NEW + LOAD_NEW + WRITER_NEW)) -le "$NEED" ] || NEED=$((STAGED_NEW + LOAD_NEW + WRITER_NEW))
# The restore-rate leg restores into the staging filesystem, one tree at a time; RESTORE.sh's
# decrypted slices sit beside its tree, so about twice the unit.
RESTORE_PEAK=$((STAGED_NEW + LOAD_NEW + RESTORE_SIZE / 100 * 220))
[ "$RESTORE_PEAK" -le "$NEED" ] || NEED=$RESTORE_PEAK
NEED=$((NEED + MARGIN))
AVAIL="$(df --output=avail -B1 "$SFS" | tail -n 1 | tr -d ' ')"
[ "$AVAIL" -ge "$NEED" ] || die "$SFS has $((AVAIL / MiB)) MiB free; the unit, its staging and the load files need $((NEED / MiB)) MiB at their peak"

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
[ "$GEN_N" = 6 ] || die "the drive's model '$SYS_MODEL' is not an LTO-6 drive: this phase measures the HP LTO-6"
if [ "$MHVTL" = 1 ]; then
    [ "$DRIVE_SERIAL" != "$TEST_DRIVE_SERIAL" ] || die "--mhvtl names the REAL drive $TEST_DRIVE_SERIAL: the rehearsal runs on mhvtl only"
    case "$DRIVE_SERIAL" in XYZZY_*) ;; *) die "--mhvtl: $TAPE_DEV has serial '$DRIVE_SERIAL', not an mhvtl drive (XYZZY_*)" ;; esac
else
    [ "$DRIVE_SERIAL" = "$TEST_DRIVE_SERIAL" ] || die "the drive's serial is '$DRIVE_SERIAL', not the validation drive $TEST_DRIVE_SERIAL (give --mhvtl for the rehearsal)"
    [ -n "$LOSE_SERIAL" ] || die "--i-will-lose-the-cartridge $TEST_CARTRIDGE is required: every run overwrites the loaded cartridge"
    [ "$LOSE_SERIAL" = "$TEST_CARTRIDGE" ] || die "--i-will-lose-the-cartridge names '$LOSE_SERIAL': this phase is for the test cartridge $TEST_CARTRIDGE only"
fi
if [ "$MHVTL" = 1 ]; then TAPE_LOCK="/tmp/tapectl-tape.lock.$ST_NODE"; else TAPE_LOCK="/tmp/tapectl-tape.lock"; fi
exec 9>"$TAPE_LOCK"
flock -n 9 || die "another process holds the tape lock ($TAPE_LOCK)"
read_serial() { # the ANCHORED match (lto-profile.sh): a loose grep finds "Density vendor/serial" first
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
# L1 and L2 run on the vm host over ssh, as home2 reaches vm-desk1.
needs_vm() { for r in ${SELECTED:-$ALL_RUNS}; do case "$(LSTAR=${LSTAR:-L2} load_for "$r")" in L1|L2) return 0 ;; esac; done; return 1; }
if [ "$LOADS" = 1 ] && needs_vm; then
    ssh -o BatchMode=yes -o ConnectTimeout=10 "$VM_HOST" true </dev/null 2>/dev/null \
        || die "cannot reach $VM_HOST with 'ssh -o BatchMode=yes' for loads L1 and L2 (--vm-host)"
fi

# ---------- the run directory, and the two probes (read only) ----------
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN="$OUT_DIR/shp-$STAMP-$(printf '%s' "$SERIAL" | tr -c 'A-Za-z0-9_.-' '_')"
mkdir -p "$RUN" || die "cannot create $RUN"
export TAPECTL_HOME_POINTER="$RUN/no-home-pointer"
TEE_OPT=(); if tee --output-error=warn </dev/null >/dev/null 2>&1; then TEE_OPT=(--output-error=warn); fi
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${TEE_OPT[@]}" "$RUN/run.log"; }
json_get() { # <file> <python expression on d>
    python3 - "$1" "$2" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
v = eval(sys.argv[2], {"d": d})
print("" if v is None else (json.dumps(v) if isinstance(v, (list, dict)) else v))
PY
}
probe() { "$TAPECTL_BIN" --home "$1" "${@:2}" </dev/null; }
PRE="$RUN/0-preflight"; PROBE_HOME="$PRE/probe-home"; mkdir -p "$PROBE_HOME"
# The research build announces itself first thing in volume write, before it resolves a drive: a
# home with no drive configured gets the banner and a refusal, and nothing is opened.
probe "$PROBE_HOME" init --operator shp-probe --no-escrow >"$PRE/probe-init.out" 2>&1 \
    || die "TAPECTL_BIN could not make a probe home (see $PRE/probe-init.out)"
probe "$PROBE_HOME" volume write SHP-PROBE >"$PRE/probe-write.out" 2>"$PRE/probe-write.err" || true
grep -F "RESEARCH BUILD" "$PRE/probe-write.err" >/dev/null \
    || die "TAPECTL_BIN does not announce a research build: this protocol measures a research build (cargo build --release --features research; see $PRE/probe-write.err)"
RESEARCH_BANNER="$(grep -F "RESEARCH BUILD" "$PRE/probe-write.err" | sed -n 1p)"
# The cartridge: blank, or a volume this home wrote (read-only probe: cartridge erase --dry-run).
PREV=""
erase_probe() { # <home> <out base>: RC, PROBE_OUTCOME, PROBE_CONFIRM
    set +e; probe "$1" --json cartridge erase --device "$TAPE_DEV" --dry-run >"$2.out" 2>"$2.err"; RC=$?; set -e
    PROBE_OUTCOME=""; PROBE_CONFIRM=""
    if [ "$RC" = 0 ]; then
        PROBE_OUTCOME="$(json_get "$2.out" 'd["outcome"]')"; PROBE_CONFIRM="$(json_get "$2.out" 'd.get("confirm_label")')"
    fi
}
cleanup_hint="clean it from the home that wrote it first. After the short-seal acceptance run that is
  tapectl --home <the acceptance home> --yes volume retire SSA-0002
  tapectl --home <the acceptance home> cartridge erase --device $TAPE_DEV --confirm-label SSA-0002"
if [ "$FRESH" = 1 ]; then
    probe "$PROBE_HOME" backend add --name probe --device-tape "$TAPE_DEV" --generation LTO-6 >"$PRE/probe-backend.out" 2>&1 \
        || die "the probe home could not add the drive (see $PRE/probe-backend.out)"
    erase_probe "$PROBE_HOME" "$PRE/erase-probe"
    [ "$RC" = 0 ] || die "cartridge erase --dry-run refused in a fresh catalog (see $PRE/erase-probe.err)"
    [ "$PROBE_OUTCOME" = nothing_to_erase ] \
        || die "the cartridge holds volume '${PROBE_CONFIRM:-?}', which no home of this run wrote: $cleanup_hint"
else
    erase_probe "$TAPECTL_HOME" "$PRE/erase-probe"
    LIVE="$(dbq "$DB" "SELECT label FROM volumes WHERE label LIKE 'SHP-%' AND status NOT IN ('retired', 'erased') ORDER BY id DESC LIMIT 1")"
    if [ "$RC" = 0 ] && [ "$PROBE_OUTCOME" = nothing_to_erase ]; then
        PREV=""
    elif [ "$RC" = 0 ] && [ -n "$PROBE_CONFIRM" ] && [ -n "$(dbq "$DB" "SELECT label FROM volumes WHERE label = ?" "$PROBE_CONFIRM")" ]; then
        PREV="$PROBE_CONFIRM"
    elif [ "$RC" != 0 ] && [ -n "$LIVE" ] && grep -F "holds volume \"$LIVE\" (matched by uuid)" "$PRE/erase-probe.err" >/dev/null; then
        PREV="$LIVE"
    else
        die "the cartridge is neither blank nor a volume this home wrote (see $PRE/erase-probe.out and .err): $cleanup_hint"
    fi
fi
rm -rf "$PROBE_HOME"

# ============================================================ from here a FAIL stops the run
trap '' HUP
INTERRUPTED=0; trap 'INTERRUPTED=1' INT TERM
STEP="preflight"; STEP_DIR="$PRE"
LOAD_KIND=""; LOAD_SSH_PID=""; LOAD_MARK=""; LOAD_PIDFILE=""; LOAD_LOCAL_PIDS=(); TAPECTL_PREFIX=()
fail() {
    log "FAIL ($STEP): $*"
    stop_load || true
    {
        echo "RESULT: FAIL"; echo "step: $STEP"; echo "why: $*"
        echo "The run stopped here. The cartridge is as this step left it: read the failure before"
        echo "anything writes it again. Everything is in $RUN."
    } > "$RUN/RESULT.txt"
    snapshot_drive "$STEP_DIR" "at-fail" || true
    cp -a "$DB" "$RUN/catalog-at-fail.db" 2>/dev/null || true
    derive_results || true
    echo "shaping-drive-phase: FAIL at $STEP: $*" >&2
    echo "shaping-drive-phase: the run directory is $RUN" >&2
    exit 1
}
trap 'stop_load || true' EXIT

# ---------- the drive's state, read-only (lto-profile.sh's page; never 0x2E, read-to-clear) ----------
page() { # <dir> <tag> <page>
    sg_logs --page="$3" --maxlen=65532 --raw "$DRIVE_SG" > "$1/$2.page_$3.bin" 2>"$1/$2.page_$3.err" \
        && sg_logs --in="$1/$2.page_$3.bin" --raw --pdt=1 > "$1/$2.page_$3.txt" 2>&1
}
snapshot_drive() { # <dir> <tag>: never during a write
    local d="$1/drive-$2" f pg
    mkdir -p "$d"
    cat "$SYS_ST/options" > "$d/options.txt" 2>&1 || true
    for f in "$SYS_ST"/stats/*; do
        [ -f "$f" ] && printf '%s\t%s\n' "$(basename "$f")" "$(cat "$f" 2>/dev/null)" >> "$d/stats.tsv"
    done
    sg_read_attr "$DRIVE_SG" > "$d/mam.txt" 2>&1 || true
    for pg in 0x02 0x03 0x0c 0x17 0x34 0x14; do page "$d" snap "$pg" || true; done
    cat /proc/loadavg > "$d/loadavg.txt" 2>&1 || true
}

# run <name> <expected rc> <cmd...>: stdout to <name>.out, stderr to <name>.err and the terminal,
# the command line to commands.log, its seconds to <name>.seconds. FAILs on any other rc.
RC=0
run() {
    local name="$1" want="$2" t0; shift 2
    [ "$INTERRUPTED" = 0 ] || fail "interrupted by the operator"
    printf '+ %q' "$@" >> "$RUN/commands.log"; echo >> "$RUN/commands.log"
    log "  \$ $*"
    t0=$SECONDS
    set +e
    # tee ignores Ctrl-C, so a write it carries can finish its interrupt and still print.
    { "$@" 2>&1 1>&3 3>&- | (trap '' INT; exec tee "${TEE_OPT[@]}" "$STEP_DIR/$name.err") >&2; } 3>"$STEP_DIR/$name.out"
    RC="${PIPESTATUS[0]}"
    set -e
    echo "$RC" > "$STEP_DIR/$name.rc"; echo $((SECONDS - t0)) > "$STEP_DIR/$name.seconds"
    if [ "$want" != "any" ] && [ "$RC" != "$want" ]; then
        fail "$name exited $RC, expected $want (see $STEP_DIR/$name.err and .out)"
    fi
}
tctl() {
    env -u TAPECTL_TEST_EARLY_WARNING_AT -u TAPECTL_PIPELINE_TRACE \
        "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null
}
tctl_write() { # <knobs> <args...>: the write, traced, with the run's knobs (and L4's taskset)
    local knobs="$1"; shift
    # shellcheck disable=SC2086 # the knobs are VAR=VALUE words, no spaces in a value
    env -u TAPECTL_TEST_EARLY_WARNING_AT TAPECTL_PIPELINE_TRACE=1 $knobs \
        "${TAPECTL_PREFIX[@]}" "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null
}
begin_step() { # <dir name> <what>
    STEP="$1"; STEP_DIR="$RUN/$1"; mkdir -p "$STEP_DIR"
    log "=== $1 -- $2"; STEP_T0="$(date +%s)"
}
end_step() {
    local secs=$(( $(date +%s) - STEP_T0 ))
    log "=== $STEP: PASS ($((secs / 60))m $((secs % 60))s)"
    printf '%s\tPASS\t%s\n' "$STEP" "$secs" >> "$RUN/steps.tsv"
}

# ---------- the loads (§10's table); each one's commands and times into the run's load.txt ----------
note_load() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*" >> "$STEP_DIR/load.txt"; }
start_load() { # <load> <run id>
    local load="$1" n cmd
    LOAD_KIND=""; TAPECTL_PREFIX=()
    if [ "$load" != L0 ] && [ "$LOADS" = 0 ]; then note_load "$load SKIPPED (--mhvtl without --loads): this run is L0"; return; fi
    case "$load" in
        L0) note_load "L0: no load" ;;
        L1|L2)
            if [ "$load" = L1 ]; then n=8; else n=16; fi
            LOAD_MARK="shp-$STAMP-$2"
            cmd="mkdir -p $REMOTE_DIR && f=$REMOTE_DIR/$LOAD_MARK.pids && : > \$f && for i in \$(seq $n); do timeout 900 sha256sum /dev/zero & echo \$! >> \$f; done; wait"
            note_load "start $load on $VM_HOST: ssh -o BatchMode=yes $VM_HOST '$cmd'"
            ssh -o BatchMode=yes "$VM_HOST" "$cmd" </dev/null >"$STEP_DIR/load.out" 2>&1 &
            LOAD_SSH_PID=$!; LOAD_KIND="$load"
            ssh -o BatchMode=yes -o ConnectTimeout=5 "$VM_HOST" cat /proc/loadavg </dev/null >> "$STEP_DIR/load.txt" 2>&1 || true ;;
        L4)
            TAPECTL_PREFIX=(taskset -c 6,7)
            note_load "start L4: tapectl under taskset -c 6,7; taskset -c 18 timeout 900 sha256sum /dev/zero; the same on 19"
            taskset -c 18 timeout 900 sha256sum /dev/zero >/dev/null 2>&1 & LOAD_LOCAL_PIDS+=("$!")
            taskset -c 19 timeout 900 sha256sum /dev/zero >/dev/null 2>&1 & LOAD_LOCAL_PIDS+=("$!")
            LOAD_KIND="$load" ;;
        L5r|L5w)
            LOAD_PIDFILE="$STEP_DIR/load.pgid"
            if [ "$load" = L5r ]; then
                cmd="dd if=$LOAD_FILE of=/dev/null bs=1M iflag=direct"
            else
                cmd="dd if=/dev/zero of=$WRITER_FILE bs=1M count=$WRITER_MIB oflag=direct"
            fi
            note_load "start $load: '$cmd', repeated until the write ends"
            # Its own session, so the whole loop (dd included) stops with one group kill.
            # shellcheck disable=SC2086 # the command's words; no path here holds a space
            setsid bash -c 'echo $$ > "$0"; while :; do "$@" status=none || exit 1; done' \
                "$LOAD_PIDFILE" $cmd </dev/null >"$STEP_DIR/load.out" 2>&1 &
            LOAD_KIND="$load" ;;
        *) fail "no load '$load'" ;;
    esac
    [ -z "$LOAD_KIND" ] || sleep 2   # the load is running before host check and the write
}
stop_load() {
    local k="$LOAD_KIND" i
    [ -n "$k" ] || return 0
    LOAD_KIND=""
    case "$k" in
        L1|L2)
            ssh -o BatchMode=yes -o ConnectTimeout=10 "$VM_HOST" \
                "f=$REMOTE_DIR/$LOAD_MARK.pids; if [ -f \$f ]; then xargs -r kill -TERM < \$f; rm -f \$f; fi" </dev/null >>"$STEP_DIR/load.out" 2>&1 || true
            for i in $(seq 30); do kill -0 "$LOAD_SSH_PID" 2>/dev/null || break; sleep 1; done
            kill "$LOAD_SSH_PID" 2>/dev/null || true
            ssh -o BatchMode=yes -o ConnectTimeout=5 "$VM_HOST" cat /proc/loadavg </dev/null >> "$STEP_DIR/load.txt" 2>&1 || true ;;
        L4) kill -TERM "${LOAD_LOCAL_PIDS[@]}" 2>/dev/null || true; LOAD_LOCAL_PIDS=(); TAPECTL_PREFIX=() ;;
        L5r|L5w)
            for i in $(seq 10); do [ -s "$LOAD_PIDFILE" ] && break; sleep 1; done
            [ -s "$LOAD_PIDFILE" ] && kill -TERM -- "-$(cat "$LOAD_PIDFILE")" 2>/dev/null || true
            [ "$k" != L5w ] || rm -f "$WRITER_FILE" ;;
    esac
    note_load "stop $k"
}

# ---------- what a run records, and the figures derived from it ----------
collect_run() { # <label>
    local label="$1" logs slog contact
    logs=("$TAPECTL_HOME"/logs/*-volume-write-"$label"-*.log)
    [ -e "${logs[0]}" ] || fail "no session log for volume write $label under $TAPECTL_HOME/logs"
    slog="${logs[${#logs[@]}-1]}"
    cp "$slog" "$STEP_DIR/session.log"
    [ -f "${slog%.log}.pipeline.tsv" ] || fail "the write of $label left no pipeline trace (${slog%.log}.pipeline.tsv)"
    cp "${slog%.log}.pipeline.tsv" "$STEP_DIR/pipeline.tsv"
    dbq "$DB" "SELECT e.timestamp, e.new_value, e.details FROM events e JOIN volumes v ON v.id = e.entity_id
        WHERE e.action = 'write_feed_ratio' AND e.entity_type = 'volume' AND v.label = ? ORDER BY e.id" "$label" > "$STEP_DIR/event.tsv"
    [ -s "$STEP_DIR/event.tsv" ] || fail "no write_feed_ratio event for $label"
    tail -1 "$STEP_DIR/event.tsv" | cut -f2 > "$STEP_DIR/ratio.txt"
    contact="$(tail -1 "$STEP_DIR/event.tsv" | cut -f3 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("contact_id", ""))')"
    [ -z "$contact" ] || python3 - "$DB" "$contact" "$STEP_DIR" <<'PY'
import sqlite3, sys
db, contact, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
con = sqlite3.connect("file:%s?mode=ro" % db, uri=True, timeout=30)
with open(out + "/sweep.tsv", "w") as tsv, open(out + "/sweep-decoded.txt", "w") as txt:
    tsv.write("id\tcaptured_at\ttrigger\tpage\tok\terror\traw_hex\n")
    for r in con.execute("SELECT id, captured_at, trigger, page_code, ok, error, hex(raw), decoded"
                         " FROM log_page_journal WHERE contact_id = ? ORDER BY id", (contact,)):
        cells = ["" if v is None else str(v).replace("\t", " ").replace("\n", " ") for v in r[:7]]
        cells[3] = "0x%02x" % r[3]
        tsv.write("\t".join(cells) + "\n")
        txt.write("=== page %s (%s)\n%s\n" % (cells[3], cells[1], r[7] or ""))
PY
    log "  $label: native per data byte $(cat "$STEP_DIR/ratio.txt")"
}
derive_results() {
    python3 - "$RUN" <<'PY'
import glob, json, os, re, sys
run = sys.argv[1]
def read(p):
    try:
        return open(p, errors="replace").read().strip()
    except OSError:
        return ""
def page_value(path, prefix):  # "Total data sets written: 13205" in an sg_logs decode
    for line in read(path).splitlines():
        line = line.strip()
        if line.startswith(prefix):
            try:
                return int(line[len(prefix):].split()[0])
            except (ValueError, IndexError):
                return None
    return None
def repositions(path):  # HP page 0x34, parameter 0 (tape::log_pages::HpPerformance)
    try:
        raw = open(path, "rb").read()
    except OSError:
        return None
    if len(raw) < 4 or raw[0] & 0x3F != 0x34:
        return None
    end, at = min(4 + int.from_bytes(raw[2:4], "big"), len(raw)), 4
    while at + 4 <= end:
        code, n = int.from_bytes(raw[at:at + 2], "big"), raw[at + 3]
        if code == 0 and 1 <= n <= 4 and at + 4 + n <= len(raw):
            return int.from_bytes(raw[at + 4:at + 4 + n], "big")
        at += 4 + n
    return None
def f(x, digits=4):
    return "" if x is None else (("%.*f" % (digits, x)).rstrip("0").rstrip(".") if isinstance(x, float) else str(x))
cols = ["run", "label", "load", "knobs", "write_rc", "write_s", "data_bytes", "native_mb",
        "native_per_data_byte", "data_sets", "native_mb_per_data_set", "repositions_per_100mb",
        "repositions_before", "repositions_after", "metres_per_gb", "write_mbps", "trace_s",
        "stalls_over_100ms", "stalls_over_1s", "stalls_over_2600ms", "writer_waited_s",
        "hasher_run_share", "hasher_busy_share", "hasher_delay_s", "disk_await_max_ms",
        "disk_await_mean_ms", "gap_max_ms", "position_row_max_ms"]
out = ["\t".join(cols)]
for rid in read(os.path.join(run, "runs.order")).split():
    d = os.path.join(run, "run-" + rid)
    r = dict.fromkeys(cols)
    r.update(run=rid, label=read(d + "/label"), load=read(d + "/load.name"),
             knobs=read(d + "/knobs.env") or "none", write_rc=read(d + "/write.rc"),
             write_s=read(d + "/write.seconds"))
    ev = read(d + "/event.tsv").splitlines()
    det = {}
    if ev:
        try:
            det = json.loads(ev[-1].split("\t")[2])
        except (IndexError, ValueError):
            det = {}
    data, native = det.get("data_bytes"), det.get("native_bop_to_eod_mb")
    r.update(data_bytes=data, native_mb=native, native_per_data_byte=det.get("ratio"),
             repositions_per_100mb=det.get("repositions_per_100mb"))
    pre, post = d + "/drive-pre-write/snap.page_", d + "/drive-post-write/snap.page_"
    a, b = page_value(pre + "0x17.txt", "Total data sets written:"), page_value(post + "0x17.txt", "Total data sets written:")
    if a is not None and b is not None and b > a:
        r["data_sets"] = b - a
        if native:
            r["native_mb_per_data_set"] = native / (b - a)
    a, b = page_value(pre + "0x14.txt", "Lifetime metres of tape processed:"), page_value(post + "0x14.txt", "Lifetime metres of tape processed:")
    if a is not None and b is not None and data:
        r["metres_per_gb"] = (b - a) / (data / 1e9)
    r["repositions_before"], r["repositions_after"] = repositions(pre + "0x34.bin"), repositions(post + "0x34.bin")
    lines = read(d + "/pipeline.tsv").splitlines()
    if len(lines) > 1:
        head = lines[0].split("\t")
        rows = [dict(zip(head, l.split("\t"))) for l in lines[1:]]
        def nums(name):
            v = []
            for row in rows:
                try:
                    v.append(float(row.get(name, "")))
                except ValueError:
                    pass
            return v
        span = sum(nums("sample_ms")) / 1000.0
        written = nums("written_bytes")
        waits = nums("writer_wait_max_ms")
        r.update(trace_s=span, writer_waited_s=sum(nums("writer_wait_ms")) / 1000.0,
                 stalls_over_100ms=sum(w > 100 for w in waits), stalls_over_1s=sum(w > 1000 for w in waits),
                 stalls_over_2600ms=sum(w > 2600 for w in waits),
                 hasher_delay_s=sum(nums("hasher_delay_ms")) / 1000.0)
        if span > 0:
            r.update(hasher_run_share=sum(nums("hasher_run_ms")) / 1000.0 / span,
                     hasher_busy_share=sum(nums("hasher_busy_ms")) / 1000.0 / span)
            if written:
                r["write_mbps"] = max(written) / 1e6 / span
        aw = nums("disk_await_max_ms")
        if aw:
            r.update(disk_await_max_ms=max(aw), disk_await_mean_ms=sum(aw) / len(aw))
    log = read(d + "/session.log")
    gaps = [float(x) for x in re.findall(r"wrote file .*?; gap ([0-9.]+) ms", log)]
    rows_ms = [float(x) for x in re.findall(r"wrote file .*?position row ([0-9.]+) ms", log)]
    r.update(gap_max_ms=max(gaps) if gaps else None, position_row_max_ms=max(rows_ms) if rows_ms else None)
    out.append("\t".join(f(r[c]) for c in cols))
open(os.path.join(run, "results.tsv"), "w").write("\n".join(out) + "\n")
PY
}

# ---------- one run ----------
next_label() {
    dbq "$DB" "SELECT label FROM volumes WHERE label LIKE 'SHP-%'" | python3 -c 'import re, sys
n = [int(m.group(1)) for m in (re.fullmatch(r"SHP-(\d+)", l.strip()) for l in sys.stdin) if m]
print("SHP-%02d" % (max(n, default=0) + 1))'
}
ratio_of() { cat "$RUN/run-$1/ratio.txt" 2>/dev/null || true; }
INIT_FORCE=()
give_up_previous() { # retire PREV (this home's last volume) and short-erase it; or take a blank tape
    if [ -n "$PREV" ]; then
        # Every run writes the same versions: retiring their one copy needs them released first
        # (the retire floor, ADR-0008 Tier 3), once.
        local u
        for u in $UNITS; do
            if [ "$(dbq "$DB" "SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id
                  WHERE u.name = ? AND s.version = $UNIT_VERSION" "$u")" != reclaimable ]; then
                run "reclaim-$u" 0 tctl snapshot mark-reclaimable "$u" --version "$UNIT_VERSION" --force
            fi
        done
        case "$(dbq "$DB" "SELECT status FROM volumes WHERE label = ?" "$PREV")" in
            retired|erased) ;;
            *) run retire 0 tctl --yes volume retire "$PREV" ;;
        esac
    fi
    run erase-probe 0 tctl --json cartridge erase --device "$TAPE_DEV" --dry-run
    local outcome confirm
    outcome="$(json_get "$STEP_DIR/erase-probe.out" 'd["outcome"]')"
    confirm="$(json_get "$STEP_DIR/erase-probe.out" 'd.get("confirm_label")')"
    case "$outcome" in
        would_erase)
            [ -n "$PREV" ] && [ "$confirm" = "$PREV" ] || fail "the tape's File 0 names '$confirm', not this home's last volume '${PREV:-none}'"
            run erase 0 tctl --json cartridge erase --device "$TAPE_DEV" --confirm-label "$PREV"
            [ "$(json_get "$STEP_DIR/erase.out" 'd["outcome"]')" = erased ] || fail "cartridge erase did not report erased" ;;
        nothing_to_erase)
            # Blank, or File 0 EMPTY from an erase this catalog did not record: the consent this run
            # was given (--i-will-lose-the-cartridge, or --mhvtl) is init's --force.
            [ -z "$PREV" ] || fail "the tape reads blank, but this home's last volume $PREV should be on it"
            INIT_FORCE=(--force) ;;
        *) fail "cartridge erase --dry-run reported '$outcome'" ;;
    esac
}
do_run() { # <run id>
    local id="$1" load knobs label
    load="$(load_for "$id")"; knobs="$(knobs_for "$id")"
    [ -n "$load" ] || fail "run $id has no load: L* is not decided"
    begin_step "run-$id" "load $load; knobs: ${knobs:-none}"
    echo "$id" >> "$RUN/runs.order"
    printf '%s\n' "$knobs" > "$STEP_DIR/knobs.env"; echo "$load" > "$STEP_DIR/load.name"
    label="$(next_label)"; echo "$label" > "$STEP_DIR/label"
    give_up_previous
    run init 0 tctl --json volume init "$label" --device "$TAPE_DEV" "${INIT_FORCE[@]}"
    INIT_FORCE=(); PREV="$label"; echo "$label" > "$STATE/last-label"
    snapshot_drive "$STEP_DIR" pre-write
    start_load "$load" "$id"
    run host-check any tctl host check
    date -u +%Y-%m-%dT%H:%M:%SZ > "$STEP_DIR/write.started"
    run write 0 tctl_write "$knobs" --json --yes volume write "$label" --device "$TAPE_DEV" --keep-staging
    date -u +%Y-%m-%dT%H:%M:%SZ > "$STEP_DIR/write.ended"
    stop_load
    grep -F "RESEARCH BUILD" "$STEP_DIR/write.err" >/dev/null || fail "the write did not announce the research build"
    ! grep -F "  ignored: " "$STEP_DIR/write.err" >/dev/null || fail "the write ignored a knob: $(grep -F "  ignored: " "$STEP_DIR/write.err" | tr '\n' ' ')"
    [ "$(json_get "$STEP_DIR/write.out" 'd["ending"]')" = full ] || fail "the write did not seal full"
    snapshot_drive "$STEP_DIR" post-write
    collect_run "$label"
    end_step
    derive_results || log "  (results.tsv could not be derived; the raw records are kept)"
}

# ---------- the restore-rate leg ----------
tree_listing() { # <dir>: each file and link, sorted, with its sha256 or target; its sha256 is
    # short-seal-acceptance.sh's tree checksum
    (
        cd "$1" || exit 1
        find . \( -type f -o -type l \) | LC_ALL=C sort | while IFS= read -r p; do
            if [ -L "$p" ]; then
                printf 'LINK\t%s\t%s\n' "$p" "$(readlink "$p")"
            else
                printf 'FILE\t%s\t%s\n' "$p" "$(sha256sum "$p" | awk '{print $1}')"
            fi
        done
    )
}
check_tree() { # <restored dir> <name>: the source's tree, file for file, or FAIL
    tree_listing "$1" > "$STEP_DIR/tree-$2.txt" || fail "$2: cannot list $1"
    diff "$STATE/restore-source.tree" "$STEP_DIR/tree-$2.txt" > "$STEP_DIR/tree-diff-$2.txt" \
        || fail "$2: the restored tree differs from $RESTORE_UNIT's source (tree-diff-$2.txt)"
    log "  $2: identical to the source (tree checksum $(sha256sum < "$STEP_DIR/tree-$2.txt" | awk '{print $1}'))"
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
mib_s() { awk -v b="$1" -v s="$2" 'BEGIN { if (s > 0) printf "%.1f", b / s / 1048576 }'; }
restore_leg() { # the last volume, before it is retired; no load runs
    local rdir="$STAGING_ROOT/restore-$STAMP" key cbytes pbytes slogs slog read_line done_line t1 t2
    begin_step 8-restore "the restore rate: $RESTORE_UNIT from $PREV by tapectl restore unit, then by RESTORE.sh"
    key="$TAPECTL_HOME/keys/$TENANT-primary.age.key"
    [ -f "$key" ] || fail "the tenant key $key is missing"
    cbytes="$(dbq "$DB" "SELECT SUM(sl.encrypted_bytes) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
        WHERE u.name = ? AND ss.status = 'staged'" "$RESTORE_UNIT")"
    mkdir -p "$rdir"
    run restore-tapectl 0 tctl restore unit --unit "$RESTORE_UNIT" --from "$PREV" --to "$rdir/tapectl" --device "$TAPE_DEV"
    t1="$(cat "$STEP_DIR/restore-tapectl.seconds")"
    slogs=("$TAPECTL_HOME"/logs/*-restore-unit-"$RESTORE_UNIT"-*.log)
    [ -e "${slogs[0]}" ] || fail "no session log for restore unit $RESTORE_UNIT under $TAPECTL_HOME/logs"
    slog="${slogs[${#slogs[@]}-1]}"; cp "$slog" "$STEP_DIR/restore-session.log"
    # The read phase's end, as the session log has it ("done: read ..." on the terminal).
    read_line="$( (grep -F 'phase end: read ' "$slog" || true) | tail -1 | sed 's/.*phase end: //; s/  */ /g')"
    done_line="$( (grep -F 'done: read' "$STEP_DIR/restore-tapectl.err" || true) | tail -1 | sed 's/.*done: //; s/  */ /g')"
    [ -n "$read_line" ] || log "  (the restore's session log has no 'phase end: read' line)"
    pbytes="$(du -sb "$rdir/tapectl" | cut -f1)"
    check_tree "$rdir/tapectl" tapectl
    rm -rf "$rdir/tapectl"
    extract_restore_sh "$rdir/heir"
    cp "$rdir/heir/RESTORE.sh" "$STEP_DIR/RESTORE.sh"; mkdir -p "$rdir/tmp"
    run restore-sh 0 env TAPE_DEVICE="$TAPE_DEV" TMPDIR="$rdir/tmp" "$rdir/heir/RESTORE.sh" \
        --restore --unit "$RESTORE_UNIT" --key "$key" --to "$rdir/restore-sh"
    t2="$(cat "$STEP_DIR/restore-sh.seconds")"
    check_tree "$rdir/restore-sh" restore-sh
    rm -rf "$rdir"
    {
        printf 'path\tlabel\tunit\tciphertext_bytes\tplaintext_bytes\tseconds\tmib_per_s\tread_phase\n'
        printf 'tapectl restore unit\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$PREV" "$RESTORE_UNIT" "$cbytes" "$pbytes" \
            "$t1" "$(mib_s "$cbytes" "$t1")" "${read_line:-$done_line}"
        printf 'RESTORE.sh --restore\t%s\t%s\t%s\t%s\t%s\t%s\t\n' "$PREV" "$RESTORE_UNIT" "$cbytes" "$pbytes" \
            "$t2" "$(mib_s "$cbytes" "$t2")"
    } > "$RUN/restore.tsv"
    log "  tapectl restore unit: ${t1}s, $(mib_s "$cbytes" "$t1") MiB/s (session log: ${read_line:-none}); RESTORE.sh: ${t2}s, $(mib_s "$cbytes" "$t2") MiB/s"
    end_step
}

# ============================================================
print_plan | tee "$RUN/PLAN.txt"
log "run $RUN"
log "drive $DRIVE_SERIAL ($SYS_MODEL) at $TAPE_DEV ($TAPE_REAL, $DRIVE_SG); cartridge chip serial $SERIAL"
log "mode: $([ "$MHVTL" = 1 ] && echo "mhvtl rehearsal (loads: $([ "$LOADS" = 1 ] && echo on || echo skipped))" || echo "REAL DRIVE")"
log "tapectl: $TAPECTL_BIN ($TAPECTL_VERSION): $RESEARCH_BANNER"
log "home: $TAPECTL_HOME ($([ "$FRESH" = 1 ] && echo fresh || echo "reused; the tape holds ${PREV:-nothing of it}")); staging: $STAGING_ROOT"
[ -z "$ONLY" ] || log "only:$SELECTED (L* ${LSTAR:-undecided}, winner ${WINNER:-undecided})"
{
    echo "started=$(date -u +%Y-%m-%dT%H:%M:%SZ)"; echo "host=$(hostname)"
    echo "mode=$([ "$MHVTL" = 1 ] && echo mhvtl || echo real)"; echo "loads=$LOADS"; echo "vm_host=$VM_HOST"
    echo "device=$TAPE_DEV"; echo "device_real=$TAPE_REAL"; echo "sg=$DRIVE_SG"
    echo "drive_serial=$DRIVE_SERIAL"; echo "drive_model=$SYS_MODEL"; echo "cartridge_serial=$SERIAL"
    echo "tapectl=$TAPECTL_BIN"; echo "tapectl_version=$TAPECTL_VERSION"
    echo "tapectl_sha256=$(sha256sum "$TAPECTL_BIN" | awk '{print $1}')"; echo "research_banner=$RESEARCH_BANNER"
    echo "dar=$("$DAR_BIN" --version 2>&1 | sed -n 1p)"
    echo "script_commit=$(git -C "$SCRIPT_DIR" describe --always --dirty 2>/dev/null || echo unknown)"
    echo "home=$TAPECTL_HOME"; echo "home_fresh=$FRESH"; echo "staging=$STAGING_ROOT"; echo "size=$SIZE"
    echo "slice_size=$SLICE_SIZE"; echo "load_file=$LOAD_FILE"; echo "only=$SELECTED"
} > "$RUN/meta.txt"
: > "$RUN/steps.tsv"; : > "$RUN/runs.order"; : > "$RUN/decisions.txt"

# ------------------------------------------------------------ setup (no tape)
begin_step 0-setup "the throwaway home and the staged unit; the load file (no tape)"
if [ "$FRESH" = 1 ]; then
    mkdir -p "$TAPECTL_HOME"
    run init 0 tctl init --operator "$OPERATOR" --no-escrow
    run escrow 0 tctl key generate --escrow
    chmod 600 "$STEP_DIR/escrow.out" "$STEP_DIR/escrow.err"   # they print the escrow secret
    STAGING="$STAGING_ROOT/staging"; mkdir -p "$STAGING"
    run cfg-dar 0 tctl config set dar.binary "\"$DAR_BIN\""
    run cfg-slice 0 tctl config set defaults.slice_size "\"$SLICE_SIZE\""
    run cfg-copies 0 tctl config set defaults.min_copies 1
    run cfg-staging 0 tctl config set staging.directory "\"$STAGING\""
    run backend 0 tctl backend add --name shp --device-tape "$TAPE_DEV" --generation LTO-6
    run config-check 0 tctl config check
    cp "$CFG" "$RUN/config.toml"
    run tenant 0 tctl tenant add "$TENANT"
    mkdir -p "$STATE"; echo "$STAGING_ROOT" > "$STATE/staging-root"; echo "$RESTORE_SIZE" > "$STATE/restore-size"
    echo "made by scripts/shaping-drive-phase.sh at $STAMP; the drive phase's throwaway home" > "$TAPECTL_HOME/$SENTINEL_NAME"
    SRC="$STAGING_ROOT/src/$UNIT"; mkdir -p "$SRC"
    log "  the synthetic data: $((SIZE - RESTORE_SIZE)) bytes for $UNIT under $SRC"
    left=$((SIZE - RESTORE_SIZE)); i=0
    while [ "$left" -gt 0 ]; do
        n=$(( left < PIECE ? left : PIECE )); i=$((i + 1))
        head -c "$n" /dev/urandom > "$SRC/blob-$(printf '%03d' "$i").bin" || fail "cannot write the synthetic unit under $SRC"
        left=$((left - n))
    done
    run unit 0 tctl unit init "$SRC" --tenant "$TENANT" --name "$UNIT"
    if [ "$RESTORE_SIZE" != 0 ]; then
        # Shaped like the acceptance's ssa-b-photos: six raw files, an index, a nested leaf.
        RSRC="$STAGING_ROOT/src/$RESTORE_UNIT"; mkdir -p "$RSRC/raw" "$RSRC/nested/deeper"
        log "  and $RESTORE_SIZE bytes for $RESTORE_UNIT under $RSRC"
        for i in 1 2 3 4 5 6; do
            head -c $((RESTORE_SIZE / 6)) /dev/urandom > "$RSRC/raw/img-$i.raw" || fail "cannot write $RSRC"
        done
        echo "the restore-rate leg's unit, $STAMP" > "$RSRC/index.txt"
        echo "deep leaf" > "$RSRC/nested/deeper/leaf.txt"
        run unit-restore 0 tctl unit init "$RSRC" --tenant "$TENANT" --name "$RESTORE_UNIT"
    fi
    for u in $UNITS; do
        run "snapshot-$u" 0 tctl snapshot create "$u"
        run "stage-$u" 0 tctl --yes stage create "$u"
        [ "$(staged_sets "$u")" = 1 ] || fail "stage create left no staged set for $u"
    done
    if [ "$RESTORE_SIZE" != 0 ]; then
        # What both restores are checked against, the source then gone.
        tree_listing "$RSRC" > "$STATE/restore-source.tree" || fail "cannot list $RSRC"
        cp "$STATE/restore-source.tree" "$STEP_DIR/restore-source.tree"
        log "  $RESTORE_UNIT's tree checksum: $(sha256sum < "$STATE/restore-source.tree" | awk '{print $1}')"
    fi
    rm -rf "$STAGING_ROOT/src"   # every write reads the staged slices, never the sources
fi
if [ "$(stat -c %s "$LOAD_FILE" 2>/dev/null || echo 0)" != $((LOAD_MIB * MiB)) ]; then
    log "  the load file: $LOAD_MIB MiB from /dev/urandom into $LOAD_FILE"
    mkdir -p "$(dirname "$LOAD_FILE")"
    dd if=/dev/urandom of="$LOAD_FILE" bs=1M count="$LOAD_MIB" iflag=fullblock status=none || fail "cannot make the load file $LOAD_FILE"
fi
run staging-status any tctl --json staging status
run plan any tctl volume plan
end_step

# ------------------------------------------------------------ the runs
decide() { echo "$*" >> "$RUN/decisions.txt"; log "  DECISION: $*"; }
if [ -n "$ONLY" ]; then
    for r in $ALL_RUNS; do
        if selected "$r"; then do_run "$r"; fi
    done
else
    for r in P0a P0b P0c R1; do do_run "$r"; done
    # The ladder and its stop rule: the first load at 1.3 or more is L*.
    LSTAR=""
    for r in R2 R3 R5r R5w; do
        do_run "$r"
        if [ "$MHVTL" = 0 ] && python3 -c 'import sys; sys.exit(0 if float(sys.argv[1]) >= float(sys.argv[2]) else 1)' \
            "$(ratio_of "$r")" "$LADDER_THRESHOLD" 2>/dev/null; then
            LSTAR="$(load_for "$r")"
            decide "stop rule: $r ($LSTAR) reached $(ratio_of "$r") >= $LADDER_THRESHOLD; L* = $LSTAR"
            case "$LSTAR" in L5*) decide "a CPU load did not reproduce it and the staging path did: weight §8 (R11 is not run in this phase)" ;; esac
            break
        fi
    done
    if [ -z "$LSTAR" ] && [ "$MHVTL" = 1 ]; then
        LSTAR=L2; decide "mhvtl: its feed ratio measures nothing; the whole ladder ran and L* = L2 is forced for the rehearsal"
    fi
    if [ -z "$LSTAR" ]; then
        decide "stop rule: no load reached $LADDER_THRESHOLD (R2 $(ratio_of R2), R3 $(ratio_of R3), R5r $(ratio_of R5r), R5w $(ratio_of R5w)); R1, R8 and R9 under L2, then stop"
        for r in R1-L2 R8-L2 R9-L2; do do_run "$r"; done
    else
        echo "$LSTAR" > "$STATE/lstar"
        for r in R6 R7 R8 R9 R10; do do_run "$r"; done
        WINNER="$(python3 - "$RUN" <<'PY'
import os, sys
reps = {}
try:
    for line in open(os.path.join(sys.argv[1], "results.tsv")).read().splitlines()[1:]:
        cells = line.split("\t")
        if len(cells) > 11 and cells[11]:
            reps[cells[0]] = float(cells[11])  # repositions_per_100mb
except (OSError, ValueError):
    pass
best = None
for rid in ["R6", "R7", "R8", "R9", "R10"]:
    try:
        ratio = float(open(os.path.join(sys.argv[1], "run-" + rid, "ratio.txt")).read())
    except (OSError, ValueError):
        continue
    key = (ratio, reps.get(rid, 1e9))
    if best is None or key < best[0]:
        best = (key, rid)
print(best[1] if best else "")
PY
)"
        [ -n "$WINNER" ] || fail "no lever run has a feed ratio to choose a winner by"
        echo "$WINNER" > "$STATE/winner"
        decide "winner: $WINNER (the lowest feed ratio of R6-R10, then the fewest repositions per 100 MB): $(knobs_for "$WINNER")"
        for r in R12a R12b; do do_run "$r"; done
    fi
fi

# ------------------------------------------------------------ the restore rate, then the end
if [ "$RESTORE_SIZE" != 0 ] && [ -n "$PREV" ]; then restore_leg; fi
begin_step 9-end "the last volume retired and the cartridge short-erased, so it is left as found"
give_up_previous
PREV=""; rm -f "$STATE/last-label"
if [ "$CLEANUP" = 1 ]; then
    CLEAN_ARGS=(); for u in $UNITS; do CLEAN_ARGS+=(--unit "$u"); done
    run release 0 tctl staging clean "${CLEAN_ARGS[@]}" --force
    rm -f "$LOAD_FILE"; log "  --cleanup: the staged unit released and $LOAD_FILE removed"
fi
end_step
cp -a "$DB" "$RUN/catalog-at-end.db"
derive_results
{
    echo "RESULT: PASS"
    echo "runs: $(tr '\n' ' ' < "$RUN/runs.order")"
    cat "$RUN/decisions.txt"
    [ ! -s "$RUN/restore.tsv" ] || cat "$RUN/restore.tsv"
    echo "the cartridge is short-erased (File 0 EMPTY); the derived figures are in results.tsv"
} > "$RUN/RESULT.txt"
column -t -s $'\t' "$RUN/results.tsv" 2>/dev/null | tee -a "$RUN/run.log" || cat "$RUN/results.tsv"
log "RESULT: PASS. $RUN"
[ "$MHVTL" = 1 ] || log "Start the drive-poll timer again: sudo systemctl start tapectl-drive-poll.timer"
