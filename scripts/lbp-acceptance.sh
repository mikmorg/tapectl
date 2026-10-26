#!/usr/bin/env bash
# lbp-acceptance.sh -- the 1.7.2 acceptance run of Logical Block Protection on a real drive
# (issue #388; ADR-0012 items 161-166, 174 and 175). The procedure around it is
# docs/lto6-validation-checklist.md, "LBP on the real drive (1.7.2 acceptance)".
#
# WHAT IT PROVES, on the one test cartridge, in throwaway homes:
#   0 preflight  read only: the drive's Control Data Protection page (current and changeable),
#                its block limits and identity. The page must read method 0 with neither check
#                on before anything is touched.
#   1 r1         R1 FIRST, because it decides item 175's switch: do blocks written WITHOUT LBP
#                come back with a valid CRC when LBP_R is armed? Two routes, both read-only of
#                what they wrote:
#                  1a the prototype's (docs/research/lbp-prototype/): 16 random blocks written
#                     with dd at BOT with the page clear, then read back with the page armed BY
#                     HAND (method 01h, LBP_W and LBP_R) and st at payload + 4. Each CRC is
#                     checked by lbp.py, a CRC implementation that is not tapectl's, and each
#                     payload compared with what was written. The page is put back after (and by
#                     the exit trap, if the script dies in between).
#                  1b tapectl's: OLD_BIN, a tapectl before 1.7.2 (no LBP: production's L6-0001
#                     and L6-0002 were written so), writes and seals LBA-0001 in its own home;
#                     the candidate's `volume verify --full` reads it back. Its session log says
#                     what the candidate armed for a read, which is item 175's switch as built.
#                Why both: 1a answers the drive's behaviour alone, in a minute, independent of
#                tapectl's code; 1b is the path the switch governs (a verify of an old tape).
#                Both yes, with the switch at "every read": item 162 stands. 1a no: item 175's
#                fallback (LBP_R only for the confirm of a session that wrote with LBP); a
#                candidate built without it stops here, to be rebuilt. --r1-only stops after
#                this step either way, so the switch can be decided before the long steps.
#   2 write      the candidate writes LBA-0002 (three units) with --full-confirm: write_lbp
#                'on', the session log names the page armed, no fallback warning, the full
#                readback passed, the page method 0 afterwards.
#   3 refusal    TAPECTL_TEST_LBP_CORRUPT_BLOCK=<n> (F5; the hook takes n1[,n2...], each block
#                write counted from 1 since the open, re-sent blocks too), n in the middle of
#                LBA-0003's data:
#                the drive refuses the block, the session writes on from the start of the file
#                holding the first block it did not take (item 174: a write_lbp_retry event and
#                the session log's line), seals, and the full readback passes. dmesg and the log
#                pages are recorded around it.
#   4 second     the hook given A and then every block from B to C: the drive refuses A, item
#                174 writes on once from its file's start, and a block of B..C, sent again or
#                later in the retry, is refused a second time: the session is left interrupted
#                (exit 4), and `volume resume` completes it. B needs slack past A: every block
#                sent counts, and the real drive reports a refusal a few blocks after the block
#                (st and the drive write behind), so a listed block sent before the first
#                refusal surfaced is spent inside it, not in the retry. Hence a range, B..C,
#                whose first block past that point is the one refused: the check is the second
#                refusal, never which listed block it was. The default puts A in the middle of
#                the data (as step 3) and B..C at A+64..A+320; --second-refusal-blocks
#                overrides it. mhvtl refuses a bad block at once, so there one B would be exact,
#                but the rehearsal cannot run the step: every open falls back there, and the
#                hook refuses an open that is not armed.
#   5 crash      `volume write LBA-0005` killed with SIGKILL mid-write: the page reads armed (the
#                leftover item 164 accepts); the heir's first dd and RESTORE.sh --info fail and
#                print nothing of the tape; `volume identify` prints tapectl's leftover note and
#                leaves the page method 0; RESTORE.sh --info then works; `volume resume`
#                completes the write.
#   6 verify     `volume verify LBA-0005 --full`: LBP_R armed under item 162's policy (none
#                under item 175's fallback), exit 0.
#   7 heir       RESTORE.sh off LBA-0005 with the real tools, the page clear and no manual step:
#                --verify passes, --restore of one unit is identical to its source, and one
#                record read in variable mode is 524,288 bytes (the CRC is not on the medium).
#   8 r11        R11: can the tapectl service user open the drive's sg node for writing (owner,
#                group or ACL)? Without it every production write falls back (item 165).
#   9 rate       step 2's write rate (phase_timings) beside the 1.6.0 acceptance's 160 MB/s. A
#                debug build caps near 117 MB/s with the CRC (build plan F9); a release build's
#                figure is the CTO's to approve.
#
# A FAIL STOPS THE RUN and leaves the cartridge as the failed step left it: no later step, no
# exit trap, ever writes the medium a failed step measured. The exit trap only puts back a page
# the script armed by hand (step 1a, and step 5's rehearsal), which is a MODE SELECT, not a
# write. The run directory keeps every command's output, every session log, the catalogs, the
# page (sg_modes, raw) and the drive's sysfs options and counters, MAM and log pages 0x02 0x03
# 0x0c 0x17 before and after every step.
#
# REFUSES unless every one of these holds (exit 2, nothing touched):
#   - TAPECTL_HOME is set, absent or empty, and none of production's homes: /etc/tapectl/home's
#     target, the tapectl service user's home, /srv/archive_meta/tapectl (home2.profile), or
#     this account's ~/.tapectl;
#   - the drive's serial is HUJ808A5L4 and the cartridge's chip serial is EW7VWMVKF6, named
#     again with --i-will-lose-the-cartridge (the barcode is a sticker; ADR-0012) -- or --mhvtl,
#     on an mhvtl drive, never that one;
#   - on the real drive, TAPECTL_BIN is 1.7.2 or later and OLD_BIN (required on the real drive) is 1.6.0 or later
#     and before 1.7.2 (the commands this script gives it are 1.6.0's; any tapectl before 1.7.2
#     writes without LBP, as 1.1.0 wrote production's tapes);
#   - TAPECTL_TEST_LBP_CORRUPT_BLOCK and TAPECTL_TEST_EARLY_WARNING_AT are unset;
#   - the page reads method 0 with neither check on;
#   - tapectl-drive-poll.timer is not active, and the tape lock is free.
#
# --mhvtl is the rehearsal on a virtual LTO-6 drive (vm-desk1: A3 or A4, ULT3580-TD6). mhvtl
# cannot carry the 524,292-byte wire block in one command, so EVERY tapectl open there falls
# back (item 165, cause 8): the rehearsal proves the script, the fallback's record and warning,
# the hook's refusal on an open that is not armed, and the leftover clear (on a page it arms by
# hand). Step 1a runs at a 262,144-byte payload, which mhvtl carries; mhvtl recomputes a CRC for
# every block it returns, so its "yes" says nothing of the HP. ONLY THE REAL DRIVE PROVES: R1's
# answer, a write with LBP on, the drive's refusal and item 174's retry, the leftover a SIGKILL
# leaves, a verify with LBP_R, the heir on a tape written with LBP, and the write rate.
#
# EXIT: 0 every step passed; 1 a step FAILED (the run stopped there; the cartridge is as that
# step left it); 2 a precondition refused (nothing was touched); 3 every step that ran passed,
# and a step was not run (step 4 in the rehearsal) or --r1-only stopped after step 1.
set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
LBP_PY="$SCRIPT_DIR/../docs/research/lbp-prototype/lbp.py"

TEST_DRIVE_SERIAL="HUJ808A5L4"
TEST_CARTRIDGE="EW7VWMVKF6"
# contrib/hosts/home2.profile, SVC_HOME_WANT: the service user's home on home2.
HOME2_SVC_HOME="/srv/archive_meta/tapectl"
SVC_USER="tapectl"
# The 1.6.0 acceptance's write rate on this drive (docs/runs/2026-10-21-1.6.0-acceptance.md).
RATE_1_6_0_MBPS=160

LABEL1="LBA-0001"; LABEL2="LBA-0002"; LABEL3="LBA-0003"; LABEL4="LBA-0004"; LABEL5="LBA-0005"
OPERATOR="lba-op"
TENANT="lba"
UNIT_A="lba-a-docs"
UNIT_B="lba-b-photos"
UNIT_C="lba-c-tail"
UNIT_R="lba-r1"
LEFTOVER_NOTE="had its Logical Block Protection page armed when tapectl opened it"
FALLBACK_WARNING="warning: Logical Block Protection is not available on "

usage() {
    cat <<'USAGE'
usage: TAPECTL_HOME=<fresh dir> TAPECTL_BIN=<tapectl 1.7.2> OLD_BIN=<tapectl before 1.7.2> \
       scripts/lbp-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \
           --i-will-lose-the-cartridge EW7VWMVKF6 [options]
       scripts/lbp-acceptance.sh --mhvtl --device /dev/tape/by-id/scsi-XYZZY_A3-nst [options]
       scripts/lbp-acceptance.sh --print-plan [--mhvtl]

The 1.7.2 acceptance run of Logical Block Protection: R1 first (item 175's switch), then an LBP
write, the drive's refusal and item 174's retry, a crash's leftover page, a verify with LBP_R,
the heir's restore, R11 and the write rate. OVERWRITES THE CARTRIDGE, several times.

  --device PATH                  Required. The drive's st node, by-id spelling. No default.
  --i-will-lose-the-cartridge S  Required on the real drive: EW7VWMVKF6, checked against MAM.
  --mhvtl                        The rehearsal on an mhvtl TD6 drive (every open falls back).
  --r1-only                      Run steps 0 and 1 and stop: R1's answer, before the long steps.
  --second-refusal-blocks A,B[-C]
                                 Step 4's blocks, counted from 1 since the write opened the
                                 drive, re-sent ones too: A for the first refusal, then B, or
                                 every block from B to C, for the second
                                 (TAPECTL_TEST_LBP_CORRUPT_BLOCK=A,B,B+1,...,C). B must lie past
                                 A by more than the drive's write-behind: a listed block sent
                                 before the first refusal surfaced is spent inside it, so a
                                 range is the safe form. The step judges the second refusal,
                                 not which listed block caused it. Default: A in the middle of
                                 the data, B-C = A+64 to A+320.
  --out DIR                      Where the run directory goes. Default /scratch/tapectl-lbp
                                 where /scratch exists, else ~/.cache/tapectl/lbp.
  --print-plan                   Print what the operator must do and the drive time, and exit.

  TAPECTL_HOME (env)             Required: the throwaway home, absent or empty.
  TAPECTL_BIN (env)              Required: the tapectl under test (1.7.2 or later).
  OLD_BIN (env)                  A tapectl from 1.6.0 to 1.7.1, which writes without LBP (step
                                 1b). Required on the real drive; on --mhvtl, 1b is skipped
                                 without it.
  DAR_BIN (env)                  dar for the throwaway configs. Default: dar on PATH.
USAGE
}

die() { echo "lbp-acceptance: REFUSED: $*" >&2; exit 2; }

TAPE_DEV=""; LOSE_SERIAL=""; OUT_DIR=""; MHVTL=0; PRINT_PLAN=0; R1_ONLY=0; SECOND_BLOCKS=""
while [ $# -gt 0 ]; do
    case "$1" in
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --mhvtl) MHVTL=1; shift ;;
        --r1-only) R1_ONLY=1; shift ;;
        --second-refusal-blocks) SECOND_BLOCKS="${2:-}"; shift 2 ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --print-plan) PRINT_PLAN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "lbp-acceptance: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
# second_list <A> <B> <C>: the hook's value for step 4, A then every block from B to C.
second_list() {
    local out="$1" n
    for n in $(seq "$2" "$3"); do out="$out,$n"; done
    printf '%s\n' "$out"
}
SECOND_A=""; SECOND_B=""; SECOND_C=""
if [ -n "$SECOND_BLOCKS" ]; then
    case "$SECOND_BLOCKS" in
        *[!0-9,-]*|,*|*,|*,*,*|*--*) die "--second-refusal-blocks wants A,B or A,B-C, block numbers from 1 (got '$SECOND_BLOCKS')" ;;
    esac
    SECOND_A="${SECOND_BLOCKS%%,*}"; rest="${SECOND_BLOCKS#*,}"
    SECOND_B="${rest%%-*}"; SECOND_C="${rest#*-}"
    for n in "$SECOND_A" "$SECOND_B" "$SECOND_C"; do
        case "$n" in ''|*[!0-9]*|0*) die "--second-refusal-blocks wants A,B or A,B-C, block numbers from 1 (got '$SECOND_BLOCKS')" ;; esac
    done
    [ "$SECOND_B" -gt "$SECOND_A" ] || die "--second-refusal-blocks: B ($SECOND_B) must lie past A ($SECOND_A)"
    [ "$SECOND_C" -ge "$SECOND_B" ] || die "--second-refusal-blocks: C ($SECOND_C) must not come before B ($SECOND_B)"
    [ $((SECOND_C - SECOND_B)) -le 4096 ] || die "--second-refusal-blocks: B..C spans more than 4096 blocks"
fi

# ---------- the two shapes of the run ----------
MiB=$((1024 * 1024)); GiB=$((1024 * MiB))
if [ "$MHVTL" = 1 ]; then
    SLICE_SIZE="1M"
    RAW_BLOCK=262144        # mhvtl carries 262,148 bytes in one command, not 524,292
    SIZE_A=$((600 * 1024)); SIZE_B=$((6 * MiB)); SIZE_C=$((2560 * 1024)); SIZE_R=$((3 * MiB))
    KILL_AFTER=$((4 * MiB)) # st write bytes before the SIGKILL
else
    SLICE_SIZE="1G"
    RAW_BLOCK=524288
    SIZE_A=$((256 * MiB)); SIZE_B=$((6 * GiB)); SIZE_C=$((1 * GiB)); SIZE_R=$((1536 * MiB))
    KILL_AFTER=$((2 * GiB))
fi
FORMAT_BLOCK=524288

print_plan() {
    cat <<PLAN
=== The 1.7.2 Logical Block Protection acceptance run (ADR-0012 items 161-166, 174, 175) ===

BEFORE YOU START (the operator):
  1. Load the test cartridge EW7VWMVKF6 into the HP LTO-6 (HUJ808A5L4). Nothing on it is kept:
     the run writes five volumes over it in turn.
  2. Nothing else may use the drive for the whole run:
       sudo systemctl stop tapectl-drive-poll.timer      (start it again afterwards)
     no production tapectl command running or scheduled, and the host quiet (vm-desk1's heavy
     jobs paused, docs/operator-guide.md "A quiet host while the tape runs").
  3. Have both binaries: the 1.7.2 candidate (TAPECTL_BIN) and a tapectl before 1.7.2
     (OLD_BIN, e.g. the installed production one), which writes step 1b's tape without LBP.
  4. Run it inside tmux. A dropped terminal does not stop it.
  5. A FAIL stops the run and leaves the cartridge as it is. Do not reuse or erase it until the
     failure is read (docs/lto6-validation-checklist.md).
  6. For step 3's kernel log, have a second terminal ready for 'sudo dmesg' (the script tries
     dmesg itself, which needs dmesg_restrict = 0).

THE STEPS and their time on home2 (the write near 117 MB/s for a debug candidate, ~158 MB/s for
a release one; about $(( (SIZE_A + SIZE_B + SIZE_C) / GiB )) GiB a volume):
  0 preflight   read only: the page, block limits, sg_inq; the page must read method 0.   ~1 min
  1 r1          1a: 16 blocks dd'd without LBP, read back with LBP_R armed by hand, every CRC
                checked by lbp.py. 1b: OLD_BIN stages ~$((SIZE_R / MiB)) MiB, writes and seals
                $LABEL1; the candidate's verify --full reads it with LBP_R.            ~15 min
                THIS ANSWERS R1 AND ITEM 175. --r1-only stops here.
  2 write       three units staged (debug: ~30-40 min), $LABEL2 written with LBP and read back
                in full; write_lbp 'on'; the page clear after.                         ~45 min
  3 refusal     $LABEL3 with the test hook mid-data: the drive refuses one block, item 174's
                retry writes on from the file's start, the volume seals and reads back. ~5 min
  4 second      $LABEL4 with the hook at A and A+64..A+320: the drive refuses twice in one
                run; the session is left interrupted; volume resume seals it.          ~5 min
  5 crash       $LABEL5's write killed mid-way: the page left armed; the heir's reads fail with
                I/O errors; volume identify clears it; volume resume completes the write. ~5 min
  6 verify      volume verify $LABEL5 --full with LBP_R.                                ~2 min
  7 heir        RESTORE.sh off $LABEL5 with the real tools: --verify, one unit restored.  ~5 min
  8 r11         the tapectl service user's write access to the drive's sg node.       no tape
  9 rate        step 2's write rate beside 1.6.0's ${RATE_1_6_0_MBPS} MB/s.                no tape
  In all about 1.5-2 hours, most of it staging.

THE COMMAND (operator's login on home2, inside tmux):
  TAPECTL_HOME=\$HOME/.cache/tapectl/lba-home-\$(date +%Y%m%d) \\
  TAPECTL_BIN=<the 1.7.2 candidate> OLD_BIN=/usr/local/bin/tapectl \\
    scripts/lbp-acceptance.sh --device /dev/tape/by-id/scsi-HUJ808A5L4-nst \\
      --i-will-lose-the-cartridge EW7VWMVKF6 [--r1-only]

Afterwards the cartridge holds $LABEL5 and belongs to the throwaway home: retire it and run
cartridge erase from that home before it is a test cartridge again (the closing lines say how),
and start the drive-poll timer.
PLAN
}

if [ "$PRINT_PLAN" = 1 ]; then print_plan; exit 0; fi

# ---------- preconditions: nothing below touches the drive until every one holds ----------
[ -n "$TAPE_DEV" ] || { usage >&2; die "--device is required; there is no default (/dev/nstN numbering moves)"; }
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"
for bin in mt dd tr cmp head od sg_read_attr sg_logs sg_modes sg_wr_mode sg_inq python3 flock age dar tar sha256sum diff awk tee df stat getent; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
[ -r "$LBP_PY" ] || die "the prototype's host tool $LBP_PY is missing (docs/research/lbp-prototype/lbp.py)"
python3 -I "$LBP_PY" selftest >/dev/null 2>&1 || die "lbp.py selftest failed: its RS-CRC does not reproduce mhvtl's vectors"
SG_LOGS_HELP="$(sg_logs --help 2>&1 || true)"
for opt in '--maxlen' '--in=' '--pdt' '--raw' '--page'; do
    case "$SG_LOGS_HELP" in *"$opt"*) ;; *) die "this sg_logs does not list the $opt option; the log-page reads would fail" ;; esac
done

for hook in TAPECTL_TEST_LBP_CORRUPT_BLOCK TAPECTL_TEST_EARLY_WARNING_AT; do
    [ -z "${!hook+set}" ] || die "$hook is set in the environment: this script sets the LBP hook itself, for step 3's write only, and never the early-warning one. Unset it"
done

[ -n "${TAPECTL_BIN:-}" ] || die "TAPECTL_BIN is required: the tapectl under test (this script never builds one)"
[ -x "$TAPECTL_BIN" ] || die "TAPECTL_BIN=$TAPECTL_BIN is not an executable file"
TAPECTL_BIN="$(readlink -f "$TAPECTL_BIN")"
: "${DAR_BIN:=$(command -v dar)}"
[ -x "$DAR_BIN" ] || die "DAR_BIN=$DAR_BIN is not an executable file"
version_of() { "$1" --version 2>&1 | sed -n 1p; }
version_in() { # <version text> <min a.b.c> <below a.b.c or ''>: 0 when min <= v < below
    python3 - "$1" "$2" "$3" <<'PY'
import re, sys
m = re.search(r"(\d+)\.(\d+)\.(\d+)", sys.argv[1])
if not m:
    sys.exit(1)
v = tuple(map(int, m.groups()))
lo = tuple(map(int, sys.argv[2].split(".")))
hi = tuple(map(int, sys.argv[3].split("."))) if sys.argv[3] else None
sys.exit(0 if v >= lo and (hi is None or v < hi) else 1)
PY
}
TAPECTL_VERSION="$(version_of "$TAPECTL_BIN")"
# The run is the 1.7.2 candidate's; a rehearsal may run a build of the branch before its
# version moves (an older tapectl fails the rehearsal's first catalog check instead).
if [ "$MHVTL" = 0 ]; then
    version_in "$TAPECTL_VERSION" 1.7.2 "" || die "TAPECTL_BIN reports '$TAPECTL_VERSION': the acceptance run is for tapectl 1.7.2 or later"
fi
OLD_VERSION=""
if [ -n "${OLD_BIN:-}" ]; then
    [ -x "$OLD_BIN" ] || die "OLD_BIN=$OLD_BIN is not an executable file"
    OLD_BIN="$(readlink -f "$OLD_BIN")"
    OLD_VERSION="$(version_of "$OLD_BIN")"
    version_in "$OLD_VERSION" 1.6.0 1.7.2 || die "OLD_BIN reports '$OLD_VERSION': step 1b needs a tapectl from 1.6.0 to 1.7.1, which writes without LBP"
elif [ "$MHVTL" = 0 ]; then
    die "OLD_BIN is required on the real drive: a tapectl before 1.7.2 writes step 1b's tape without LBP (R1's tapectl route)"
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
SVC_DIR="$(getent passwd "$SVC_USER" 2>/dev/null | cut -d: -f6 || true)"
[ -z "$SVC_DIR" ] || PROD_HOMES+=("$SVC_DIR" "$SVC_DIR/.tapectl")
for p in "${PROD_HOMES[@]}"; do
    pc="$(canon "$p")"
    case "$HOME_C/" in "$pc"/*) die "TAPECTL_HOME=$TAPECTL_HOME is (or is inside) production's home $p" ;; esac
    case "$pc/" in "$HOME_C"/*) die "TAPECTL_HOME=$TAPECTL_HOME contains production's home $p" ;; esac
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
# The node the kernel binds to the st node, as tapectl itself finds it (F13): never config's.
DRIVE_SG="/dev/${SG_ENTRIES[0]}"
DRIVE_SERIAL="$(tail -c +5 "$SYS_ST/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
[ -n "$DRIVE_SERIAL" ] || die "cannot read the drive serial from sysfs ($SYS_ST/device/vpd_pg80)"
SYS_MODEL="$(sed 's/[[:space:]]*$//' "$SYS_ST/device/model" 2>/dev/null || true)"
GEN_N="$(echo "$SYS_MODEL" | sed -n 's/.*Ultrium \([0-9]\{1,2\}\).*/\1/p; s/.*TD\([0-9]\{1,2\}\).*/\1/p' | sed -n 1p)"
[ "$GEN_N" = 6 ] || die "the drive's model '$SYS_MODEL' is not an LTO-6 drive: this run is the LTO-6 acceptance run (on mhvtl, a TD6: Reed-Solomon)"
if [ "$MHVTL" = 1 ]; then
    [ "$DRIVE_SERIAL" != "$TEST_DRIVE_SERIAL" ] || die "--mhvtl names the REAL drive $TEST_DRIVE_SERIAL: the rehearsal runs on mhvtl only"
    case "$DRIVE_SERIAL" in XYZZY_*) ;; *) die "--mhvtl: $TAPE_DEV has serial '$DRIVE_SERIAL', not an mhvtl drive (XYZZY_*)" ;; esac
else
    [ "$DRIVE_SERIAL" = "$TEST_DRIVE_SERIAL" ] || die "the drive's serial is '$DRIVE_SERIAL', not the validation drive $TEST_DRIVE_SERIAL (give --mhvtl for the rehearsal)"
    [ -n "$LOSE_SERIAL" ] || die "--i-will-lose-the-cartridge $TEST_CARTRIDGE is required: this run overwrites the loaded cartridge"
    [ "$LOSE_SERIAL" = "$TEST_CARTRIDGE" ] || die "--i-will-lose-the-cartridge names '$LOSE_SERIAL': this run is for the test cartridge $TEST_CARTRIDGE only"
fi
[ -r "$DRIVE_SG" ] && [ -w "$DRIVE_SG" ] || die "this login cannot open $DRIVE_SG for reading and writing ($(stat -c '%A %U:%G' "$DRIVE_SG")): the page cannot be read or set (the tape group can on home2)"

# Single-drive rule (#9). The real drive shares the harness lock with the gate, the suite and
# the profile; an mhvtl drive takes its own, as the other rehearsals do.
if [ "$MHVTL" = 1 ]; then TAPE_LOCK="/tmp/tapectl-tape.lock.$ST_NODE"; else TAPE_LOCK="/tmp/tapectl-tape.lock"; fi
exec 9>"$TAPE_LOCK"
flock -n 9 || die "another process holds the tape lock ($TAPE_LOCK)"

# ---------- the page, read-only ----------
# The Control Data Protection page (0Ah/F0h) from a MODE SENSE(10) answer on stdin, as
# tape::lbp parses it: "method=01 info_len=04 flags=c0" (LBP_W 0x80, LBP_R 0x40), or "contents"
# for sg_wr_mode with bytes 4-6 replaced and PS cleared (the prototype's set_lbp, never saved).
page_py() { # page_py fields | page_py contents <method> <len> <flags>
    python3 -c '
import sys
r = sys.stdin.buffer.read()
if len(r) < 8:
    sys.exit("a MODE SENSE(10) answer of %d bytes" % len(r))
p = bytearray(r[8 + int.from_bytes(r[6:8], "big"):])
if len(p) < 8 or p[0] & 0x3F != 0x0A or p[1] != 0xF0:
    sys.exit("no Control Data Protection page (0Ah/F0h) in %s" % r.hex())
p = p[:min(len(p), int.from_bytes(p[2:4], "big") + 4)]
if sys.argv[1] == "fields":
    print("method=%02x info_len=%02x flags=%02x" % (p[4], p[5], p[6] & 0xC0))
else:
    m, n, f = (int(x, 16) for x in sys.argv[2:5])
    p[0] &= 0x7F
    p[4], p[5] = m, n
    p[6] = (p[6] & ~0xC0 & 0xFF) | f
    print(",".join("%02x" % b for b in p))
' "$@"
}
page_raw() { sg_modes --page=0x0a,0xf0 --control="${1:-0}" --raw "$DRIVE_SG"; }
page_state() { page_raw 0 | page_py fields; }
page_is_clear() { case "$(page_state 2>/dev/null)" in "method=00 "*" flags=00") return 0 ;; *) return 1 ;; esac; }
FOUND_PAGE="$(page_state 2>&1)" || die "the drive's Control Data Protection page cannot be read on $DRIVE_SG: $FOUND_PAGE"
case "$FOUND_PAGE" in
    "method=00 "*" flags=00") ;;
    *) die "the drive's page reads '$FOUND_PAGE', not method 0 with neither check on: a tapectl that crashed or was killed, or a hand-run sg_wr_mode, left it so. Clear it (any tapectl tape command does, or power the drive off and on) and run again" ;;
esac

# ---------- consent: the cartridge is known by its chip serial, never its sticker ----------
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

# ---------- the run directory ----------
if [ -z "$OUT_DIR" ]; then
    if [ -d /scratch ]; then OUT_DIR="/scratch/tapectl-lbp"; else OUT_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl/lbp"; fi
fi
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
SAFE_SERIAL="$(printf '%s' "$SERIAL" | tr -c 'A-Za-z0-9_.-' '_')"
RUN="$OUT_DIR/lba-$STAMP-$SAFE_SERIAL"
mkdir -p "$RUN" || die "cannot create $RUN"
NEED=$((2 * (SIZE_A + SIZE_B + SIZE_C + SIZE_R) + SIZE_B + 32 * RAW_BLOCK + 256 * MiB))
AVAIL="$(df --output=avail -B1 "$RUN" | tail -n 1 | tr -d ' ')"
[ "$AVAIL" -ge "$NEED" ] || die "$RUN has $((AVAIL / MiB)) MiB free; the sources, staging and restores need $((NEED / MiB)) MiB"
mkdir -p "$TAPECTL_HOME"
R1_HOME="$RUN/r1-home"
STAGING="$RUN/staging"; R1_STAGING="$RUN/r1-staging"; SRC="$RUN/src"; WORK="$RUN/work"
mkdir -p "$STAGING" "$R1_STAGING" "$SRC" "$WORK" "$RUN/tmp" "$R1_HOME"
CFG="$TAPECTL_HOME/config.toml"
DB="$TAPECTL_HOME/tapectl.db"

export TAPECTL_HOME_POINTER="$RUN/no-home-pointer"
TEE_OPT=(); if tee --output-error=warn </dev/null >/dev/null 2>&1; then TEE_OPT=(--output-error=warn); fi
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "${TEE_OPT[@]}" "$RUN/run.log"; }
trap '' HUP

# A page this script armed by hand (step 1a; step 5's rehearsal) is put back on ANY exit: a
# MODE SELECT of the page as found, and st's block size back to the format's. Neither writes
# the medium.
ARMED_BY_HAND=0
put_page_back() {
    [ "$ARMED_BY_HAND" = 1 ] || return 0
    local c
    if c="$(page_raw 0 | page_py contents "${FOUND_METHOD:-00}" "${FOUND_LEN:-00}" 00)" \
        && sg_wr_mode --page=0x0a,0xf0 --dbd --contents="$c" "$DRIVE_SG" >/dev/null 2>&1; then
        ARMED_BY_HAND=0
    else
        echo "lbp-acceptance: COULD NOT put the drive's page back ($(page_state 2>&1)): run any tapectl tape command, or power the drive off and on" >&2
    fi
    mt -f "$TAPE_DEV" setblk "$FORMAT_BLOCK" >/dev/null 2>&1 || true
}
trap 'put_page_back' EXIT
FOUND_METHOD="$(sed -n 's/^method=\([0-9a-f]*\) .*/\1/p' <<<"$FOUND_PAGE")"
FOUND_LEN="$(sed -n 's/.* info_len=\([0-9a-f]*\) .*/\1/p' <<<"$FOUND_PAGE")"
page_set() { # page_set <method> <len> <flags>, hex: read-modify-write, PS cleared, never saved
    local c
    c="$(page_raw 0 | page_py contents "$1" "$2" "$3")" || return 1
    sg_wr_mode --page=0x0a,0xf0 --dbd --contents="$c" "$DRIVE_SG"
}

STEP=""; STEP_DIR="$RUN"
NOT_RUN=()
fail() {
    log "FAIL (step $STEP): $*"
    {
        echo "RESULT: FAIL"
        echo "step: $STEP"
        echo "why: $*"
        echo "The run stopped here. The cartridge is as this step left it: read the failure"
        echo "before anything writes it again. Everything is in $RUN."
    } > "$RUN/RESULT.txt"
    snapshot_drive "$STEP_DIR" "at-fail" || true
    cp -a "$DB" "$RUN/catalog-at-fail.db" 2>/dev/null || true
    echo "lbp-acceptance: FAIL at step $STEP: $*" >&2
    echo "lbp-acceptance: the run directory is $RUN" >&2
    exit 1
}

# ---------- the drive's state, read-only ----------
# One LOG SENSE per page (#328); never 0x2E (read-to-clear TapeAlert: only tapectl's sweep
# reads it).
logpage() { # <dir> <tag> <page>
    sg_logs --page="$3" --maxlen=65532 --raw "$DRIVE_SG" > "$1/$2.page_$3.bin" 2>"$1/$2.page_$3.err" \
        && sg_logs --in="$1/$2.page_$3.bin" --raw --pdt=1 > "$1/$2.page_$3.txt" 2>&1
}
snapshot_drive() { # <dir> <tag>
    local d="$1/drive-$2" f pg
    mkdir -p "$d"
    cat "$SYS_ST/options" > "$d/options.txt" 2>&1 || true
    for f in "$SYS_ST"/stats/*; do
        [ -f "$f" ] && printf '%s\t%s\n' "$(basename "$f")" "$(cat "$f" 2>/dev/null)" >> "$d/stats.tsv"
    done
    sg_read_attr "$DRIVE_SG" > "$d/mam.txt" 2>&1 || true
    for pg in 0x02 0x03 0x0c 0x17; do logpage "$d" snap "$pg" || true; done
    page_raw 0 > "$d/lbp-page.bin" 2>"$d/lbp-page.err" || true
    page_state > "$d/lbp-page.txt" 2>&1 || true
    mt -f "$TAPE_DEV" status > "$d/mt-status.txt" 2>&1 || true
}

# run <name> <expected rc> <cmd...>: stdout to <name>.out, stderr to <name>.err and the terminal
# (tapectl's progress), the command line to commands.log, and every session log the command
# wrote copied to <name>.logs/. Sets RC; FAILs on any other rc.
RC=0
LOG_DIRS=("$TAPECTL_HOME/logs" "$R1_HOME/logs")
run() {
    local name="$1" want="$2"; shift 2
    printf '+ %q' "$@" >> "$RUN/commands.log"; echo >> "$RUN/commands.log"
    log "  \$ $*"
    : > "$STEP_DIR/.t0-$name"
    set +e
    { "$@" 2>&1 1>&3 3>&- | tee "${TEE_OPT[@]}" "$STEP_DIR/$name.err" >&2; } 3>"$STEP_DIR/$name.out"
    RC="${PIPESTATUS[0]}"
    set -e
    echo "$RC" > "$STEP_DIR/$name.rc"
    collect_logs "$name"
    if [ "$want" != "any" ] && [ "$RC" != "$want" ]; then
        fail "$name exited $RC, expected $want (see $STEP_DIR/$name.err and .out)"
    fi
}
collect_logs() { # <name>: the session logs written since <name> started, into <name>.logs/
    local d f
    mkdir -p "$STEP_DIR/$1.logs"
    for d in "${LOG_DIRS[@]}"; do
        [ -d "$d" ] || continue
        while IFS= read -r f; do cp -a "$f" "$STEP_DIR/$1.logs/" 2>/dev/null || true; done \
            < <(find "$d" -type f -newer "$STEP_DIR/.t0-$1" 2>/dev/null)
    done
}
session_says() { # <name> <fixed text>: one of <name>'s session logs holds it
    grep -rlF -- "$2" "$STEP_DIR/$1.logs" >/dev/null 2>&1
}
tctl() { env -u TAPECTL_TEST_LBP_CORRUPT_BLOCK "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null; }
tctl_hooked() { # step 3's write: the test hook on this one command line
    env TAPECTL_TEST_LBP_CORRUPT_BLOCK="$HOOK_N" "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null
}
tctl_second() { # step 4's write: the hook's list on this one command line
    env TAPECTL_TEST_LBP_CORRUPT_BLOCK="$SECOND_LIST" "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" "$@" </dev/null
}
old() { env -u TAPECTL_TEST_LBP_CORRUPT_BLOCK "$OLD_BIN" --home "$R1_HOME" --config "$R1_HOME/config.toml" "$@" </dev/null; }
cand_r1() { env -u TAPECTL_TEST_LBP_CORRUPT_BLOCK "$TAPECTL_BIN" --home "$R1_HOME" --config "$R1_HOME/config.toml" "$@" </dev/null; }

begin_step() { # <number> <name> <what>
    STEP="$1 $2"
    STEP_DIR="$RUN/$1-$2"
    mkdir -p "$STEP_DIR"
    log "=== step $1: $2 -- $3"
    snapshot_drive "$STEP_DIR" before
    STEP_T0="$(date +%s)"
}
end_step() { # [note]
    snapshot_drive "$STEP_DIR" after
    page_is_clear || fail "the drive's page reads '$(page_state 2>&1)' at the end of the step: item 164 says tapectl leaves it clear"
    local secs=$(( $(date +%s) - STEP_T0 ))
    log "=== step $STEP: PASS ($((secs / 3600))h $(((secs % 3600) / 60))m $((secs % 60))s)${1:+ -- $1}"
    printf '%s\tPASS\t%s\t%s\n' "$STEP" "$secs" "${1:-}" >> "$RUN/steps.tsv"
}
skip_step() { # <number> <name> <why>
    log "=== step $1: $2 -- NOT RUN: $3"
    printf '%s %s\tNOT RUN\t0\t%s\n' "$1" "$2" "$3" >> "$RUN/steps.tsv"
    NOT_RUN+=("$1 $2")
}
assert_page_clear() { # <when>
    page_is_clear || fail "$1: the drive's page reads '$(page_state 2>&1)', not method 0 with neither check on"
    log "  the page after $1: $(page_state)"
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
write_lbp_of() { dbq "$DB" "SELECT coalesce(write_lbp, 'NULL') || char(9) || coalesce(write_lbp_note, '') FROM volumes WHERE label = ?" "$1"; }
status_of() { dbq "$DB" "SELECT status FROM volumes WHERE label = ?" "$1"; }
last_verify_of() { # <db> <label>: "<verify_type>\t<outcome>" of the volume's newest verification
    dbq "$1" "SELECT vs.verify_type, vs.outcome FROM verification_sessions vs JOIN volumes v ON v.id = vs.volume_id
        WHERE v.label = ? ORDER BY vs.id DESC LIMIT 1" "$2"
}

# assert_identical <src> <restored> <what>: lifecycle-suite.sh's: `diff -r --no-dereference` and a
# tree checksum must both agree.
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
assert_identical() {
    diff -r --no-dereference "$1" "$2" > "$STEP_DIR/diff-$3.txt" 2>&1 \
        || fail "$3: the restored tree differs from its source (diff-$3.txt)"
    local h1 h2
    h1="$(tree_checksum "$1")"; h2="$(tree_checksum "$2")"
    [ "$h1" = "$h2" ] || fail "$3: tree checksum $h2 restored, $h1 at the source"
    log "  $3: identical to its source (tree checksum $h1)"
}

random_file() { head -c "$2" /dev/urandom > "$1"; }
make_sources() {
    mkdir -p "$SRC/a/nested" "$SRC/b/raw" "$SRC/c" "$SRC/r"
    echo "plain content $STAMP" > "$SRC/a/plain.txt"
    printf '\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e content\n' > "$SRC/a/nested/ünïcödé 日本語.txt"
    : > "$SRC/a/empty.bin"
    ln -s plain.txt "$SRC/a/link-ok"
    random_file "$SRC/a/blob.bin" "$SIZE_A"
    local i each=$((SIZE_B / 6))
    for i in $(seq 1 6); do random_file "$SRC/b/raw/img-$i.raw" "$each"; done
    echo "photos manifest $STAMP" > "$SRC/b/index.txt"
    random_file "$SRC/c/tail.bin" "$SIZE_C"
    echo "the third unit $STAMP" > "$SRC/c/README.txt"
    random_file "$SRC/r/old.bin" "$SIZE_R"
    echo "written by a tapectl before 1.7.2, without LBP $STAMP" > "$SRC/r/README.txt"
}

extract_restore_sh() { # <dir>: RESTORE.sh off the loaded tape, as File 1 tells an heir to
    mkdir -p "$1"
    mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
    mt -f "$TAPE_DEV" setblk "$FORMAT_BLOCK" || fail "mt setblk failed"
    mt -f "$TAPE_DEV" fsf 2 || fail "mt fsf 2 failed"
    dd if="$TAPE_DEV" bs=512k 2>"$1/dd.err" | tr -d '\0' > "$1/RESTORE.sh" || fail "dd of File 2 failed"
    chmod +x "$1/RESTORE.sh"
    bash -n "$1/RESTORE.sh" || fail "the RESTORE.sh read off the tape does not parse"
}
heir() { # <script dir> <name> <expected rc> <RESTORE.sh args...>
    local dir="$1" name="$2" want="$3"; shift 3
    run "$name" "$want" env TAPE_DEVICE="$TAPE_DEV" TMPDIR="$RUN/tmp" "$dir/RESTORE.sh" "$@"
}

# The cartridge's next volume, in this home: one filemark at BOT by hand, so File 0 reads EMPTY,
# then `volume init --force` (an EMPTY File 0 that nothing in this catalog proves blank needs it;
# the consent is --i-will-lose-the-cartridge, or --mhvtl). The chip serial matches the
# cartridge's row, so binding marks the volume it displaces `erased` (ADR-0012 item 139's route,
# which `volume init --help` names).
reinit() { # <label>
    mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
    mt -f "$TAPE_DEV" setblk "$FORMAT_BLOCK" || fail "mt setblk failed"
    mt -f "$TAPE_DEV" weof 1 || fail "mt weof 1 at BOT failed"
    mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
    run "init-$1" 0 tctl --json volume init "$1" --device "$TAPE_DEV" --force
    grep -F "it is now marked erased" "$STEP_DIR/init-$1.err" >/dev/null \
        || fail "volume init $1 does not say the volume it displaced is now marked erased (binding)"
}

# The LBP expectations of a tapectl write, by mode: on the real drive the page armed and
# write_lbp 'on'; on mhvtl the fallback (cause 8), warned once, write_lbp 'off'.
expect_write_lbp() { # <name> <label>
    local got
    got="$(write_lbp_of "$2")"
    log "  $2: write_lbp ${got/$'\t'/, note: }"
    if [ "$MHVTL" = 1 ]; then
        case "$got" in
            "off"$'\t'*"st cannot carry a 524292-byte block"*) ;;
            *) fail "$2: write_lbp is '$got'; on mhvtl the write falls back by cause 8 (st cannot carry the 524,292-byte block)" ;;
        esac
        [ "$(grep -c "^$FALLBACK_WARNING" "$STEP_DIR/$1.err")" = 1 ] \
            || fail "$1: the fallback warning is not on stderr exactly once (item 165: once per drive)"
    else
        [ "$got" = "on"$'\t' ] || fail "$2: write_lbp is '$got', not 'on': LBP did not protect every block of the write"
        ! grep -F "$FALLBACK_WARNING" "$STEP_DIR/$1.err" >/dev/null \
            || fail "$1: the fallback warning is on stderr: LBP did not arm on the real drive"
        session_says "$1" "Logical Block Protection armed on" \
            || fail "$1: no session log says the page was armed"
    fi
}

# ============================================================
print_plan | tee "$RUN/PLAN.txt"
log "run $RUN"
log "drive $DRIVE_SERIAL ($SYS_MODEL) at $TAPE_DEV ($TAPE_REAL, $DRIVE_SG); cartridge chip serial $SERIAL"
log "mode: $([ "$MHVTL" = 1 ] && echo "mhvtl rehearsal: every tapectl open falls back" || echo "REAL DRIVE")"
log "tapectl: $TAPECTL_BIN ($TAPECTL_VERSION); OLD_BIN: ${OLD_BIN:-none} ${OLD_VERSION:+($OLD_VERSION)}"
log "home: $TAPECTL_HOME; R1 home: $R1_HOME; staging: $STAGING"
{
    echo "started=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "host=$(hostname)"
    echo "mode=$([ "$MHVTL" = 1 ] && echo mhvtl || echo real)"
    echo "device=$TAPE_DEV"; echo "device_real=$TAPE_REAL"; echo "sg=$DRIVE_SG"
    echo "drive_serial=$DRIVE_SERIAL"; echo "drive_model=$SYS_MODEL"; echo "cartridge_serial=$SERIAL"
    echo "tapectl=$TAPECTL_BIN"; echo "tapectl_version=$TAPECTL_VERSION"
    echo "tapectl_sha256=$(sha256sum "$TAPECTL_BIN" | awk '{print $1}')"
    echo "tapectl_file=$(file -b "$TAPECTL_BIN" 2>/dev/null || echo unknown)"
    echo "old_bin=${OLD_BIN:-}"; echo "old_version=$OLD_VERSION"
    echo "dar=$("$DAR_BIN" --version 2>&1 | sed -n 1p)"
    echo "age=$(age --version 2>&1 | sed -n 1p)"
    echo "tar=$(tar --version 2>&1 | sed -n 1p)"
    echo "sg3_utils=$(sg_wr_mode --version 2>&1 | sed -n 1p)"
    echo "script_commit=$(git -C "$SCRIPT_DIR" describe --always --dirty 2>/dev/null || echo unknown)"
    echo "page_found=$FOUND_PAGE"; echo "slice_size=$SLICE_SIZE"; echo "raw_block=$RAW_BLOCK"
} > "$RUN/meta.txt"
: > "$RUN/steps.tsv"

# ------------------------------------------------------------
begin_step 0 preflight "the page, block limits and identity, read only"
page_raw 0 > "$STEP_DIR/page-current.bin"
page_raw 1 > "$STEP_DIR/page-changeable.bin" 2>"$STEP_DIR/page-changeable.err" || true
od -An -tx1 -v "$STEP_DIR/page-current.bin" > "$STEP_DIR/page-current.txt"
od -An -tx1 -v "$STEP_DIR/page-changeable.bin" > "$STEP_DIR/page-changeable.txt" 2>/dev/null || true
log "  page now: $FOUND_PAGE (raw: $(tr -s ' \n' ' ' < "$STEP_DIR/page-current.txt"))"
log "  page changeable: $(tr -s ' \n' ' ' < "$STEP_DIR/page-changeable.txt")"
if command -v sg_read_block_limits >/dev/null; then
    sg_read_block_limits "$DRIVE_SG" > "$STEP_DIR/block-limits.txt" 2>&1 || true
    log "  block limits: $(tr '\n' ' ' < "$STEP_DIR/block-limits.txt")"
fi
sg_inq "$DRIVE_SG" > "$STEP_DIR/sg_inq.txt" 2>&1 || true
log "  $(grep -E 'Vendor|Product|revision' "$STEP_DIR/sg_inq.txt" | tr -s ' ' | tr '\n' ' ')"
log "  the sg node: $DRIVE_SG $(stat -c '%A %U:%G' "$DRIVE_SG"); step 8 judges the service user's access"
end_step

# ------------------------------------------------------------
begin_step 1 r1 "R1: blocks written without LBP, read with LBP_R armed (item 175's switch)"
# 1a, the prototype's route: dd, the page set by hand, lbp.py.
RAW_N=16
RAW_WIRE=$((RAW_BLOCK + 4))
random_file "$STEP_DIR/raw.bin" $((RAW_N * RAW_BLOCK))
mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
mt -f "$TAPE_DEV" setblk "$RAW_BLOCK" || fail "mt setblk $RAW_BLOCK failed"
dd if="$STEP_DIR/raw.bin" of="$TAPE_DEV" bs="$RAW_BLOCK" count="$RAW_N" 2>"$STEP_DIR/raw-write.err" \
    || fail "dd of $RAW_N blocks without LBP failed (raw-write.err)"
mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
log "  1a: $RAW_N blocks of $RAW_BLOCK bytes written with the page clear (no CRC sent)"
ARMED_BY_HAND=1
page_set 01 04 c0 > "$STEP_DIR/page-set.out" 2>&1 || fail "sg_wr_mode refused method 01h with LBP_W and LBP_R (page-set.out)"
ARMED_NOW="$(page_state)"
[ "$ARMED_NOW" = "method=01 info_len=04 flags=c0" ] || fail "the page set by hand reads '$ARMED_NOW', not method 01h, length 4, flags c0"
mt -f "$TAPE_DEV" setblk "$RAW_WIRE" || fail "mt setblk $RAW_WIRE failed"
set +e
dd if="$TAPE_DEV" of="$STEP_DIR/raw.lbp" bs="$RAW_WIRE" count="$RAW_N" 2>"$STEP_DIR/raw-read.err"
RAW_DD_RC=$?
python3 -I "$LBP_PY" verify "$STEP_DIR/raw.lbp" --block "$RAW_BLOCK" > "$STEP_DIR/raw-verify.txt" 2>&1
RAW_VERIFY_RC=$?
set -e
python3 - "$STEP_DIR/raw.bin" "$STEP_DIR/raw.lbp" "$RAW_BLOCK" > "$STEP_DIR/raw-payload.txt" 2>&1 <<'PY'
import sys
want, got, n = open(sys.argv[1], "rb").read(), open(sys.argv[2], "rb").read(), int(sys.argv[3])
blocks = len(got) // (n + 4)
same = sum(1 for i in range(blocks) if got[i * (n + 4):i * (n + 4) + n] == want[i * n:(i + 1) * n])
print("payload: %d of %d block(s) read with a CRC; %d equal to what was written" % (blocks, len(want) // n, same))
sys.exit(0 if blocks == len(want) // n and same == blocks and len(got) == blocks * (n + 4) else 1)
PY
RAW_PAYLOAD_RC=$?
put_page_back
assert_page_clear "1a's hand-armed read"
mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
log "  1a: dd rc $RAW_DD_RC ($(tr '\n' ' ' < "$STEP_DIR/raw-read.err")); $(tail -1 "$STEP_DIR/raw-verify.txt"); $(cat "$STEP_DIR/raw-payload.txt")"
if [ "$RAW_DD_RC" = 0 ] && [ "$RAW_VERIFY_RC" = 0 ] && [ "$RAW_PAYLOAD_RC" = 0 ]; then
    R1_RAW=yes
else
    R1_RAW=no
fi
log "  1a: R1 by the prototype's route: $R1_RAW"
# A filemark at BOT: File 0 EMPTY for OLD_BIN's volume init (on mhvtl it reads the old bytes
# as an unparseable File 0, issue #327; --force takes either).
mt -f "$TAPE_DEV" weof 1 || fail "mt weof 1 at BOT failed"
mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"

# 1b, tapectl's route.
R1_POLICY=""
if [ -n "${OLD_BIN:-}" ]; then
    make_sources
    run r1-init 0 old init --operator "$OPERATOR-r1" --no-escrow
    run r1-escrow 0 old key generate --escrow
    chmod 600 "$STEP_DIR/r1-escrow.out" "$STEP_DIR/r1-escrow.err"
    run r1-cfg-dar 0 old config set dar.binary "\"$DAR_BIN\""
    run r1-cfg-slice 0 old config set defaults.slice_size "\"$SLICE_SIZE\""
    run r1-cfg-copies 0 old config set defaults.min_copies 1
    run r1-cfg-staging 0 old config set staging.directory "\"$R1_STAGING\""
    run r1-backend 0 old backend add --name lba --device-tape "$TAPE_DEV" --generation LTO-6
    run r1-tenant 0 old tenant add "$TENANT"
    run r1-unit 0 old unit init "$SRC/r" --tenant "$TENANT" --name "$UNIT_R"
    run r1-snapshot 0 old snapshot create "$UNIT_R"
    run r1-stage 0 old --yes stage create "$UNIT_R"
    run r1-vol-init 0 old volume init "$LABEL1" --device "$TAPE_DEV" --force
    run r1-write 0 old --yes volume write "$LABEL1" --device "$TAPE_DEV"
    log "  1b: $OLD_VERSION wrote and sealed $LABEL1 without LBP"
    cp -a "$R1_HOME/tapectl.db" "$RUN/r1-catalog-before-verify.db"
    run r1-verify any cand_r1 volume verify "$LABEL1" --device "$TAPE_DEV"
    R1_VERIFY_RC="$RC"
    if session_says r1-verify "sends its own with every block read"; then
        R1_POLICY=all
    elif session_says r1-verify "Logical Block Protection is not available on"; then
        R1_POLICY=unavailable
    else
        R1_POLICY=confirm-only
    fi
    log "  1b: the candidate's verify exited $R1_VERIFY_RC; what it armed for the read: $R1_POLICY; the verification recorded: $(last_verify_of "$R1_HOME/tapectl.db" "$LABEL1" | tr '\t' ' ')"
else
    log "  1b: no OLD_BIN: tapectl's route not run (the rehearsal's only)"
fi

# The answer, and what it means for item 175.
{
    echo "R1 by the prototype's route (1a, dd + lbp.py): $R1_RAW"
    echo "  dd rc $RAW_DD_RC; $(tail -1 "$STEP_DIR/raw-verify.txt"); $(cat "$STEP_DIR/raw-payload.txt")"
    [ -z "$R1_POLICY" ] || echo "R1 by tapectl's route (1b): the candidate's verify of $LABEL1 exited $R1_VERIFY_RC, arming '$R1_POLICY' for the read"
} > "$RUN/R1.txt"
if [ "$MHVTL" = 1 ]; then
    echo "REHEARSAL: mhvtl computes a CRC for every block it returns; this says nothing of the HP." >> "$RUN/R1.txt"
    [ "$R1_RAW" = yes ] || fail "1a on mhvtl: the hand-armed read did not return a valid CRC for every block (R1.txt): the script's route is broken"
    if [ -n "$R1_POLICY" ]; then
        [ "$R1_POLICY" = unavailable ] || fail "1b on mhvtl: the candidate armed '$R1_POLICY' for a read at 524,288; mhvtl cannot carry the wire block, so it must fall back"
        [ "$R1_VERIFY_RC" = 0 ] || fail "1b on mhvtl: the candidate's verify of $LABEL1 exited $R1_VERIFY_RC"
    fi
    R1_POLICY=fallback
    R1_NOTE="rehearsal"
else
    case "$R1_RAW/$R1_POLICY" in
        yes/all)
            [ "$R1_VERIFY_RC" = 0 ] || fail "R1: the drive returns valid CRCs for blocks written without LBP (1a), yet the candidate's verify of $LABEL1 with LBP_R exited $R1_VERIFY_RC: tapectl's read path, not the drive (r1-verify.err, its session log)"
            R1_NOTE="R1 = YES: the HP returns a valid CRC for blocks written without LBP. Item 175's switch stays at LBP_R on every read (LbpReadPolicy::AllReads, item 162): verify and restore of production's older tapes run with LBP_R."
            ;;
        yes/confirm-only)
            fail "R1: the drive returns valid CRCs for blocks written without LBP (1a), so item 175's fallback is not warranted, but the candidate is built with it (its verify armed no read check): rebuild with LBP_R_POLICY = LbpReadPolicy::AllReads (src/tape/lbp.rs) and run again"
            ;;
        no/all)
            echo "R1 = NO: item 175's fallback. Rebuild the candidate with LBP_R_POLICY = LbpReadPolicy::OnlyConfirmOfLbpWrite (src/tape/lbp.rs) and run again." >> "$RUN/R1.txt"
            fail "R1 = NO: the HP returns no valid CRC for blocks written without LBP (1a: R1.txt), and the candidate's verify exited $R1_VERIFY_RC. Item 175: LBP_R only for the confirm of a session that wrote with LBP. Rebuild the candidate with LBP_R_POLICY = LbpReadPolicy::OnlyConfirmOfLbpWrite (src/tape/lbp.rs) and run again"
            ;;
        no/confirm-only)
            [ "$R1_VERIFY_RC" = 0 ] || fail "R1 = NO and the candidate carries item 175's fallback, yet its verify of $LABEL1 (no read check) exited $R1_VERIFY_RC"
            R1_NOTE="R1 = NO: the HP returns no valid CRC for blocks written without LBP. The candidate carries item 175's fallback (LBP_R only for the confirm of a session that wrote with LBP), as it must."
            ;;
        */unavailable)
            fail "1b: LBP could not be armed for the candidate's read on the real drive (r1-verify's session log names why): R11, sg3-utils, or the drive; R1's tapectl route is unanswered"
            ;;
        *) fail "R1: no answer from tapectl's route (1b)" ;;
    esac
fi
echo "$R1_NOTE" >> "$RUN/R1.txt"
log "  $R1_NOTE"
end_step "$R1_NOTE"

if [ "$R1_ONLY" = 1 ]; then
    { echo "RESULT: R1 ANSWERED (--r1-only; steps 2-9 not run)"; cat "$RUN/R1.txt"; } > "$RUN/RESULT.txt"
    log "RESULT: --r1-only: R1 answered, the run stops here. $RUN/R1.txt"
    log "The cartridge holds $LABEL1, in $R1_HOME."
    exit 3
fi

# ------------------------------------------------------------
begin_step 2 write "$LABEL2 written with LBP and read back in full"
[ -d "$SRC/a" ] || make_sources
run init 0 tctl init --operator "$OPERATOR" --no-escrow
run escrow 0 tctl key generate --escrow
chmod 600 "$STEP_DIR/escrow.out" "$STEP_DIR/escrow.err"
run cfg-dar 0 tctl config set dar.binary "\"$DAR_BIN\""
run cfg-slice 0 tctl config set defaults.slice_size "\"$SLICE_SIZE\""
run cfg-copies 0 tctl config set defaults.min_copies 1
run cfg-staging 0 tctl config set staging.directory "\"$STAGING\""
run backend 0 tctl backend add --name lba --device-tape "$TAPE_DEV" --generation LTO-6
run config-check 0 tctl config check
cp "$CFG" "$RUN/config.toml"
run tenant 0 tctl tenant add "$TENANT"
run unit-a 0 tctl unit init "$SRC/a" --tenant "$TENANT" --name "$UNIT_A"
run unit-b 0 tctl unit init "$SRC/b" --tenant "$TENANT" --name "$UNIT_B"
run unit-c 0 tctl unit init "$SRC/c" --tenant "$TENANT" --name "$UNIT_C"
for u in "$UNIT_A" "$UNIT_B" "$UNIT_C"; do
    run "snapshot-$u" 0 tctl snapshot create "$u"
    run "stage-$u" 0 tctl --yes stage create "$u"
done
# The cartridge holds LBA-0001 from R1's home (or 1a's blocks): this home's own erase.
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
    nothing_to_erase) INIT_FORCE=(--force) ;;
    *) fail "cartridge erase --dry-run reported '$PROBE'" ;;
esac
run init2 0 tctl --json volume init "$LABEL2" --device "$TAPE_DEV" "${INIT_FORCE[@]}"
run write2 0 tctl --json --yes volume write "$LABEL2" --device "$TAPE_DEV" --full-confirm --keep-staging
expect_write_lbp write2 "$LABEL2"
[ "$(status_of "$LABEL2")" = sealed ] || fail "$LABEL2 is '$(status_of "$LABEL2")', not sealed"
[ "$(last_verify_of "$DB" "$LABEL2")" = "full"$'\t'"passed" ] || fail "$LABEL2's confirm is '$(last_verify_of "$DB" "$LABEL2")', not a passed full readback"
if [ "$R1_POLICY" = confirm-only ]; then
    session_says write2 "read check armed on" || fail "write2: item 175's fallback arms LBP_R for the confirm of a session that wrote with LBP, and no session log says it did"
fi
dbq "$DB" "SELECT e.timestamp, e.new_value, e.details FROM events e JOIN volumes v ON v.id = e.entity_id
    WHERE e.entity_type = 'volume' AND e.action = 'write_lbp' AND v.label = ?" "$LABEL2" > "$STEP_DIR/write-lbp-events.tsv"
log "  write_lbp event: $(tail -1 "$STEP_DIR/write-lbp-events.tsv")"
run volume-info 0 tctl volume info "$LABEL2"
grep -i "write check" "$STEP_DIR/volume-info.out" | sed 's/^/  volume info: /' | tee -a "$RUN/run.log" || true
HEIR2="$WORK/heir-$LABEL2"
extract_restore_sh "$HEIR2"
cp -a "$DB" "$RUN/catalog-after-write2.db"
end_step

# ------------------------------------------------------------
begin_step 3 refusal "the drive refuses one block of $LABEL3; item 174 writes on from the file's start"
reinit "$LABEL3"
TOTAL_BYTES="$(dbq "$DB" "SELECT coalesce(sum(sl.encrypted_bytes), 0) FROM stage_slices sl
    JOIN stage_sets ss ON ss.id = sl.stage_set_id WHERE ss.status = 'staged'")"
HOOK_N=$(( TOTAL_BYTES / FORMAT_BLOCK / 2 ))
[ "$HOOK_N" -ge 8 ] || HOOK_N=8
log "  the hook: block write $HOOK_N since the write store opened (of about $((TOTAL_BYTES / FORMAT_BLOCK)) data blocks)"
dmesg > "$STEP_DIR/dmesg-before.txt" 2>&1 || echo "dmesg needs privilege here: run 'sudo dmesg' by hand around this step" > "$STEP_DIR/dmesg-before.txt"
if [ "$MHVTL" = 1 ]; then
    # Every open falls back on mhvtl, and the hook refuses an open that is not armed: the write
    # stops before anything is written (store::lbp_corrupt_hook).
    run write3 any tctl_hooked --json --yes volume write "$LABEL3" --device "$TAPE_DEV" --full-confirm --keep-staging
    [ "$RC" != 0 ] || fail "write3: the hook ran on an open that could not arm LBP: it must refuse"
    grep -F "is set, but Logical Block Protection is not armed" "$STEP_DIR/write3.err" >/dev/null \
        || fail "write3: the refusal does not say the hook needs LBP armed"
    log "  rehearsal: the hook refused the unarmed open (exit $RC); the drive's refusal is the real drive's"
    run write3b 0 tctl --json --yes volume write "$LABEL3" --device "$TAPE_DEV" --full-confirm --keep-staging
    expect_write_lbp write3b "$LABEL3"
else
    run write3 0 tctl_hooked --json --yes volume write "$LABEL3" --device "$TAPE_DEV" --full-confirm --keep-staging
    grep -F "TAPECTL_TEST_LBP_CORRUPT_BLOCK=$HOOK_N is set" "$STEP_DIR/write3.err" >/dev/null \
        || fail "write3: the hook's announcement is not on stderr"
    session_says write3 "(ADR-0012 item 174)" || fail "write3: no session log names item 174's retry"
    dbq "$DB" "SELECT e.new_value, e.details FROM events e JOIN volumes v ON v.id = e.entity_id
        WHERE e.entity_type = 'volume' AND e.action = 'write_lbp_retry' AND v.label = ?" "$LABEL3" > "$STEP_DIR/retry-events.tsv"
    [ "$(wc -l < "$STEP_DIR/retry-events.tsv")" = 1 ] || fail "write3: $(wc -l < "$STEP_DIR/retry-events.tsv") write_lbp_retry event(s) for $LABEL3, want exactly one"
    log "  item 174: the retry wrote on from tape file $(cut -f1 "$STEP_DIR/retry-events.tsv"): $(cut -f2 "$STEP_DIR/retry-events.tsv")"
    expect_write_lbp write3 "$LABEL3"
fi
dmesg > "$STEP_DIR/dmesg-after.txt" 2>&1 || echo "dmesg needs privilege here" > "$STEP_DIR/dmesg-after.txt"
grep -i "guard check\|logical block" "$STEP_DIR/dmesg-after.txt" | tail -5 | sed 's/^/  dmesg: /' | tee -a "$RUN/run.log" || true
[ "$(status_of "$LABEL3")" = sealed ] || fail "$LABEL3 is '$(status_of "$LABEL3")', not sealed"
[ "$(last_verify_of "$DB" "$LABEL3")" = "full"$'\t'"passed" ] || fail "$LABEL3's confirm is '$(last_verify_of "$DB" "$LABEL3")', not a passed full readback"
cp -a "$DB" "$RUN/catalog-after-write3.db"
end_step

# ------------------------------------------------------------
if [ "$MHVTL" = 1 ]; then
    skip_step 4 second "every open falls back on mhvtl, and the hook refuses an open that is not armed"
else
    begin_step 4 second "two refusals in one run: interrupted, then volume resume completes it"
    if [ -z "$SECOND_A" ]; then
        SECOND_A="$HOOK_N"; SECOND_B=$((HOOK_N + 64)); SECOND_C=$((HOOK_N + 320))
    fi
    SECOND_LIST="$(second_list "$SECOND_A" "$SECOND_B" "$SECOND_C")"
    log "  the hook: block write $SECOND_A, then every one from $SECOND_B to $SECOND_C, counted since the write opened the drive (re-sent blocks too)"
    reinit "$LABEL4"
    run write4 any tctl_second --json --yes volume write "$LABEL4" --device "$TAPE_DEV" --keep-staging
    [ "$RC" = 4 ] || fail "write4 exited $RC: a second refusal in one run leaves the session interrupted, exit 4 (item 174). Exit 0 means no listed block of $SECOND_B..$SECOND_C reached the retry (give a later range); exit 2 with 'is not a block number' means this candidate's hook takes one number"
    grep -F "second write the drive refused" "$STEP_DIR/write4.err" >/dev/null || fail "write4: the message does not name the second refusal"
    session_says write4 "(ADR-0012 item 174)" || fail "write4: no session log names item 174's first retry"
    dbq "$DB" "SELECT e.new_value, e.details FROM events e JOIN volumes v ON v.id = e.entity_id
        WHERE e.entity_type = 'volume' AND e.action = 'write_lbp_retry' AND v.label = ?" "$LABEL4" > "$STEP_DIR/retry-events.tsv"
    [ "$(wc -l < "$STEP_DIR/retry-events.tsv")" = 1 ] || fail "write4: $(wc -l < "$STEP_DIR/retry-events.tsv") write_lbp_retry event(s) for $LABEL4, want exactly one (one retry, then interrupted)"
    log "  the first refusal's retry: $(tr '\t' ' ' < "$STEP_DIR/retry-events.tsv")"
    run resume4 0 tctl --json --yes volume resume "$LABEL4" --device "$TAPE_DEV" --keep-staging
    [ "$(status_of "$LABEL4")" = sealed ] || fail "$LABEL4 is '$(status_of "$LABEL4")' after the resume, not sealed"
    expect_write_lbp resume4 "$LABEL4"
    cp -a "$DB" "$RUN/catalog-after-write4.db"
    end_step
fi

# ------------------------------------------------------------
begin_step 5 crash "$LABEL5's write killed mid-way: the page left armed, then cleared by the next open"
reinit "$LABEL5"
W0="$(cat "$SYS_ST/stats/write_byte_cnt")"
PARK="$STEP_DIR/parked-$LABEL5"
: > "$STEP_DIR/.t0-write5"
printf '+ %q' env "$TAPECTL_BIN" volume write "$LABEL5" "(killed with SIGKILL)" >> "$RUN/commands.log"; echo >> "$RUN/commands.log"
if [ "$MHVTL" = 1 ]; then
    # mhvtl writes this rehearsal's few MiB faster than any poll: the writer parks after its
    # plan, store open (TAPECTL_TEST_PAUSE_AFTER_PLAN, the gate's interrupt legs' hook), and is
    # killed there. No page is armed on mhvtl either way.
    env -u TAPECTL_TEST_LBP_CORRUPT_BLOCK TAPECTL_TEST_PAUSE_AFTER_PLAN="$PARK" \
        "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" \
        --yes volume write "$LABEL5" --device "$TAPE_DEV" --keep-staging \
        </dev/null >"$STEP_DIR/write5.out" 2>"$STEP_DIR/write5.err" &
else
    env -u TAPECTL_TEST_LBP_CORRUPT_BLOCK "$TAPECTL_BIN" --home "$TAPECTL_HOME" --config "$CFG" \
        --yes volume write "$LABEL5" --device "$TAPE_DEV" --keep-staging \
        </dev/null >"$STEP_DIR/write5.out" 2>"$STEP_DIR/write5.err" &
fi
WPID=$!
# The PID is the binary's own (env execs it), checked by its executable before it is killed:
# on the real drive once KILL_AFTER bytes have gone through st (a fact, not a sleep), on mhvtl
# once the writer has parked.
KILLED=0
T0=$SECONDS
while kill -0 "$WPID" 2>/dev/null && [ $((SECONDS - T0)) -lt 1800 ]; do
    if [ "$MHVTL" = 1 ]; then
        [ -e "$PARK" ] && READY=1 || READY=0
    else
        W1="$(cat "$SYS_ST/stats/write_byte_cnt")"
        [ $((W1 - W0)) -ge "$KILL_AFTER" ] && READY=1 || READY=0
    fi
    if [ "$READY" = 1 ]; then
        [ "$(readlink -f "/proc/$WPID/exe" 2>/dev/null)" = "$TAPECTL_BIN" ] \
            || fail "PID $WPID is not $TAPECTL_BIN: not killed"
        kill -9 "$WPID"
        KILLED=1
        break
    fi
    sleep 0.2
done
set +e; wait "$WPID"; WRC=$?; set -e
collect_logs write5
[ "$KILLED" = 1 ] || fail "write5 ended (exit $WRC) or timed out before it was ready to kill: nothing was killed mid-write"
log "  write5 killed with SIGKILL after $(( ($(cat "$SYS_ST/stats/write_byte_cnt") - W0) / MiB )) MiB reached st (exit $WRC)"
LEFT="$(page_state 2>&1)"
log "  the page after the SIGKILL: $LEFT"
# From here a page left armed is put back by the exit trap if the run fails, after the at-fail
# snapshot has recorded it.
if [ "$MHVTL" = 1 ]; then
    # Every open falls back on mhvtl, so the killed write never armed the page: arm it by hand
    # to stand for the leftover a crash leaves on the real drive.
    page_is_clear || fail "write5 never armed the page on mhvtl, yet it reads '$LEFT'"
    ARMED_BY_HAND=1
    page_set 01 04 c0 > "$STEP_DIR/page-set.out" 2>&1 || fail "sg_wr_mode refused the hand-armed leftover"
    log "  rehearsal: the leftover armed by hand: $(page_state)"
else
    ARMED_BY_HAND=1
    case "$R1_POLICY" in
        all) WANT_LEFT="method=01 info_len=04 flags=c0" ;;          # LBP_W and LBP_R
        *) WANT_LEFT="method=01 info_len=04 flags=80" ;;            # item 175: LBP_W alone
    esac
    [ "$LEFT" = "$WANT_LEFT" ] \
        || fail "after a SIGKILL mid-write the page reads '$LEFT': item 164 expects the leftover, '$WANT_LEFT'"
fi
# The heir, before tapectl runs again: the first dd of File 2 and RESTORE.sh --info (the copy
# step 2 read off $LABEL2; a tape's own RESTORE.sh cannot be read now).
mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
mt -f "$TAPE_DEV" setblk "$FORMAT_BLOCK" || fail "mt setblk failed"
set +e
mt -f "$TAPE_DEV" fsf 2 2>"$STEP_DIR/heir-fsf.err"
dd if="$TAPE_DEV" of="$STEP_DIR/heir-file2.bin" bs=512k 2>"$STEP_DIR/heir-dd.err"
HEIR_DD_RC=$?
set -e
HEIR_DD_BYTES="$(stat -c %s "$STEP_DIR/heir-file2.bin" 2>/dev/null || echo 0)"
log "  the heir's dd with the page left armed: rc $HEIR_DD_RC, $HEIR_DD_BYTES bytes ($(tr '\n' ' ' < "$STEP_DIR/heir-dd.err"))"
heir "$HEIR2" info-leftover any --info
INFO_RC="$RC"
log "  RESTORE.sh --info with the page left armed: exit $INFO_RC"
if [ "$MHVTL" = 0 ]; then
    [ "$HEIR_DD_RC" != 0 ] && [ "$HEIR_DD_BYTES" = 0 ] \
        || fail "with the page left armed the heir's dd read $HEIR_DD_BYTES bytes (rc $HEIR_DD_RC): item 164 says a refusal, never data"
    [ "$INFO_RC" != 0 ] || fail "RESTORE.sh --info succeeded with the page left armed"
    ! grep -F "Verdict: SEALED" "$STEP_DIR/info-leftover.out" >/dev/null \
        || fail "RESTORE.sh --info called the tape sealed with the page left armed"
fi
# The next tapectl open: identify clears it (re-armed and cleared at close under item 162's
# policy; cleared at once by an open that arms nothing or cannot arm).
run identify 0 tctl volume identify --device "$TAPE_DEV"
grep -F "$LEFTOVER_NOTE" "$STEP_DIR/identify.err" >/dev/null || fail "volume identify did not print the leftover note"
log "  $(grep -F "$LEFTOVER_NOTE" "$STEP_DIR/identify.err" | sed -n 1p | cut -c1-200)"
assert_page_clear "volume identify over the leftover"
ARMED_BY_HAND=0
# Reads work again: the killed write left the tape unsealed, so any verdict will do.
heir "$HEIR2" info-after any --info
grep -F "Verdict: " "$STEP_DIR/info-after.out" >/dev/null \
    || fail "RESTORE.sh --info gives no verdict with the page clear (exit $RC): its reads still fail"
log "  RESTORE.sh --info reads the tape again with the page clear: $(grep -F "Verdict: " "$STEP_DIR/info-after.out" | sed -n 1p)"
run resume5 0 tctl --json --yes volume resume "$LABEL5" --device "$TAPE_DEV" --keep-staging
[ "$(status_of "$LABEL5")" = sealed ] || fail "$LABEL5 is '$(status_of "$LABEL5")' after the resume, not sealed"
expect_write_lbp resume5 "$LABEL5"
cp -a "$DB" "$RUN/catalog-after-crash.db"
end_step

# ------------------------------------------------------------
begin_step 6 verify "volume verify $LABEL5 --full, with LBP_R under item 162's policy"
run verify6 0 tctl volume verify "$LABEL5" --device "$TAPE_DEV"
[ "$(last_verify_of "$DB" "$LABEL5")" = "full"$'\t'"passed" ] || fail "$LABEL5's verify is '$(last_verify_of "$DB" "$LABEL5")', not a passed full readback"
case "$R1_POLICY" in
    all) session_says verify6 "sends its own with every block read" || fail "verify6: no session log says LBP_R was armed for the read" ;;
    confirm-only) ! session_says verify6 "Logical Block Protection armed on" || fail "verify6: item 175's fallback arms nothing for a verify, and a session log says it armed" ;;
    fallback) session_says verify6 "Logical Block Protection is not available on" || fail "verify6: the rehearsal's read falls back, and no session log says so" ;;
esac
end_step

# ------------------------------------------------------------
begin_step 7 heir "RESTORE.sh off $LABEL5 with the real tools, the page clear, no manual step"
HEIR5="$WORK/heir-$LABEL5"
extract_restore_sh "$HEIR5"
heir "$HEIR5" heir-verify 0 --verify
grep -F "VERIFY: PASS" "$STEP_DIR/heir-verify.out" >/dev/null || fail "RESTORE.sh --verify did not PASS"
TENANT_KEY="$TAPECTL_HOME/keys/$TENANT-primary.age.key"
[ -f "$TENANT_KEY" ] || fail "the tenant key $TENANT_KEY is missing"
heir "$HEIR5" heir-restore 0 --restore --unit "$UNIT_B" --key "$TENANT_KEY" --to "$WORK/restore-heir-b"
assert_identical "$SRC/b" "$WORK/restore-heir-b" heir-restore
mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
mt -f "$TAPE_DEV" setblk 0 || fail "mt setblk 0 failed"
dd if="$TAPE_DEV" of="$STEP_DIR/record0.bin" bs=1M count=1 2>"$STEP_DIR/record0.err" || fail "the variable-mode read of File 0 failed"
mt -f "$TAPE_DEV" setblk "$FORMAT_BLOCK" || fail "mt setblk failed"
mt -f "$TAPE_DEV" rewind || fail "mt rewind failed"
REC="$(stat -c %s "$STEP_DIR/record0.bin")"
[ "$REC" = "$FORMAT_BLOCK" ] || fail "one record of an LBP-written tape read as $REC bytes in variable mode, not $FORMAT_BLOCK: the CRC would be on the medium"
log "  one record in variable mode: $REC bytes (no CRC on the medium)"
end_step

# ------------------------------------------------------------
begin_step 8 r11 "R11: the $SVC_USER service user's access to $DRIVE_SG"
R11_NOTE=""
if ! getent passwd "$SVC_USER" >/dev/null; then
    [ "$MHVTL" = 1 ] || fail "no '$SVC_USER' service user on this host: R11 cannot be answered here"
    R11_NOTE="no '$SVC_USER' user on this host (the rehearsal's): not checked"
else
    can_rw() { # <node>: can the service user open it read-write, by owner, group, other or ACL?
        python3 - "$1" "$SVC_USER" "$(id -Gn "$SVC_USER")" "$(getfacl -p "$1" 2>/dev/null || true)" <<'PY'
import grp, os, pwd, stat, sys
node, user, groups, acl = sys.argv[1], sys.argv[2], sys.argv[3].split(), sys.argv[4]
st = os.stat(node)
owner = pwd.getpwuid(st.st_uid).pw_name
group = grp.getgrgid(st.st_gid).gr_name
m = st.st_mode
how = []
if owner == user and m & stat.S_IRUSR and m & stat.S_IWUSR:
    how.append("owner")
if group in groups and m & stat.S_IRGRP and m & stat.S_IWGRP:
    how.append("group " + group)
if m & stat.S_IROTH and m & stat.S_IWOTH:
    how.append("other")
for line in acl.splitlines():
    parts = line.split(":")
    if len(parts) >= 3 and "rw" in parts[2][:2]:
        if parts[0] == "user" and parts[1] == user:
            how.append("ACL user")
        if parts[0] == "group" and parts[1] in groups:
            how.append("ACL group " + parts[1])
print("%s %s:%s %o: %s" % (node, owner, group, m & 0o777, ", ".join(how) if how else "NO read-write access"))
sys.exit(0 if how else 1)
PY
    }
    set +e
    SG_ACCESS="$(can_rw "$DRIVE_SG")"; SG_OK=$?
    ST_ACCESS="$(can_rw "$TAPE_REAL")"; ST_OK=$?
    set -e
    log "  $SVC_USER on the sg node: $SG_ACCESS"
    log "  $SVC_USER on the st node: $ST_ACCESS"
    [ "$SG_OK" = 0 ] || fail "R11: the $SVC_USER service user cannot open $DRIVE_SG read-write ($SG_ACCESS): every production write would fall back without LBP (item 165). Give it the node's group or an ACL, then run step 8 again"
    [ "$ST_OK" = 0 ] || fail "R11: the $SVC_USER service user cannot open $TAPE_REAL read-write ($ST_ACCESS)"
    R11_NOTE="$SVC_USER: $SG_ACCESS"
fi
end_step "$R11_NOTE"

# ------------------------------------------------------------
begin_step 9 rate "step 2's write rate with LBP on"
RATE_ROW="$(dbq "$DB" "SELECT pt.bytes, pt.duration_ms FROM phase_timings pt JOIN volumes v ON v.id = pt.volume_id
    WHERE v.label = ? AND pt.operation = 'volume write' AND pt.phase = 'write' ORDER BY pt.id DESC LIMIT 1" "$LABEL2")"
WBYTES="$(cut -f1 <<<"$RATE_ROW")"; WMS="$(cut -f2 <<<"$RATE_ROW")"
case "$WBYTES/$WMS" in *[!0-9/]*|/*|*/) fail "no write phase recorded for $LABEL2 (phase_timings): '$RATE_ROW'" ;; esac
[ "$WMS" -gt 0 ] || fail "the write phase of $LABEL2 lasted 0 ms"
RATE="$(awk -v b="$WBYTES" -v ms="$WMS" 'BEGIN { printf "%.1f", b / ms / 1000 }')"
FILE_DESC="$(file -b "$TAPECTL_BIN" 2>/dev/null || echo unknown)"
case "$FILE_DESC" in
    *"not stripped"*) BUILD="not stripped: likely a debug build, which caps near 117 MB/s with the CRC (F9)" ;;
    *stripped*) BUILD="stripped: likely a release build" ;;
    *) BUILD="of unknown build ($FILE_DESC)" ;;
esac
RATE_NOTE="$LABEL2's write phase: $WBYTES bytes in $WMS ms, $RATE MB/s (1.6.0's acceptance: $RATE_1_6_0_MBPS MB/s); the binary is $BUILD"
[ "$MHVTL" = 0 ] || RATE_NOTE="$RATE_NOTE (mhvtl: meaningless)"
log "  $RATE_NOTE"
echo "$RATE_NOTE" > "$RUN/RATE.txt"
end_step "$RATE MB/s"

# ------------------------------------------------------------
cp -a "$DB" "$RUN/catalog-at-end.db"
{
    if [ "${#NOT_RUN[@]}" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: PASS, except steps not run: ${NOT_RUN[*]}"; fi
    cat "$RUN/R1.txt"
    echo "$RATE_NOTE"
    echo "R11: $R11_NOTE"
    echo "the cartridge now holds $LABEL5 and belongs to $TAPECTL_HOME"
} > "$RUN/RESULT.txt"
log "RESULT: $(sed -n 1p "$RUN/RESULT.txt"). $RUN"
log "The cartridge holds $LABEL5, in the throwaway home. Before it is a test cartridge again, with"
log "THIS tapectl (an older installed one refuses this home's catalog):"
log "  $TAPECTL_BIN --home $TAPECTL_HOME snapshot mark-reclaimable <unit> --version 1 --force"
log "  for each of $UNIT_A $UNIT_B $UNIT_C ($LABEL5 holds their only copy, so retire is refused until"
log "  then), then $TAPECTL_BIN --home $TAPECTL_HOME volume retire $LABEL5, then"
log "  $TAPECTL_BIN --home $TAPECTL_HOME cartridge erase --device $TAPE_DEV --confirm-label $LABEL5"
[ "$MHVTL" = 1 ] || log "Start the drive-poll timer again: sudo systemctl start tapectl-drive-poll.timer"
[ "${#NOT_RUN[@]}" -eq 0 ] || exit 3
exit 0
