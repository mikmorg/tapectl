#!/usr/bin/env bash
# lifecycle-suite.sh — a scenario runner that simulates years of tapectl use
# and exercises every restore method (renovation follow-up to issue #115).
#
# scripts/mhvtl-verify-gate.sh proves one happy path plus interrupt/resume on
# one tape. It cannot see defects that come from COMMAND ORDERING ACROSS TIME
# — issue #115 (staging before escrow registration seals a tape the escrow
# key cannot decrypt) is exactly that class, found on real LTO-6 hardware on
# 2026-09-10 (docs/lto6-session-journal-2026-09-10.md). This suite is the
# instrument for that class: multi-year archives with evolving sources,
# several volumes, key rotation, tenant reassignment, tape-only marking,
# reclamation, compaction, retirement, cartridge reuse, database loss — and,
# at the end of every scenario, a restore matrix that recovers the data every
# way tapectl and its on-tape scripts allow.
#
# Runs unchanged on the mhvtl virtual library and on a real single-cartridge
# LTO-6 drive (see --single-cartridge / --i-will-lose-the-cartridge below).
#
# Harness idioms (lock, TCTL, check, discovery, config-via-python-heredoc)
# are copied from scripts/mhvtl-verify-gate.sh's shapes — this script does
# NOT source the gate.
#
# Three visible outcomes for every check: PASS, FAIL, SKIP. A SKIP is never
# allowed to look like a PASS — see SKIPPED.txt in the run's workspace and
# the verdict block at the end.
#
# Every side-effecting primitive (TCTL, devcmd, and everything scenarios
# build on top of them) is $DRY_RUN-aware, so a scenario function is the
# SAME function whether this is `--dry-run` or a real run: in dry-run every
# primitive prints "PLAN: <exact command>" and returns success instead of
# acting, so the ordered trace IS the real execution order.
set -uo pipefail

# ---------- defaults ----------
TAPE_DEV="${TAPECTL_GATE_TAPE:-/dev/nst0}"
ERASE_MODE="long"
SINGLE_CARTRIDGE=0
SEED=1
STEPS=40
OUT_DIR="${TAPECTL_LIFECYCLE_OUT:-/scratch/tapectl-lifecycle}"
LOSE_SERIAL=""
DRY_RUN=0
LIST_ONLY=0
SCENARIO=""
RUN_ALL=0

# Registry of scenario name -> one-line description, used by --list and to
# validate --scenario. Parallel arrays (not an assoc array) so iteration
# order matches the documented scenario order.
SCENARIO_NAMES=(
    first-year evolving-source key-rotation tenant-reassign
    tape-only-and-reclaim compaction retire-and-reuse db-loss
    escrow-ordering restore-file-and-catalog quick-archive collection permute
    stale-catalog-sealed-tape cartridge-displacement collection-second-copy
)
SCENARIO_DESCS=(
    "Baseline multi-tenant archive: escrow BEFORE staging, 3 units, 1 volume, full restore matrix"
    "Mutate sources across a year: dirty/diff/supersedable, snapshot v2, restore both versions"
    "Rotate a tenant's key mid-archive; prove old+new+escrow keys all still restore"
    "Move a tenant's units to another tenant; prove both old and new owner restore paths"
    "mark-tape-only preconditions (copies/locations), reclaim+purge a superseded snapshot"
    "Compact an underutilized volume onto a new one; refusal until copies exist elsewhere"
    "Retire a volume, reuse its cartridge, and the ADR-0003 sealed-tape refusal"
    "Recover via db backup/import, DB-less raw-volume + import, and the pure heir path"
    "issue #115 regression: stage-before-escrow refusal, then a working escrow restore"
    "Catalog browsing, single-file restore (incl. unicode/empty/symlink), integrity checks"
    "The one-shot create+stage+write flow"
    "Folder-per-unit collection sync/status/plan/run, including a rename-by-uuid"
    "Seeded random walk exercising the full command surface (--seed/--steps)"
    "issue #208: a sealed tape refuses a write the CATALOG still thinks is allowed"
    "issue #226: ADR-0010 re-init displaces a bound volume and names who lost their last copy"
    "issue #226/#229: the per-copy flow -- one collection run, staging retained, a second volume write"
)

usage() {
    cat <<'USAGE'
usage: lifecycle-suite.sh [--scenario NAME | --all] [--device /dev/nstN]
                           [--erase long|short] [--single-cartridge]
                           [--seed N] [--steps N] [--out DIR]
                           [--i-will-lose-the-cartridge SERIAL]
                           [--dry-run] [--list]

  --scenario NAME                 Run one scenario (see --list for names).
  --all                           Run every scenario in order.
  --device PATH                   Tape device (default: $TAPECTL_GATE_TAPE or /dev/nst0).
  --erase long|short              long = mt rewind+erase (instant on mhvtl, HOURS on
                                   real LTO — never the default on a real drive).
                                   short = mt rewind+weof 1+rewind. This UNSEALS a tape;
                                   it does NOT blank one — a read at BOT still returns
                                   the old File 0's bytes, so `volume init` refuses it as
                                   "present but unparseable" unless --force. Usable only
                                   where --force is in play (--single-cartridge reuse).
                                   A freshly loaded slot tape is always really erased.
  --single-cartridge              Reuse one cartridge for every "next tape" instead of
                                   loading a new slot. Cross-volume checks become SKIP,
                                   visibly.
  --seed N                        Seed for the `permute` scenario and every deterministic
                                   fixture mutation (default 1).
  --steps N                       Step count for the `permute` scenario (default 40).
  --out DIR                       Workspace root (default: /scratch/tapectl-lifecycle).
  --i-will-lose-the-cartridge S   Required on a real (non-mhvtl) drive. S is cross-checked
                                   against sg_read_attr's medium serial number before
                                   anything destructive runs.
  --dry-run                       Print every step this invocation would run, in order,
                                   with exact command lines. Executes NOTHING — no cargo
                                   build, no discovery, no lock. Exits 0.
  --list                          Print scenario names + descriptions and exit 0.
  -h, --help                      This text.
USAGE
}

# ---------- arg parsing (before ANYTHING else, per --dry-run's contract) ----------
while [ $# -gt 0 ]; do
    case "$1" in
        --scenario) SCENARIO="${2:-}"; shift 2 ;;
        --all) RUN_ALL=1; shift ;;
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --erase) ERASE_MODE="${2:-}"; shift 2 ;;
        --single-cartridge) SINGLE_CARTRIDGE=1; shift ;;
        --seed) SEED="${2:-}"; shift 2 ;;
        --steps) STEPS="${2:-}"; shift 2 ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        --i-will-lose-the-cartridge) LOSE_SERIAL="${2:-}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --list) LIST_ONLY=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "lifecycle-suite.sh: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$LIST_ONLY" = 1 ]; then
    for i in "${!SCENARIO_NAMES[@]}"; do
        printf '%-26s %s\n' "${SCENARIO_NAMES[$i]}" "${SCENARIO_DESCS[$i]}"
    done
    exit 0
fi

scenario_known() {
    local want="$1" n
    for n in "${SCENARIO_NAMES[@]}"; do [ "$n" = "$want" ] && return 0; done
    return 1
}

if [ "$RUN_ALL" = 0 ] && [ -z "$SCENARIO" ]; then
    echo "lifecycle-suite.sh: pass --scenario NAME, --all, or --list" >&2
    usage >&2
    exit 2
fi
if [ -n "$SCENARIO" ] && ! scenario_known "$SCENARIO"; then
    echo "lifecycle-suite.sh: unknown scenario \"$SCENARIO\" — see --list" >&2
    exit 2
fi
case "$ERASE_MODE" in long|short) ;; *)
    echo "lifecycle-suite.sh: --erase must be 'long' or 'short', got \"$ERASE_MODE\"" >&2
    exit 2 ;;
esac

die() { echo "LIFECYCLE PRECONDITION FAILED: $*" >&2; exit 2; }

# ---------- mode setup: dry-run gets placeholders, real gets the real thing ----------
# Both branches leave behind the SAME variable set (RUN, HOME_DIR, CFG, SRC,
# BIN, COMMANDS_LOG, SKIPPED_FILE, REPORT, TAPE_DEV, DRIVE_SG, CHG_SG, DTE,
# GEN, LOADED_TAG, MHVTL_DISCOVERY) so every function below is mode-agnostic.
if [ "$DRY_RUN" = 1 ]; then
    CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}"
    BIN="$CARGO_TARGET_DIR/debug/tapectl"
    RUN="$OUT_DIR/DRYRUN"
    HOME_DIR="$RUN/home"
    CFG="$HOME_DIR/config.toml"
    SRC="$RUN/src"
    COMMANDS_LOG="/dev/null"
    SKIPPED_FILE="/dev/null"
    REPORT="/dev/null"
    DRIVE_SG="<DISCOVERED_SG>"; CHG_SG="<DISCOVERED_CHANGER_SG>"
    DTE="<DISCOVERED_DTE>"; GEN="<DISCOVERED_GEN>"; LOADED_TAG="<DISCOVERED_TAPE>"
    DRIVE_MODEL="<DISCOVERED_MODEL>"
    MHVTL_DISCOVERY=1
    STAMP="dryrun"
else
    # ---------- preconditions (loud — see mhvtl-verify-gate.sh's rationale) ----------
    for bin in lsscsi mtx mt dar age sha256sum python3 cargo shellcheck; do
        command -v "$bin" >/dev/null || die "required binary missing: $bin"
    done
    [ -e "$TAPE_DEV" ] || die "$TAPE_DEV missing"

    # Single-drive rule (#9): one tape user at a time, across processes —
    # shared with the gate and lto6-measure.sh so nothing ever contends for
    # the drive.
    exec 9>/tmp/tapectl-tape.lock
    flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock)"

    # ---------- device discovery, with a real-drive fallback (issue #67 idiom) ----------
    # mhvtl-device.sh dies (exit 2) at "no device.conf Drive matches" before
    # any mtx side effect when $TAPE_DEV isn't an mhvtl drive — that failure
    # IS the real-hardware signal, not a precondition to die loudly on.
    MHVTL_DISCOVERY=1
    DISCOVERY="$("$(dirname "$0")/mhvtl-device.sh" --tape "$TAPE_DEV" --ensure-media 2>"/tmp/lifecycle-discovery-err.$$")" \
        || MHVTL_DISCOVERY=0
    DISCOVERY_ERR="$(cat "/tmp/lifecycle-discovery-err.$$" 2>/dev/null || true)"
    rm -f "/tmp/lifecycle-discovery-err.$$"

    if [ "$MHVTL_DISCOVERY" = 1 ]; then
        eval "$DISCOVERY"
        echo "lifecycle-suite: mhvtl drive discovered: $TAPE_DEV ($DRIVE_MODEL, sg=$DRIVE_SG) changer=$CHG_SG dte=$DTE tape=$LOADED_TAG"
    else
        echo "lifecycle-suite: device discovery did not resolve an mhvtl drive for $TAPE_DEV:" >&2
        echo "  $DISCOVERY_ERR" >&2
        echo "lifecycle-suite: treating this as a REAL DRIVE. Real-drive requirements:" >&2
        [ -n "${TAPE_DEV:-}" ] || die "real drive: --device is required"
        [ "$ERASE_MODE" = "short" ] || die "real drive: --erase short is required (never 'long' by default on real LTO)"
        [ "$SINGLE_CARTRIDGE" = 1 ] || die "real drive: --single-cartridge is required"
        [ -n "$LOSE_SERIAL" ] || die "real drive: --i-will-lose-the-cartridge SERIAL is required"

        # Consent check copied in spirit from scripts/lto6-measure.sh
        # (validated on hardware): anchored, label-specific match against
        # sg_read_attr's own "Medium serial number" field — a loose grep
        # matches an unrelated MAM field first and either refuses every
        # legitimate run or, worse, matches the wrong thing and passes.
        ST_BASE="$(basename "$TAPE_DEV")"; ST_BASE="${ST_BASE#n}"
        ROW="$(lsscsi -g | grep -F "/dev/$ST_BASE " | head -1)"
        [ -n "$ROW" ] || die "cannot find $TAPE_DEV in lsscsi -g"
        DRIVE_SG="$(echo "$ROW" | awk '{print $NF}')"
        DRIVE_MODEL="$(echo "$ROW" | awk '{print $3" "$4}')"
        CHG_SG=""; DTE=""; GEN=""; LOADED_TAG="$LOSE_SERIAL"

        command -v sg_read_attr >/dev/null || die "sg_read_attr required to verify the named cartridge"
        MAM_TXT="$(sg_read_attr "$DRIVE_SG" 2>&1)" || die "sg_read_attr failed on $DRIVE_SG"
        SERIAL="$(echo "$MAM_TXT" | grep -iE '^[[:space:]]*medium serial number[[:space:]]*:' \
            | head -1 | sed 's/^[^:]*: *//' | tr -d ' \r')"
        [ -n "$SERIAL" ] || die "sg_read_attr did not report a medium serial number — cannot verify consent"
        [ "$SERIAL" = "$LOSE_SERIAL" ] || die \
            "loaded cartridge reports serial '$SERIAL' but you named '$LOSE_SERIAL' — refusing to touch a cartridge you did not name"
        echo "lifecycle-suite: real drive $TAPE_DEV ($DRIVE_MODEL), cartridge serial '$SERIAL' verified against --i-will-lose-the-cartridge" >&2
    fi

    # ---------- workspace + build ----------
    STAMP="$(date +%Y%m%d-%H%M%S)"
    RUN="$OUT_DIR/run-$STAMP"
    mkdir -p "$RUN" || die "cannot create $RUN"
    echo "lifecycle-suite: workspace $RUN"

    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/scratch/tapectl-target-pm-lifecycle}"
    # The build lock is shared with every other cargo invocation on this VM
    # (worktree-agent.md, "Build lock"): the box is 9 GB, and two concurrent
    # links OOM-kill each other. This script gets its own CARGO_TARGET_DIR
    # above, which keeps cargo's own per-directory lock from serializing it
    # against a worker — but that is exactly what makes the memory collision
    # possible, so the flock is not optional here either.
    #
    # -w/-E, not a bare wait: if a CALLER already wrapped this script in
    # `flock /scratch/tapectl-build.lock`, this line would wait forever on a
    # lock its own ancestor holds — flock locks are per-open-file-description,
    # with no reentrancy for a child. That happened to the mhvtl gate on
    # 2026-09-16 and hung for 13 minutes looking exactly like a slow build.
    # 99 gives the conflict its own exit code so it is never read as a compile
    # failure (cargo exits 101, and a bare -w reports 1, which cargo also uses).
    flock -w 1200 -E 99 /scratch/tapectl-build.lock cargo build --quiet
    build_rc=$?
    if [ "$build_rc" -eq 99 ]; then
        die "timed out waiting for /scratch/tapectl-build.lock.
   If you ran this script inside an outer 'flock /scratch/tapectl-build.lock',
   that is the cause: run it bare — the script takes the lock itself."
    elif [ "$build_rc" -ne 0 ]; then
        die "cargo build failed"
    fi
    BIN="$CARGO_TARGET_DIR/debug/tapectl"
    [ -x "$BIN" ] || die "built binary not found at $BIN"

    HOME_DIR="$RUN/home"; mkdir -p "$HOME_DIR"
    CFG="$HOME_DIR/config.toml"
    SRC="$RUN/src"; mkdir -p "$SRC"

    COMMANDS_LOG="$RUN/commands.log"; : > "$COMMANDS_LOG"
    SKIPPED_FILE="$RUN/SKIPPED.txt"; : > "$SKIPPED_FILE"
    REPORT="$RUN/REPORT.md"; : > "$REPORT"
fi

# ---------- TCTL / devcmd: the two dry-run-safe primitives everything else uses ----------
# TCTL always uses THIS run's isolated home — never ~/.tapectl (guardrail #2).
TCTL() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: tapectl --home $HOME_DIR --config $CFG $*"
        return 0
    fi
    echo "+ tapectl --home $HOME_DIR --config $CFG $*" >>"$COMMANDS_LOG"
    "$BIN" --home "$HOME_DIR" --config "$CFG" "$@" </dev/null
}

# devcmd wraps any real OS-level action (mt, dd, mtx, age, dar, sha256sum,
# chmod, rm, cp, tee) so it is skipped in dry-run and journaled for real.
# Never used for pure inspection the report doesn't need (`[ -L ... ]`,
# `test -f`) — only for actions and destructive/state-changing calls.
devcmd() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: $*"
        return 0
    fi
    echo "+ $*" >>"$COMMANDS_LOG"
    "$@"
}

skip() { # skip <check-name> <reason...>
    local name="$1"; shift
    echo "$name: SKIP — $*" | tee -a "$SKIPPED_FILE"
    return 77
}

# ---------- check harness (PASS / FAIL / SKIP, rc 77 == SKIP) ----------
declare -A RESULT
declare -A NOTE
CHECKS=()
check() { # check <name> <fn> [args...]
    local name="$1"; shift
    if [ "$DRY_RUN" = 1 ]; then
        echo "-- PLAN: check $name --"
        "$@"
        return 0
    fi
    CHECKS+=("$name")
    local rc
    "$@" >"$RUN/log-$name.txt" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
        RESULT[$name]=PASS
    elif [ "$rc" -eq 77 ]; then
        RESULT[$name]=SKIP
        NOTE[$name]="$(tail -1 "$RUN/log-$name.txt" 2>/dev/null | sed 's/^[^:]*: SKIP — //')"
    else
        RESULT[$name]=FAIL
    fi
    echo "  [$name] ${RESULT[$name]}"
}

# ---------- audit_passes: an audit exit code the scenario tolerates ----------
# `audit` exit 0/1 is always fine (clean / advisory warnings). Exit 2 is a
# VIOLATION and normally a failure — EXCEPT where the SCENARIO itself creates a
# state policy is right to complain about. The scenario declares which checks
# those are; this helper never infers them (issue #156).
#
# It used to gate exit 2 on --single-cartridge, which conflated two unrelated
# reasons for a legitimate copy_count violation: cartridge REUSE (one cartridge
# cannot hold two copies) and simply writing ONE volume. `first-year` writes a
# single volume, so it under-copies in EVERY mode — which made
# `--scenario first-year` without --single-cartridge red at fy.audit by
# construction, on any drive (observed 2026-09-13: 44/45, then 45/45 with only
# --single-cartridge added).
#
# $1 = audit exit code, $2 = path to the captured audit --json,
# $3.. = check names whose violations this scenario expects. Exit 2 with no
# declared check is a failure, so silence is never an allowance.
audit_passes() {
    local rc="$1" jf="$2"; shift 2
    { [ "$rc" -eq 0 ] || [ "$rc" -eq 1 ]; } && return 0
    [ "$rc" -eq 2 ] || return 1
    [ "$#" -gt 0 ] || return 1
    # exit 2: pass iff there IS at least one violation and every one of them is
    # a check the caller declared. A declared-but-absent check is not a pass —
    # that would let a scenario silently stop exercising what it claims to.
    python3 - "$jf" "$*" <<'PY2'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
allowed = set(sys.argv[2].split())
findings = d.get("findings") or d.get("results") or []
viols = [f for f in findings if f.get("severity") == "violation"]
sys.exit(0 if viols and all(f.get("check") in allowed for f in viols) else 1)
PY2
}

# ---------- vinit: happy-path volume init that tolerates a reused cartridge ----------
# In single-cartridge reuse, erase_tape (weof at BOT) unseals and truncates the
# cartridge but leaves an unparseable File 0 that `volume init` refuses without
# --force (verified on a real HP LTO-6, 2026-09-10). The reuse consent
# (--single-cartridge, and on a real drive --i-will-lose-the-cartridge)
# authorizes that override. Scenarios that WRITE a volume go through vinit; the
# retire-and-reuse negative checks call `volume init` directly (they test the
# refusal itself and the ADR-0003 sealed-tape rule), so they must not use this.
REUSE_FORCE=""
[ "$SINGLE_CARTRIDGE" = 1 ] && REUSE_FORCE="--force"

# The permutation matrix tolerates copy_count in EVERY mode, and the reasoning
# that said otherwise was wrong in a way worth recording (issue #203, found by
# the first `--all` run, 2026-09-16).
#
# The old comment here read: "reusing a single cartridge under-counts copies,
# while a multi-cartridge run must reach full coverage and any copy_count
# violation there is a real regression." The second half does not hold. A copy
# comes from writing another VOLUME, not from having another CARTRIDGE, and
# `write-next-volume` is drawn at random from pm_generate_sequence's op pool —
# so how many copies exist at step N is decided by the RNG, not by the mode.
# With seed=1 the two writes landed at steps 7 and 8, so steps 2, 3 and 6 had
# one copy and failed; with another seed they could land at step 11 and fail
# ten checks, or at step 1 and fail none. The check was not merely too strict,
# it was SEED-DEPENDENT — a green permute run proved nothing about any other
# seed, which is the worst property a gate can have.
#
# So copy_count cannot be a per-step invariant here. It is allowed per-step and
# the real assertion moved to pm_final_copy_count_is_honest (end of the walk),
# which compares tapectl's count against the volumes this walk actually wrote —
# seed-independent, and a stronger statement than "audit is quiet" ever was.
# Declared here rather than inferred inside audit_passes (issue #156).
PM_ALLOWED_VIOLATIONS=(copy_count)
vinit() { TCTL volume init "$1" --device "$TAPE_DEV" $REUSE_FORCE; }

# ---------- erase_tape: the ONE place scenarios reuse a tape ----------
erase_tape() {
    case "$ERASE_MODE" in
        long)  devcmd mt -f "$TAPE_DEV" rewind && devcmd mt -f "$TAPE_DEV" erase ;;
        short) devcmd mt -f "$TAPE_DEV" rewind && devcmd mt -f "$TAPE_DEV" weof 1 && devcmd mt -f "$TAPE_DEV" rewind ;;
    esac
}

# blank_tape: a REAL erase, whatever --erase says (issue #194).
#
# `--erase short` (weof 1 at BOT) unseals a tape but does not blank one: a read
# at BOT still returns the previous volume's bytes, and `volume init` refuses
# that as "a present but unparseable/corrupt File 0" -- correctly, per #27
# contact discipline and ADR-0003. Measured on mhvtl: after `short` over a
# written tape, init refuses; after a real erase, it initialises.
#
# --single-cartridge never hit this because `vinit` passes $REUSE_FORCE
# (--force) on that branch alone, which overrides the refusal. A freshly loaded
# slot tape has no such cover, and MUST NOT get one: widening --force to this
# path would defeat exactly the check that catches a wrong-cartridge load.
#
# The cost is nil today. Multi-slot loading requires a changer, and a real drive
# is already refused this path (--single-cartridge is mandatory there), so this
# only ever runs on mhvtl, where `mt erase` is instant.
blank_tape() {
    devcmd mt -f "$TAPE_DEV" rewind && devcmd mt -f "$TAPE_DEV" erase
}

# ---------- next_tape: multi-tape on mhvtl, same-cartridge on single ----------
# Tracks which library slots this RUN has already used so a long scenario (or
# --all) doesn't reload the same cartridge and call it a second volume.
USED_SLOTS=""
if [ "$DRY_RUN" = 1 ]; then SLOT_LABEL_MAP="/dev/null"; else SLOT_LABEL_MAP="$RUN/slot-label-map.txt"; : > "$SLOT_LABEL_MAP"; fi

next_tape() { # next_tape <intended-label>
    local label="${1:-}"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: next_tape \"$label\" -> $([ "$SINGLE_CARTRIDGE" = 1 ] && echo "retire \$PREV_LABEL in the DB (physical truth catch-up), then reuse loaded cartridge" || echo "unload current, load next unused $GEN slot")"
        erase_tape
        PREV_LABEL="$label"
        return 0
    fi
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        # Retire the previous volume before its cartridge is reused, so
        # copy-count/mark-tape-only/audit see the truth a real single-cartridge
        # operator lives with instead of crediting a cartridge that no longer
        # physically holds those bytes.
        #
        # This is no longer the catalog's ONLY route to that truth: since
        # ADR-0010, `volume init` binds the cartridge by its MAM serial and
        # RECORDS the displacement — it closes the open mount, marks the
        # displaced volume 'erased' and warns about any unit left without a
        # copy. That happens at the next init, i.e. after the erase below.
        # Retiring here is still the honest order: it makes the catalog
        # truthful at the moment the copy is physically lost, rather than
        # leaving a window in which the DB credits a copy that is already
        # gone. Keep both.
        if [ -n "${PREV_LABEL:-}" ]; then
            TCTL volume retire "$PREV_LABEL" --yes \
                || echo "next_tape: warning — could not retire \"$PREV_LABEL\" before reusing its cartridge (continuing; single-cartridge copy counts may over-credit it)" >&2
        fi
        echo "next_tape: single-cartridge mode — erasing $LOADED_TAG in place for \"$label\""
        erase_tape
        echo "$LOADED_TAG	$label	SAME_CARTRIDGE" >>"$SLOT_LABEL_MAP"
        PREV_LABEL="$label"
        return 0
    fi
    [ "$MHVTL_DISCOVERY" = 1 ] || { echo "next_tape: not an mhvtl drive and not --single-cartridge — nothing this script can safely swap" >&2; return 1; }

    local status slot origin
    status="$(mtx -f "$CHG_SG" status)" || { echo "next_tape: mtx status failed" >&2; return 1; }
    origin="$(echo "$status" | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    if [ -n "$origin" ]; then
        devcmd mtx -f "$CHG_SG" unload "$origin" "$DTE" >&2 || { echo "next_tape: unload failed" >&2; return 1; }
    fi
    # First Full slot of the right generation this run has not used yet.
    slot="$(echo "$status" | grep -E "Storage Element [0-9]+:Full" \
        | grep "VolumeTag=[EF][0-9]*$GEN" \
        | sed -n 's/.*Storage Element \([0-9]*\):Full.*/\1/p' \
        | while read -r s; do
              case " $USED_SLOTS " in *" $s "*) ;; *) echo "$s"; break ;; esac
          done)"
    [ -n "$slot" ] || { echo "next_tape: no unused $GEN slot left in the library for this run" >&2; return 1; }
    devcmd mtx -f "$CHG_SG" load "$slot" "$DTE" >&2 || { echo "next_tape: load $slot $DTE failed" >&2; return 1; }
    USED_SLOTS="$USED_SLOTS $slot"
    LOADED_TAG="$(mtx -f "$CHG_SG" status | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"
    echo "$slot	$label	$LOADED_TAG" >>"$SLOT_LABEL_MAP"
    # A real erase, not erase_tape: this slot tape carries whatever a previous
    # RUN left on it, and there is no --force on this path (issue #194).
    blank_tape
    PREV_LABEL="$label"
}

# load_volume_tape <label> — reload a PREVIOUSLY WRITTEN cartridge (recorded
# earlier by next_tape in $SLOT_LABEL_MAP) WITHOUT erasing it, so a
# scenario can read back an older volume after moving on to a newer one.
# Multi-tape mhvtl only; single-cartridge mode cannot do this (the older
# volume's cartridge was erased to become the newer one) — callers SKIP.
load_volume_tape() {
    local label="$1" slot status origin
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: load_volume_tape $label -> mtx unload current, load the slot recorded for $label"
        return 0
    fi
    [ "$SINGLE_CARTRIDGE" = 1 ] && { echo "load_volume_tape: single-cartridge mode cannot reload a superseded volume"; return 1; }
    [ "$MHVTL_DISCOVERY" = 1 ] || { echo "load_volume_tape: not an mhvtl drive"; return 1; }
    slot="$(awk -F'\t' -v l="$label" '$2==l {print $1}' "$SLOT_LABEL_MAP" | tail -1)"
    [ -n "$slot" ] || { echo "load_volume_tape: no recorded slot for \"$label\" in $SLOT_LABEL_MAP"; return 1; }
    status="$(mtx -f "$CHG_SG" status)" || { echo "load_volume_tape: mtx status failed"; return 1; }
    origin="$(echo "$status" | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    if [ -n "$origin" ]; then
        devcmd mtx -f "$CHG_SG" unload "$origin" "$DTE" || { echo "load_volume_tape: unload failed"; return 1; }
    fi
    devcmd mtx -f "$CHG_SG" load "$slot" "$DTE" || { echo "load_volume_tape: load $slot $DTE failed"; return 1; }
}

# ---------- leak scan: the tape device, BOT to EOD ----------
# This replaced `mhvtl_media_dir()`, which resolved mhvtl's backing
# directory for a scan that was never written (issue #275). Two reasons
# not to revive that shape: the directory is mode 0750 mhvtl:mhvtl and
# this suite runs unprivileged with no sudo anywhere -- which is how the
# GATE's equivalent check came to report PASS for 100+ commits without
# being able to read a byte -- and a real LTO-6 has no media directory at
# all, so a directory scan can never run on the drive that matters.
#
# A zero-length read is a filemark; the st driver then advances past it,
# so the next read starts the next file. Two consecutive empty reads is
# EOD. The 64-file ceiling is a runaway guard, not a layout assumption.
lc_dump_whole_tape() { # lc_dump_whole_tape <outfile>
    local out="$1" tmp="$RUN/.lc-tapefile" got empty=0 n=0
    mt -f "$TAPE_DEV" rewind || { echo "lc_dump_whole_tape: rewind failed"; return 1; }
    : > "$out"
    while [ "$n" -lt 64 ]; do
        dd if="$TAPE_DEV" bs=512k of="$tmp" 2>/dev/null
        got="$(stat -c %s "$tmp" 2>/dev/null)" || return 1
        if [ "$got" -eq 0 ]; then
            empty=$((empty + 1))
            [ "$empty" -ge 2 ] && break
        else
            empty=0
            cat "$tmp" >> "$out"
        fi
        n=$((n + 1))
    done
    rm -f "$tmp"
    [ "$n" -lt 64 ] || { echo "lc_dump_whole_tape: hit the 64-file ceiling without reaching EOD"; return 1; }
    mt -f "$TAPE_DEV" rewind || return 1
    echo "lc_dump_whole_tape: $n file(s), $(stat -c %s "$out") bytes"
}

# Assert the volume just written carries no plaintext canary.
#
# The POSITIVE CONTROL is the point, not the scan: a check that only
# asserts absence cannot distinguish "searched and found nothing" from
# "searched nothing", which is how the gate's version of this check passed
# for 100+ commits while reading nothing at all (issue #275), how
# `csc_fingerprint` passed vacuously (#258), and how `permute`'s restore
# matrix went unrun. `volume-format-v2.md` puts the volume label in the ID
# thunk in plaintext by design, so if the label is not found the SCAN is
# broken and this check must fail as loudly as a real leak.
#
# Only $CANARY is used as a negative needle, deliberately. Unit names
# would be the obvious second needle -- the gate uses "unitA" -- but this
# suite's units are "photos", "docs" and "big", short enough words that a
# plaintext RESTORE.sh or system guide could contain one legitimately and
# turn this into a flaky red. $CANARY is long, unique per run, and planted
# in a real archived file's name AND content by `make_source`.
#
# Call this while the written cartridge is still loaded -- i.e. directly
# after the `volume write` check, before any `next_tape`.
lc_leak_scan() { # lc_leak_scan <label>
    local label="$1" dump="$RUN/leakscan-$1.bin" rc
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: read $TAPE_DEV BOT..EOD and assert no plaintext canary on $label"
        return 0
    fi
    lc_dump_whole_tape "$dump" || return 1

    grep -a -q "label = \"$label\"" "$dump" || {
        echo "lc_leak_scan: volume label $label is NOT in the tape dump -- the scan is broken, not the tape clean"
        return 1
    }

    grep -a -q "$CANARY" "$dump"
    rc=$?
    case "$rc" in
        0) echo "lc_leak_scan: PLAINTEXT LEAK -- the canary appears on $label"; return 1 ;;
        1) return 0 ;;
        *) echo "lc_leak_scan: grep failed (rc=$rc) -- inconclusive, not clean"; return 1 ;;
    esac
}

# ---------- globals shared by fixtures / restore matrix ----------
# CANARY: embedded by `make_source` in one file's NAME and CONTENT, for
# `lc_leak_scan` (issue #275). Until that check existed the canary was
# planted and never looked for by anything -- see lc_leak_scan's comment.
# OPERATOR: the operator tenant name every scenario's `init --operator`
# uses, so the restore-matrix's operator-envelope step knows which key to
# reach for without threading it through every call.
# shellcheck disable=SC2034  # consumed by make_source calls in scenario functions (next commit)
CANARY="CANARY_tapectl_lifecycle_${STAMP}_$$"
OPERATOR="lc-op"

# ============================================================
# Fixtures
# ============================================================

# make_source <dir> <profiles> [canary]
# `profiles` is one or more of plain/big/links/unicode/deep, joined with
# "+" (e.g. "plain+links"). Deterministic content only where content
# matters for a check (canary, unicode name/content) — bulk filler is
# /dev/urandom, which is fine since size/slicing is what those profiles
# exist to exercise, not reproducibility of the bytes themselves.
make_source() {
    local dir="$1" profiles="$2" canary="${3:-}"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: make_source $dir ($profiles)${canary:+, canary embedded}"
        return 0
    fi
    mkdir -p "$dir"
    local IFS_OLD="$IFS" p
    IFS='+'
    for p in $profiles; do
        case "$p" in
            plain)
                echo "plain content" > "$dir/plain.txt"
                head -c 700000 /dev/urandom > "$dir/big-block.bin"
                : > "$dir/empty.bin"
                mkdir -p "$dir/nested"
                echo "nested" > "$dir/nested/déjà-vu.txt"
                ;;
            big)
                head -c 12000000 /dev/urandom > "$dir/twelve-meg.bin"
                ;;
            links)
                echo "target" > "$dir/target.txt"
                ln -sf target.txt "$dir/link-ok"
                ln -sf /nonexistent-lifecycle-path "$dir/link-broken"
                ;;
            unicode)
                printf '\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e content\n' > "$dir/ünïcödé 日本語.txt"
                ;;
            deep)
                mkdir -p "$dir/d1/d2/d3/d4/d5/d6/d7"
                echo "deep leaf" > "$dir/d1/d2/d3/d4/d5/d6/d7/leaf.txt"
                ;;
            *)
                echo "make_source: unknown profile \"$p\"" >&2
                IFS="$IFS_OLD"
                return 1
                ;;
        esac
    done
    IFS="$IFS_OLD"
    [ -n "$canary" ] && echo "$canary payload" > "$dir/${canary}.txt"
    return 0
}

# mutate_source <dir> <seed> <kind>
# kind: add | modify | delete | rename | touch-only. Deterministic from
# <seed> via python's random.Random so a `permute` failure is reproducible.
mutate_source() {
    local dir="$1" seed="$2" kind="$3"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: mutate_source $dir seed=$seed kind=$kind"
        return 0
    fi
    python3 - "$dir" "$seed" "$kind" <<'PY'
import sys, os, random, pathlib

d, seed, kind = sys.argv[1], int(sys.argv[2]), sys.argv[3]
rng = random.Random(seed)
root = pathlib.Path(d)
# `.tapectl-unit.toml` is tapectl's own control file, not user content.
# Mutating it does not simulate source drift — `modify` flips byte 0, turning
# `[unit]` into `\unit]`, and audit then (correctly) reports
# policy_unresolvable as a VIOLATION for that unit for the rest of the run.
# Only the mutating step tolerated it, so every later permute step failed on
# damage the walk itself caused: 19 of 26 failures in the 2026-09-11
# single-cartridge run. Corrupting the dotfile is a real scenario, but a
# deliberate one (issue #59 covers it) — not a side effect of "a file changed".
CONTROL_FILE = ".tapectl-unit.toml"
files = sorted(
    p
    for p in root.rglob("*")
    if p.is_file() and not p.is_symlink() and p.name != CONTROL_FILE
)

if kind == "add":
    name = f"added-{seed}-{rng.randint(0, 999999)}.txt"
    (root / name).write_text("added by mutate_source seed=%d\n" % seed + "x" * rng.randint(10, 200))
    sys.exit(0)

if not files:
    print("mutate_source: no eligible regular file to mutate in %s" % d, file=sys.stderr)
    sys.exit(1)

target = rng.choice(files)
if kind == "modify":
    data = bytearray(target.read_bytes()) or bytearray(b"x")
    data[0] = (data[0] + 1) % 256
    target.write_bytes(bytes(data))
elif kind == "delete":
    target.unlink()
elif kind == "rename":
    target.rename(target.with_name(target.name + ".renamed"))
elif kind == "touch-only":
    st = target.stat()
    new_mtime = st.st_mtime + 3600
    os.utime(target, (new_mtime, new_mtime))
else:
    print("mutate_source: unknown kind %r" % kind, file=sys.stderr)
    sys.exit(2)
PY
}

# ============================================================
# Comparison helpers
# ============================================================

# tree_checksum <dir> — content+symlink-target checksum, journal Phase 4's
# shape: sorted file list, per-entry sha256 (files) or readlink (symlinks),
# then sha256 of that list.
tree_checksum() {
    local dir="$1"
    (
        cd "$dir" || exit 1
        find . \( -type f -o -type l \) | LC_ALL=C sort | while IFS= read -r p; do
            if [ -L "$p" ]; then
                printf 'LINK\t%s\t%s\n' "$p" "$(readlink "$p")"
            else
                printf 'FILE\t%s\t%s\n' "$p" "$(sha256sum "$p" | awk '{print $1}')"
            fi
        done | sha256sum | awk '{print $1}'
    )
}

# assert_identical <src> <restored> — plain `diff -r` FOLLOWS symlinks and
# false-fails (or false-passes) on a dangling one; --no-dereference is
# load-bearing (docs/lto6-session-journal-2026-09-10.md's methodology
# note). Both the diff AND the tree checksum must agree.
assert_identical() {
    local src="$1" restored="$2"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: diff -r --no-dereference $src $restored && tree_checksum comparison"
        return 0
    fi
    if ! diff -r --no-dereference "$src" "$restored"; then
        echo "assert_identical: diff -r --no-dereference disagreed"
        return 1
    fi
    local h1 h2
    h1="$(tree_checksum "$src")"
    h2="$(tree_checksum "$restored")"
    if [ "$h1" != "$h2" ]; then
        echo "assert_identical: tree checksum mismatch: $h1 (src) != $h2 (restored)"
        return 1
    fi
    echo "identical: $src == $restored (tree checksum $h1)"
}

# ============================================================
# Restore matrix
# ============================================================

# active_key_path <tenant> <key_type: primary|backup> — resolves the
# CURRENTLY ACTIVE key file for a tenant via `key list --json`, not a
# hardcoded "<tenant>-primary.age.key" guess. Matters because `key rotate`
# (src/cli/key.rs) never reuses that filename — it mints
# "<tenant>-rotated-primary-<seq>.age.key" and leaves the old file on disk,
# now inactive. Falls back to the conventional "<tenant>-<key_type>"
# filename if `key list` can't be read (e.g. tenant renamed/reassigned
# mid-scenario and the lookup needs a retry the caller controls).
active_key_path() {
    local tenant="$1" ktype="$2" alias
    if [ "$DRY_RUN" = 1 ]; then
        echo "$HOME_DIR/keys/$tenant-$ktype.age.key"
        return 0
    fi
    alias="$(TCTL key list --tenant "$tenant" --json 2>/dev/null | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    d = []
for k in d:
    if k.get('key_type') == '$ktype' and k.get('is_active') and not k.get('is_escrow'):
        print(k.get('alias'))
        break
" 2>/dev/null)"
    if [ -n "$alias" ] && [ -f "$HOME_DIR/keys/$alias.age.key" ]; then
        echo "$HOME_DIR/keys/$alias.age.key"
    else
        echo "$HOME_DIR/keys/$tenant-$ktype.age.key"
    fi
}

# ensure_heir_restore_sh [workdir] — dd RESTORE.sh off the CURRENTLY LOADED
# tape into <workdir>/heir, once (self-healing if an earlier step failed to
# extract it). Defaults to $RM_WORK so restore_matrix's steps can keep
# calling it with no argument; scenarios that need the heir script for a
# tape outside a restore_matrix call (e.g. key-rotation reloading VOL-A)
# pass their own workdir explicitly.
ensure_heir_restore_sh() {
    local work="${1:-$RM_WORK}"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: mt rewind; setblk 524288; fsf 2; dd if=$TAPE_DEV bs=512k | tr -d '\\0' > RESTORE.sh; chmod +x; bash -n"
        return 0
    fi
    [ -f "$work/heir/RESTORE.sh" ] && return 0
    mkdir -p "$work/heir"
    devcmd mt -f "$TAPE_DEV" rewind || return 1
    devcmd mt -f "$TAPE_DEV" setblk 524288 || return 1
    devcmd mt -f "$TAPE_DEV" fsf 2 || return 1
    dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' > "$work/heir/RESTORE.sh"
    chmod +x "$work/heir/RESTORE.sh"
    bash -n "$work/heir/RESTORE.sh"
}

rm_step_unit() {
    local to="$RM_WORK/unit"
    TCTL restore unit --unit "$RM_UNIT" --from "$RM_LABEL" --to "$to" --device "$TAPE_DEV" || return 1
    [ "$DRY_RUN" = 1 ] && return 0
    assert_identical "$RM_SRC" "$to"
}

rm_step_file() {
    local to="$RM_WORK/file" relpath
    if [ "$DRY_RUN" = 1 ]; then
        TCTL restore file --file "<nested-file-in-$RM_UNIT>" --unit "$RM_UNIT" --from "$RM_LABEL" --to "$to" --device "$TAPE_DEV"
        return 0
    fi
    relpath="$(cd "$RM_SRC" && find . -type f | LC_ALL=C sort | tail -1 | sed 's#^\./##')"
    [ -n "$relpath" ] || { echo "no regular file found under $RM_SRC"; return 1; }
    TCTL restore file --file "$relpath" --unit "$RM_UNIT" --from "$RM_LABEL" --to "$to" --device "$TAPE_DEV" || return 1
    local base expect actual
    base="$(basename "$relpath")"
    expect="$(sha256sum "$RM_SRC/$relpath" | awk '{print $1}')"
    actual="$(sha256sum "$to/$base" 2>/dev/null | awk '{print $1}')"
    [ -n "$actual" ] && [ "$expect" = "$actual" ] || {
        echo "restore file: sha256 mismatch or missing ($to/$base)"; return 1;
    }
}

rm_step_restore_sh_dd() {
    ensure_heir_restore_sh || return 1
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --info (expect 'Verdict: SEALED'); ./RESTORE.sh --verify (expect 'VERIFY: PASS')"
        return 0
    fi
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --info) >"$RM_WORK/info.txt" 2>&1
    grep -q "Verdict: SEALED" "$RM_WORK/info.txt" || { cat "$RM_WORK/info.txt"; echo "expected Verdict: SEALED"; return 1; }
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --verify) >"$RM_WORK/verify_sh.txt" 2>&1
    grep -q "VERIFY: PASS" "$RM_WORK/verify_sh.txt" || { cat "$RM_WORK/verify_sh.txt"; echo "expected VERIFY: PASS"; return 1; }
}

# heir_key_candidates <tenant> <key_type> — every key FILE of that type this
# operator holds, active first, then the deactivated ones newest-first.
#
# Issue #288(b). `active_key_path` alone is WRONG for a volume written before a
# `key rotate`: a tenant envelope is sealed at WRITE time with the recipients
# the tenant had then (src/volume/build.rs:290), so the key active NOW cannot
# open a tape written before it existed. tapectl's own restore never trips on
# this because it trial-decrypts with every tenant and operator key; the heir
# path takes a single --key, so the harness must model what an heir actually
# does -- reach for the Heir Kit and try the keys they hold.
#
# This is emphatically NOT "pass if any key works": see heir_restore_try_keys,
# which still fails when NO held key opens the envelope. Trying the operator's
# own keyring is the heir's real situation; having no key that works is a real
# failure and stays RED.
heir_key_candidates() { # <tenant> <key_type>
    local tenant="$1" ktype="$2" aliases
    aliases="$(TCTL key list --tenant "$tenant" --json 2>/dev/null | KT="$ktype" python3 -c '
import json, os, sys
try:
    d = json.load(sys.stdin)
except Exception:
    raise SystemExit
kt = os.environ["KT"]
keys = [k for k in d if k.get("key_type") == kt and not k.get("is_escrow")]
active   = [k for k in keys if k.get("is_active")]
inactive = [k for k in keys if not k.get("is_active")]
# Ordering is a preference, not a correctness requirement — every candidate is
# tried. `key list` is chronological, so reversing puts the most recently
# deactivated (most likely to match a recent tape) first.
for k in active + list(reversed(inactive)):
    a = k.get("alias")
    if a:
        print(a)
' 2>/dev/null)"
    local a found=0
    while IFS= read -r a; do
        [ -n "$a" ] || continue
        if [ -f "$HOME_DIR/keys/$a.age.key" ]; then
            echo "$HOME_DIR/keys/$a.age.key"; found=1
        fi
    done <<<"$aliases"
    # Fall back to the conventional filename only when `key list` told us
    # nothing — same reasoning as active_key_path's own fallback.
    if [ "$found" -eq 0 ] && [ -f "$HOME_DIR/keys/$tenant-$ktype.age.key" ]; then
        echo "$HOME_DIR/keys/$tenant-$ktype.age.key"
    fi
}

# heir_restore_try_keys <key_type> <dest> <logfile> — run the heir RESTORE.sh
# against each key the tenant holds, stopping at the first that opens the
# envelope, and SAY WHICH ONE DID. Fails if none does.
heir_restore_try_keys() { # <key_type> <dest> <logfile>
    local ktype="$1" to="$2" log="$3"
    local keys tried=0 k
    keys="$(heir_key_candidates "$RM_TENANT" "$ktype")"
    [ -n "$keys" ] || { echo "no $ktype key of tenant $RM_TENANT on disk at all"; return 1; }
    : >"$log"
    while IFS= read -r k; do
        [ -n "$k" ] || continue
        tried=$((tried + 1))
        if (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore \
                --unit "$RM_UNIT" --key "$k" --to "$to") >>"$log" 2>&1; then
            echo "heir restore of $RM_UNIT opened with $ktype key $(basename "$k") (candidate $tried of $(printf '%s\n' "$keys" | grep -c .))"
            return 0
        fi
        echo "--- $ktype candidate $(basename "$k") did not open the envelope ---" >>"$log"
        rm -rf "$to"
    done <<<"$keys"
    cat "$log"
    echo "no $ktype key held by tenant $RM_TENANT opened $RM_UNIT's envelope ($tried tried)"
    return 1
}

rm_step_restore_sh_primary() {
    ensure_heir_restore_sh || return 1
    local to="$RM_WORK/primary"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit $RM_UNIT --key <each primary key of $RM_TENANT, active first> --to $to"
        return 0
    fi
    heir_restore_try_keys primary "$to" "$RM_WORK/restore_primary.txt" || return 1
    assert_identical "$RM_SRC" "$to"
}

rm_step_restore_sh_backup() {
    ensure_heir_restore_sh || return 1
    local to="$RM_WORK/backup"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit $RM_UNIT --key <each backup key of $RM_TENANT, active first> --to $to (proves the backup key is a real recipient)"
        return 0
    fi
    heir_restore_try_keys backup "$to" "$RM_WORK/restore_backup.txt" || return 1
    assert_identical "$RM_SRC" "$to"
}

# Decided from src/volume/layout.rs:516 (envelope_positions lists
# tenant_envelope, operator_envelope AND operator_envelope_backup as equal
# trial-decrypt candidates) and :883 ("multiple units found" only fires when
# a single envelope covers >1 unit) — the operator envelope IS a normal
# restore path, requiring --unit exactly like a tenant envelope covering
# more than one unit. Not a refusal-by-design case.
rm_step_operator_envelope() {
    ensure_heir_restore_sh || return 1
    local opkey to="$RM_WORK/operator"
    opkey="$(active_key_path "$OPERATOR" primary)"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --find-envelope --key $opkey; ./RESTORE.sh --restore --unit $RM_UNIT --key $opkey --to $to"
        return 0
    fi
    [ -f "$opkey" ] || { echo "operator key missing: $opkey"; return 1; }
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --find-envelope --key "$opkey") \
        >"$RM_WORK/find_operator.txt" 2>&1 || { cat "$RM_WORK/find_operator.txt"; return 1; }
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit "$RM_UNIT" --key "$opkey" --to "$to") \
        >"$RM_WORK/restore_operator.txt" 2>&1 || { cat "$RM_WORK/restore_operator.txt"; return 1; }
    assert_identical "$RM_SRC" "$to"
}

rm_step_escrow() {
    ensure_heir_restore_sh || return 1
    local to="$RM_WORK/escrow"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit $RM_UNIT --key \$ESCROW_KEY_PATH --to $to"
        return 0
    fi
    [ -f "$ESCROW_KEY_PATH" ] || { echo "no escrow key captured for this scenario ($ESCROW_KEY_PATH) — was key generate --escrow run?"; return 1; }
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit "$RM_UNIT" --key "$ESCROW_KEY_PATH" --to "$to") \
        >"$RM_WORK/restore_escrow.txt" 2>&1 || { cat "$RM_WORK/restore_escrow.txt"; return 1; }
    assert_identical "$RM_SRC" "$to"
}

rm_step_raw_volume() {
    local to="$RM_WORK/raw"
    if [ "$DRY_RUN" = 1 ]; then
        TCTL restore raw-volume --to "$to" --device "$TAPE_DEV" --json
        echo "PLAN: assert mismatched_count == 0 and all_verified from the JSON above"
        return 0
    fi
    TCTL restore raw-volume --to "$to" --device "$TAPE_DEV" --json >"$RM_WORK/raw.json" 2>"$RM_WORK/raw.json.err"
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("mismatched_count", 1) == 0 and d.get("all_verified", False), d
' "$RM_WORK/raw.json" || { cat "$RM_WORK/raw.json"; return 1; }
}

# Cross-tenant negative. Primary evidence (mirrors mhvtl-verify-gate.sh's
# step_crosskey): the other tenant's key must NOT age-decrypt a slice
# belonging to RM_UNIT, resolved via the catalog. RM_OTHER is set by
# restore_matrix's caller (or auto-detected); no other tenant on this
# volume is a legitimate SKIP, not a failure.
rm_step_isolation() {
    local other="$RM_OTHER"
    if [ -z "$other" ]; then
        skip "$RM_TAG.isolation" "no other tenant registered on this archive besides $RM_TENANT/$OPERATOR — nothing to cross-test"
        return $?
    fi
    local otherkey; otherkey="$(active_key_path "$other" primary)"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: resolve a $RM_UNIT slice via the catalog; age -d -i $otherkey <slice> (must fail)"
        return 0
    fi
    [ -f "$otherkey" ] || { echo "other tenant key missing: $otherkey"; return 1; }
    local slice
    slice="$(python3 - "$HOME_DIR/tapectl.db" "$RM_UNIT" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    """SELECT sl.staging_path FROM stage_slices sl
       JOIN stage_sets ss ON ss.id = sl.stage_set_id
       JOIN snapshots s ON s.id = ss.snapshot_id
       JOIN units u ON u.id = s.unit_id
       WHERE u.name = ? AND sl.staging_path IS NOT NULL
       -- NEWEST stage set first. Ordering by slice_number alone could pick a
       -- slice staged under a PREVIOUS owner (before `tenant reassign`), which
       -- the previous owner's key legitimately still decrypts -- sealed media
       -- cannot be retroactively re-encrypted. Isolation means the CURRENT
       -- owner's data is not readable by another tenant; historical slices are
       -- a separate question (see #131).
       ORDER BY ss.id DESC, sl.slice_number LIMIT 1""",
    (sys.argv[2],),
).fetchone()
print(row[0] if row else "")
PY
)"
    if [ -z "$slice" ] || [ ! -f "$slice" ]; then
        skip "$RM_TAG.isolation" "no live staged slice for $RM_UNIT (already released by staging clean) — crypto isolation already proved by other tags in this run"
        return $?
    fi
    if age -d -i "$otherkey" "$slice" >/dev/null 2>&1; then
        echo "$other's key decrypted $RM_TENANT's ($RM_UNIT) slice — isolation broken"
        return 1
    fi
}

rm_step_verify() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: tapectl volume verify $RM_LABEL --full/--quick --device $TAPE_DEV --json; tapectl report verify-status --json (assert $RM_LABEL listed)"
        return 0
    fi
    TCTL volume verify "$RM_LABEL" --full --device "$TAPE_DEV" --json >"$RM_WORK/verify_full.json" 2>"$RM_WORK/verify_full.json.err" || { cat "$RM_WORK/verify_full.json.err" "$RM_WORK/verify_full.json"; return 1; }
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0 and d.get("passed",0)>0, d' "$RM_WORK/verify_full.json" || return 1
    TCTL volume verify "$RM_LABEL" --quick --device "$TAPE_DEV" --json >"$RM_WORK/verify_quick.json" 2>"$RM_WORK/verify_quick.json.err" || { cat "$RM_WORK/verify_quick.json.err" "$RM_WORK/verify_quick.json"; return 1; }
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0, d' "$RM_WORK/verify_quick.json" || return 1
    TCTL report verify-status --json >"$RM_WORK/verify_status.json" 2>"$RM_WORK/verify_status.json.err" || return 1
    grep -q "\"$RM_LABEL\"" "$RM_WORK/verify_status.json" || { echo "volume $RM_LABEL not listed in report verify-status"; return 1; }
}

# restore_matrix <label> <unit> <tenant> <expected_src_dir> <tag> [other_tenant]
# Runs all 10 methods as separate `check`s named "<tag>.<method>". Call
# after a volume is sealed and while its cartridge is (or can be) reloaded.
# restore_matrix <label> <unit> <tenant> <src> <tag> [other-tenant]
#
# Deliberately has NO "skip this whole matrix" escape hatch. One was added while
# closing #128, on the premise that quick-archive, collection, tenant-reassign
# and key-rotation had matrices a single cartridge could not satisfy. Every one
# of those turned out to be a real bug — a missing escrow step, a missing
# `volume init`, and #131 — so the hatch was removed. If a matrix fails here,
# find out why before deciding the media is at fault.
restore_matrix() {
    RM_LABEL="$1"; RM_UNIT="$2"; RM_TENANT="$3"; RM_SRC="$4"; RM_TAG="$5"; RM_OTHER="${6:-}"
    if [ "$DRY_RUN" != 1 ]; then
        RM_WORK="$RUN/matrix-$RM_TAG"; mkdir -p "$RM_WORK"
        if [ -z "$RM_OTHER" ] && [ -d "$HOME_DIR/keys" ]; then
            RM_OTHER="$(find "$HOME_DIR/keys" -maxdepth 1 -name '*-primary.age.key' -printf '%f\n' 2>/dev/null \
                | sed 's/-primary\.age\.key$//' | grep -vx "$RM_TENANT" | grep -vx "$OPERATOR" | head -1 || true)"
        fi
    else
        RM_WORK="$RUN/matrix-$RM_TAG"
    fi

    check "$RM_TAG.unit"               rm_step_unit
    check "$RM_TAG.file"               rm_step_file
    check "$RM_TAG.restore_sh_dd"      rm_step_restore_sh_dd
    check "$RM_TAG.restore_sh_primary" rm_step_restore_sh_primary
    check "$RM_TAG.restore_sh_backup"  rm_step_restore_sh_backup
    check "$RM_TAG.operator_envelope"  rm_step_operator_envelope
    check "$RM_TAG.escrow"             rm_step_escrow
    check "$RM_TAG.raw_volume"         rm_step_raw_volume
    check "$RM_TAG.isolation"          rm_step_isolation
    check "$RM_TAG.verify"             rm_step_verify
}

# ============================================================
# Shared scenario helpers
# ============================================================

# bootstrap_config — `tapectl init` plus the same config.toml hand-edits
# every scenario needs: dar resolved via PATH (never hardcode a path —
# CLAUDE.md), a staging directory under this scenario's own tree, the
# discovered/consented device wired into a [[backends.lto]] entry, and
# `slice_size = "1M"` so the `big` fixture profile (12 MB) naturally spans
# multiple slices without a separate archive-set. `compaction.
# utilization_threshold` is raised for every scenario (harmless — only the
# `compaction` scenario ever calls `volume compact*`) so that scenario
# doesn't need its own config pass. `min_copies_for_tape_only` /
# `min_locations_for_tape_only` are already 2/2 in a fresh `init`, matching
# what `tape-only-and-reclaim` needs — no override required (verified via
# `tapectl init` in an isolated home).
bootstrap_config() {
    TCTL init --operator "$OPERATOR" --no-escrow || return 1
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: python3 rewrites $CFG (dar binary=dar, slice_size=1M, staging dir, backends.lto entry for $TAPE_DEV, compaction.utilization_threshold=0.95)"
        return 0
    fi
    local scenario_dir staging_dir
    scenario_dir="$(dirname "$HOME_DIR")"
    staging_dir="$scenario_dir/staging"
    mkdir -p "$staging_dir"
    python3 - "$CFG" "$staging_dir" "$TAPE_DEV" "$DRIVE_SG" "$SINGLE_CARTRIDGE" <<'PY'
import re
import sys

cfg, staging, tape, sg, single = sys.argv[1:6]
_ = single  # kept for signature stability; copy policy is handled in audit_passes()
t = open(cfg).read()
t = re.sub(r'(?m)^binary *=.*$', 'binary = "dar"', t, count=1)
t = re.sub(r'(?m)^slice_size *=.*$', 'slice_size = "1M"', t, count=1)
t = re.sub(r'(?m)^directory *=.*$', f'directory = "{staging}"', t, count=1)
t = re.sub(r'(?m)^utilization_threshold *=.*$', 'utilization_threshold = 0.95', t, count=1)
# Must match an UNCOMMENTED table header: `tapectl init` now writes a
# commented-out [[backends.lto]] example (#124b), and a plain substring test
# sees that and concludes a backend is already configured.
if not re.search(r"(?m)^\[\[backends\.lto\]\]", t):
    t = re.sub(r'(?m)^lto *= *\[\] *\n', "", t)
    t += f'''
[[backends.lto]]
name = "lifecycle"
device_tape = "{tape}"
device_sg = "{sg}"
generation = "LTO-8"
# 2 748 779 069 440 = 2.5 TiB exactly, written as a bare byte count on
# purpose (issue #200). This string used to be "2.5T", parsed BINARY by
# volume init and DECIMAL by config validation; #200 made init decimal
# too, which would have shrunk this microcosm by 9.95%. Fixing a parser
# drift and resizing the test microcosm are two different changes, and
# doing both at once means a red gate cannot be attributed to either.
# A bare integer is the one literal both parsers read identically.
capacity_override = "2748779069440"
usable_capacity_factor = 0.95
enospc_buffer = "2G"
'''
open(cfg, "w").write(t)
PY
}

# capture_escrow_secret <check-name> — pulls the AGE-SECRET-KEY-1... line
# `key generate --escrow` printed into that check's log (never re-printed;
# ADR-0005 shows it exactly once) into $ESCROW_KEY_PATH (mode 600), and
# locks the log down to 600 too so the secret doesn't sit world-readable —
# it must never reach REPORT.md, only the public key does.
capture_escrow_secret() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: parse AGE-SECRET-KEY-1... from log-$1.txt into \$ESCROW_KEY_PATH (mode 600); record only the public key"
        return 0
    fi
    local log="$RUN/log-$1.txt" secret pub
    [ -f "$log" ] || return 0
    secret="$(grep -oE 'AGE-SECRET-KEY-1[A-Z0-9]+' "$log" | head -1)"
    pub="$(grep -oE 'age1[a-z0-9]+' "$log" | head -1)"
    if [ -n "$secret" ]; then
        printf '%s\n' "$secret" >"$ESCROW_KEY_PATH"
        chmod 600 "$ESCROW_KEY_PATH"
        [ -n "$pub" ] && echo "$pub" >"$(dirname "$ESCROW_KEY_PATH")/escrow.pub"
    fi
    chmod 600 "$log" 2>/dev/null || true
}

# NEWHOME_TCTL <home> [args...] — like TCTL, but for a SECOND, throwaway
# home a scenario stands up alongside its own $HOME_DIR (db-loss's three
# "what if the database/home is gone" arms). Never touches $HOME_DIR/$CFG.
NEWHOME_TCTL() {
    local home="$1"; shift
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: tapectl --home $home --config $home/config.toml $*"
        return 0
    fi
    echo "+ tapectl --home $home --config $home/config.toml $*" >>"$COMMANDS_LOG"
    "$BIN" --home "$home" --config "$home/config.toml" "$@" </dev/null
}

# pending_count — number of pending (staged, unwritten) stage sets, via
# `report pending --json` (a plain JSON array — src/cli/report.rs
# report_pending) rather than sqlite3 directly (guardrail: no direct DB
# access except the one documented read-only exception this isn't).
pending_count() {
    if [ "$DRY_RUN" = 1 ]; then echo 0; return 0; fi
    TCTL report pending --json 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin)
print(len(d) if isinstance(d, list) else 0)
' 2>/dev/null || echo 0
}

# json_field <json-file> <python-expr-on-d> — small helper so scenario
# checks don't hand-roll a python3 heredoc for every single field read.
# <python-expr-on-d> is evaluated with `d` bound to the parsed JSON.
json_field() {
    python3 -c "
import json, sys
d = json.load(open(sys.argv[1]))
print($2)
" "$1" 2>/dev/null
}

# bootstrap_archive_v1 [label=VOL-A] — the "after first-year" state several
# scenarios need (evolving-source, key-rotation, tenant-reassign, tape-
# only-and-reclaim, compaction, retire-and-reuse, db-loss): two tenants,
# three units (photos/alice, docs/bob, big/alice), escrow before staging,
# snapshot+stage v1, one volume written and moved to vault, cartridge
# registered. The label is parameterized so compaction can call it as
# "VOL-E" per the task spec's naming. Runs as ONE `check` (the caller names
# it) rather than first-year's own per-step checks, so scenarios that build
# on it don't re-report first-year's checks under a different scenario's
# report section.
# shellcheck disable=SC2120  # called with an explicit label via `check cp.setup bootstrap_archive_v1 VOL-E`
bootstrap_archive_v1() {
    local label="${1:-VOL-A}"
    bootstrap_config || return 1
    TCTL location add vault --description "Home vault" || return 1
    TCTL location add offsite --description "Offsite shelf" || return 1
    TCTL tenant add alice || return 1
    TCTL tenant add bob || return 1
    if [ "$DRY_RUN" = 1 ]; then
        TCTL key generate --escrow
    else
        TCTL key generate --escrow >"$RUN/log-_bootstrap_escrow.txt" 2>&1 || return 1
        capture_escrow_secret _bootstrap_escrow
    fi
    make_source "$SRC/photos" "plain+links" "$CANARY" || return 1
    make_source "$SRC/docs" "unicode+deep" || return 1
    make_source "$SRC/big" "big" || return 1
    TCTL unit init "$SRC/photos" --tenant alice --name photos || return 1
    TCTL unit init "$SRC/docs" --tenant bob --name docs || return 1
    TCTL unit init "$SRC/big" --tenant alice --name big || return 1
    TCTL snapshot create photos || return 1
    TCTL snapshot create docs || return 1
    TCTL snapshot create big || return 1
    TCTL stage create photos || return 1
    TCTL stage create docs || return 1
    TCTL stage create big || return 1
    next_tape "$label" || return 1
    vinit "$label" || return 1
    TCTL volume write "$label" --device "$TAPE_DEV" || return 1
    TCTL volume move "$label" --to vault || return 1
    # No `cartridge register` here: since ADR-0010, `volume init` reads the
    # medium serial from MAM and registers and binds the cartridge itself.
    # Doing it by hand created a SECOND row (the mtx VolumeTag is not the MAM
    # serial) and had to guess a generation - wrong on mhvtl, whose media is
    # LTO-8, and wrong again on any drive fed another generation.
}

# bootstrap_two_volumes — bootstrap_archive_v1 plus a second, mutated
# version of every unit written to VOL-B and moved to offsite. Gives every
# unit 2 sealed copies in 2 distinct locations (vault, offsite) — what
# tape-only-and-reclaim and compaction both need as a starting point.
bootstrap_two_volumes() {
    bootstrap_archive_v1 || return 1
    mutate_source "$SRC/photos" "$SEED" modify || return 1
    mutate_source "$SRC/docs" "$SEED" add || return 1
    mutate_source "$SRC/big" "$SEED" touch-only || return 1
    TCTL snapshot create photos || return 1
    TCTL snapshot create docs || return 1
    TCTL snapshot create big || return 1
    TCTL stage create photos || return 1
    TCTL stage create docs || return 1
    TCTL stage create big || return 1
    next_tape VOL-B || return 1
    vinit VOL-B || return 1
    TCTL volume write VOL-B --device "$TAPE_DEV" || return 1
    TCTL volume move VOL-B --to offsite || return 1
}

# ---------- scenario stubs ----------
# Each is replaced with a real implementation in a later commit. Kept as
# real (if minimal) functions from the start so --list/--dry-run/--all can
# already enumerate and plan every scenario name.
# ============================================================
# Scenario: first-year
# ============================================================
# Baseline every later scenario builds on: escrow BEFORE any staging (the
# correct order — issue #115's escrow-ordering scenario tests what happens
# when it's violated), three units across two tenants, one volume, moved to
# a shelf location, cartridge registered, full restore matrix on all three
# units.
fy_init()      { bootstrap_config; }
fy_locations() { TCTL location add vault --description "Home vault" && TCTL location add offsite --description "Offsite shelf"; }
fy_tenants()   { TCTL tenant add alice && TCTL tenant add bob; }
fy_escrow()    { TCTL key generate --escrow; }
fy_units() {
    make_source "$SRC/photos" "plain+links" "$CANARY" || return 1
    make_source "$SRC/docs" "unicode+deep" || return 1
    make_source "$SRC/big" "big" || return 1
    TCTL unit init "$SRC/photos" --tenant alice --name photos \
    && TCTL unit init "$SRC/docs" --tenant bob --name docs \
    && TCTL unit init "$SRC/big" --tenant alice --name big
}
fy_snapshots() { TCTL snapshot create photos && TCTL snapshot create docs && TCTL snapshot create big; }
fy_stage()     { TCTL stage create photos && TCTL stage create docs && TCTL stage create big; }
fy_pending_is_three() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl report pending --json (assert 3 entries)"; return 0; }
    local n; n="$(pending_count)"
    [ "$n" = 3 ] || { echo "expected 3 pending stage sets, got $n"; return 1; }
}
fy_plan()      { TCTL volume plan; }
fy_write() {
    next_tape VOL-A || return 1
    vinit VOL-A \
    && TCTL volume write VOL-A --device "$TAPE_DEV"
}
fy_move()      { TCTL volume move VOL-A --to vault; }
# ADR-0010 turned this step inside out: the operator no longer registers the
# cartridge, `volume init` does it from the medium serial. The step now
# ASSERTS the binding rather than performing it.
fy_cartridge() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: assert volume init auto-registered and bound VOL-A's cartridge (ADR-0010)"; return 0; }
    local out
    out="$(python3 - "$HOME_DIR/tapectl.db" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT c.barcode, c.media_type, c.status FROM cartridges c "
    "JOIN cartridge_volumes cv ON cv.cartridge_id = c.id "
    "JOIN volumes v ON v.id = cv.volume_id "
    "WHERE v.label = 'VOL-A' AND cv.unmounted_at IS NULL").fetchone()
print("|".join(map(str, row)) if row else "")
PY
)"
    [ -n "$out" ] || { echo "fy_cartridge: no cartridge bound to VOL-A - volume init did not bind (ADR-0010)"; return 1; }
    echo "fy_cartridge: VOL-A bound to cartridge $out (barcode|generation|status)"
    case "$out" in *"|in_use") ;; *) echo "fy_cartridge: bound cartridge is not in_use"; return 1 ;; esac
}
fy_audit() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl audit --json (record exit code; 0 or 1 both PASS)"; return 0; }
    local fy_audit_json="$RUN/fy.audit.json"
    TCTL audit --json >"$fy_audit_json" 2>"$fy_audit_json.err"
    local rc=$?
    # first-year writes exactly one volume (fy.write), so every unit is
    # legitimately one copy short of min_copies in EVERY cartridge mode. That
    # is the scenario's own doing, not a defect, and not a reuse artefact.
    audit_passes "$rc" "$fy_audit_json" copy_count || {
        echo "audit exited $rc with violations other than copy_count:"; cat "$fy_audit_json"; return 1; }
    return 0
}
fy_fsck()      { TCTL db fsck; }
fy_summary()   { TCTL report summary; }

# Issue #270: proves the --json captures in this file keep stdout CLEAN.
#
# Every `--json` capture here used to be `>"$f" 2>&1`, merging stderr into the
# file the next line parses. tapectl writes progress and diagnostics to stderr
# ON PURPOSE so `--json` stdout stays parseable, so those captures were one
# warning away from a JSONDecodeError that reads like a product bug. Three
# instances of that trap had already been fixed one at a time (#226, #265's
# run_capture_json, then these); this pins the property instead.
#
# `--verbose` is the lever because it is deterministic: it guarantees DEBUG
# lines on stderr for any command, so the check cannot pass by the command
# happening to be quiet. That matters -- a version of this assertion that
# merely parsed a clean capture would be vacuous, which is the exact failure
# mode this whole class is made of.
fy_json_capture_keeps_stdout_clean() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl --verbose report copies --json, split streams, assert stdout parses and stderr is non-empty"; return 0; }
    local d="$RUN/json-split"; mkdir -p "$d"
    TCTL --verbose report copies --json >"$d/out.json" 2>"$d/out.err" || {
        cat "$d/out.err" "$d/out.json"; return 1
    }
    [ -s "$d/out.err" ] || {
        echo "--verbose produced no stderr, so this check proves nothing about stream separation (issue #270)"
        return 1
    }
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$d/out.json" || {
        echo "stdout did not parse as JSON even with the streams split -- the capture is still contaminated:"
        head -c 400 "$d/out.json"; return 1
    }
    grep -q "DEBUG" "$d/out.json" && {
        echo "a DEBUG line reached the JSON file: stderr is still being merged into stdout (issue #270)"
        return 1
    }
    echo "stdout parsed as JSON while stderr carried $(wc -c <"$d/out.err") bytes of diagnostics"
    return 0
}

scenario_first_year() {
    check fy.init       fy_init
    check fy.json_split fy_json_capture_keeps_stdout_clean
    check fy.locations  fy_locations
    check fy.tenants    fy_tenants
    check fy.escrow     fy_escrow
    capture_escrow_secret fy.escrow
    check fy.units      fy_units
    check fy.snapshots  fy_snapshots
    check fy.stage      fy_stage
    check fy.pending    fy_pending_is_three
    check fy.plan       fy_plan
    check fy.write      fy_write
    # Directly after the write, while VOL-A's cartridge is still loaded and
    # before fy_move: the scan reads the tape device (issue #275).
    check fy.no_plaintext_leak lc_leak_scan VOL-A
    check fy.move       fy_move
    check fy.cartridge  fy_cartridge

    restore_matrix VOL-A photos alice "$SRC/photos" fy-photos bob
    restore_matrix VOL-A docs   bob   "$SRC/docs"   fy-docs   alice
    restore_matrix VOL-A big    alice "$SRC/big"    fy-big    bob

    check fy.audit      fy_audit
    check fy.fsck       fy_fsck
    check fy.summary    fy_summary
}
# ============================================================
# Scenario: evolving-source
# ============================================================
# After first-year's state: mutate every unit's source, prove the dirty
# detector and snapshot diff see it, write v2 to a second volume, and prove
# BOTH versions remain restorable — v2 from the new volume, v1 still from
# the old one (multi-tape only).
ev_save_pristine() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: cp -a photos/docs/big to pristine-v1-* before mutating"; return 0; }
    local sd; sd="$(dirname "$HOME_DIR")"
    cp -a "$SRC/photos" "$sd/pristine-v1-photos" || return 1
    cp -a "$SRC/docs" "$sd/pristine-v1-docs" || return 1
    cp -a "$SRC/big" "$sd/pristine-v1-big" || return 1
}

# One mutation per unit, deterministic from --seed. `big` gets touch-only
# deliberately: under the default checksum_mode (mtime_size, config.rs
# default_checksum_mode), mtime_size compares mtime AND size, so a
# same-content/new-mtime edit DOES register dirty — src/collection/
# fingerprint.rs's own test names the ONE edit mtime_size is blind to as
# "same size, same mtime, different bytes", which touch-only is not. So
# the expectation here (documented, not assumed) is dirty=true for all
# three units, including the touched-only one.
ev_mutate() {
    mutate_source "$SRC/photos" "$SEED" modify || return 1
    mutate_source "$SRC/docs" "$SEED" add || return 1
    mutate_source "$SRC/big" "$SEED" touch-only || return 1
}

ev_dirty_lists_mutated() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl report dirty --json (assert photos, docs, big all listed, including the touch-only unit)"; return 0; }
    local logf="$RUN/log-ev.dirty.json"
    TCTL report dirty --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
names = {r.get("unit") for r in d} if isinstance(d, list) else set()
missing = {"photos", "docs", "big"} - names
assert not missing, f"expected photos/docs/big all dirty (touch-only counts under mtime_size), missing: {missing}, got: {names}"
' "$logf"
}

ev_snapshot_v2() { TCTL snapshot create photos && TCTL snapshot create docs && TCTL snapshot create big; }

ev_diff() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl snapshot diff --v1 1 --v2 2 photos/docs/big (assert non-empty, changed paths named)"; return 0; }
    local u
    for u in photos docs big; do
        TCTL snapshot diff --v1 1 --v2 2 "$u" >"$RUN/log-ev.diff.$u.txt" 2>&1 || { cat "$RUN/log-ev.diff.$u.txt"; return 1; }
        [ -s "$RUN/log-ev.diff.$u.txt" ] || { echo "snapshot diff v1..v2 for $u produced no output"; return 1; }
    done
}

ev_stage_v2() { TCTL stage create photos && TCTL stage create docs && TCTL stage create big; }

ev_write_volb() {
    next_tape VOL-B || return 1
    vinit VOL-B && TCTL volume write VOL-B --device "$TAPE_DEV"
}

ev_restore_v1_from_vola() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "ev.restore_v1_from_vola" "single-cartridge mode: VOL-A's cartridge was erased to become VOL-B — v1 is no longer on any tape this run controls"
        return $?
    fi
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: load_volume_tape VOL-A; tapectl restore unit --unit photos --from VOL-A --to DIR; assert identical to the pristine v1 copy"
        return 0
    fi
    load_volume_tape VOL-A || return 1
    local sd to; sd="$(dirname "$HOME_DIR")"; to="$sd/restore-v1-photos"
    TCTL restore unit --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" || return 1
    assert_identical "$sd/pristine-v1-photos" "$to"
}

ev_supersedable() { TCTL report supersedable; }
ev_age()          { TCTL report age; }

scenario_evolving_source() {
    check ev.setup         bootstrap_archive_v1
    check ev.save_pristine ev_save_pristine
    check ev.mutate        ev_mutate
    check ev.dirty         ev_dirty_lists_mutated
    check ev.snapshot_v2   ev_snapshot_v2
    check ev.diff          ev_diff
    check ev.stage_v2      ev_stage_v2
    check ev.write_volb    ev_write_volb

    restore_matrix VOL-B photos alice "$SRC/photos" ev-photos-v2 bob
    restore_matrix VOL-B docs   bob   "$SRC/docs"   ev-docs-v2   alice
    restore_matrix VOL-B big    alice "$SRC/big"    ev-big-v2    bob

    check ev.restore_v1_from_vola ev_restore_v1_from_vola
    check ev.supersedable         ev_supersedable
    check ev.age                  ev_age
}
# ============================================================
# Scenario: key-rotation
# ============================================================
# After first-year: rotate alice's key mid-archive and prove OLD (now
# inactive), NEW, and ESCROW keys all still restore — the old key's
# volume (VOL-A) as well as the new key's volume (VOL-C). VOL-C's checks
# run FIRST, while it is still the loaded tape from `next_tape`; VOL-A's
# checks reload it via `load_volume_tape` afterward. `key rotate` refusing
# without an escrow recipient is pre-existing behaviour covered in
# escrow-ordering, not re-tested here.
kr_rotate() { TCTL key rotate --tenant alice; }

kr_keylist_shows_rotation() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl key list --tenant alice --json (assert 1 active + >=1 inactive primary key)"; return 0; }
    local logf="$RUN/log-kr.keylist.json"
    TCTL key list --tenant alice --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
primaries = [k for k in d if k.get("key_type") == "primary" and not k.get("is_escrow")]
active = [k for k in primaries if k.get("is_active")]
inactive = [k for k in primaries if not k.get("is_active")]
assert len(active) == 1, f"expected exactly 1 active primary key after rotation, got {len(active)}: {active}"
assert len(inactive) >= 1, f"expected >=1 deactivated (pre-rotation) primary key, got {inactive}"
' "$logf"
}

kr_save_pristine() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: cp -a photos to pristine-v1-photos before mutating"; return 0; }
    local sd; sd="$(dirname "$HOME_DIR")"
    cp -a "$SRC/photos" "$sd/pristine-v1-photos"
}

kr_mutate_and_stage_v2() {
    mutate_source "$SRC/photos" "$SEED" modify || return 1
    TCTL snapshot create photos || return 1
    TCTL stage create photos
}

kr_write_volc() {
    next_tape VOL-C || return 1
    vinit VOL-C && TCTL volume write VOL-C --device "$TAPE_DEV"
}

# --- VOL-C checks (new key), run while VOL-C is still the loaded tape ---
kr_restore_volc_new_key() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl restore unit --unit photos --from VOL-C --to DIR (new key)"; return 0; }
    local sd to; sd="$(dirname "$HOME_DIR")"; to="$sd/restore-volc-photos"
    TCTL restore unit --unit photos --from VOL-C --to "$to" --device "$TAPE_DEV" || return 1
    assert_identical "$SRC/photos" "$to"
}

kr_restore_sh_new_key_volc() {
    local sd work newkey to
    sd="$(dirname "$HOME_DIR")"; work="$sd/heir-volc"; to="$sd/restore-sh-volc-photos"
    ensure_heir_restore_sh "$work" || return 1
    newkey="$(active_key_path alice primary)"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit photos --key $newkey --to $to (VOL-C, the NEW rotated key)"
        return 0
    fi
    (cd "$work/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit photos --key "$newkey" --to "$to") \
        >"$RUN/log-kr.restore_sh_new_volc.txt" 2>&1 || { cat "$RUN/log-kr.restore_sh_new_volc.txt"; return 1; }
    assert_identical "$SRC/photos" "$to"
}

kr_escrow_restores_volc() {
    local sd work to
    sd="$(dirname "$HOME_DIR")"; work="$sd/heir-volc"; to="$sd/escrow-volc-photos"
    ensure_heir_restore_sh "$work" || return 1
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit photos --key \$ESCROW_KEY_PATH --to $to (VOL-C)"
        return 0
    fi
    [ -f "$ESCROW_KEY_PATH" ] || { echo "no escrow key captured for this scenario"; return 1; }
    (cd "$work/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit photos --key "$ESCROW_KEY_PATH" --to "$to") \
        >"$RUN/log-kr.escrow_volc.txt" 2>&1 || { cat "$RUN/log-kr.escrow_volc.txt"; return 1; }
    assert_identical "$SRC/photos" "$to"
}

# --- VOL-A checks (old, now-inactive key), reloaded via load_volume_tape ---
kr_restore_vola_old_key() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "kr.restore_vola_old_key" "single-cartridge mode: VOL-A's cartridge was erased to become VOL-C"
        return $?
    fi
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: load_volume_tape VOL-A; tapectl restore unit --unit photos --from VOL-A --to DIR (old, now-inactive key — restore trial-decrypts every key)"
        return 0
    fi
    load_volume_tape VOL-A || return 1
    local sd to; sd="$(dirname "$HOME_DIR")"; to="$sd/restore-vola-photos"
    TCTL restore unit --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" || return 1
    assert_identical "$sd/pristine-v1-photos" "$to"
}

kr_restore_sh_old_key_vola() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "kr.restore_sh_old_key_vola" "single-cartridge mode: VOL-A's cartridge was erased to become VOL-C"
        return $?
    fi
    local sd work oldkey to
    sd="$(dirname "$HOME_DIR")"; work="$sd/heir-vola"; oldkey="$HOME_DIR/keys/alice-primary.age.key"; to="$sd/restore-sh-vola-photos"
    if [ "$DRY_RUN" != 1 ]; then load_volume_tape VOL-A || return 1; fi
    ensure_heir_restore_sh "$work" || return 1
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit photos --key $oldkey --to $to (VOL-A, the OLD deactivated key — its file never moves on rotation)"
        return 0
    fi
    (cd "$work/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit photos --key "$oldkey" --to "$to") \
        >"$RUN/log-kr.restore_sh_old_vola.txt" 2>&1 || { cat "$RUN/log-kr.restore_sh_old_vola.txt"; return 1; }
    local sd2; sd2="$(dirname "$HOME_DIR")"
    assert_identical "$sd2/pristine-v1-photos" "$to"
}

kr_escrow_restores_vola() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "kr.escrow_restores_vola" "single-cartridge mode: VOL-A's cartridge was erased to become VOL-C"
        return $?
    fi
    local sd work to
    sd="$(dirname "$HOME_DIR")"; work="$sd/heir-vola"; to="$sd/escrow-vola-photos"
    if [ "$DRY_RUN" != 1 ]; then load_volume_tape VOL-A || return 1; fi
    ensure_heir_restore_sh "$work" || return 1
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit photos --key \$ESCROW_KEY_PATH --to $to (VOL-A)"
        return 0
    fi
    [ -f "$ESCROW_KEY_PATH" ] || { echo "no escrow key captured for this scenario"; return 1; }
    (cd "$work/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit photos --key "$ESCROW_KEY_PATH" --to "$to") \
        >"$RUN/log-kr.escrow_vola.txt" 2>&1 || { cat "$RUN/log-kr.escrow_vola.txt"; return 1; }
    assert_identical "$sd/pristine-v1-photos" "$to"
}

scenario_key_rotation() {
    check kr.setup                    bootstrap_archive_v1
    check kr.rotate                   kr_rotate
    check kr.keylist                  kr_keylist_shows_rotation
    check kr.save_pristine            kr_save_pristine
    check kr.mutate_and_stage_v2      kr_mutate_and_stage_v2
    check kr.write_volc               kr_write_volc

    check kr.restore_volc_new_key     kr_restore_volc_new_key
    check kr.restore_sh_new_key_volc  kr_restore_sh_new_key_volc
    check kr.escrow_restores_volc     kr_escrow_restores_volc

    check kr.restore_vola_old_key     kr_restore_vola_old_key
    check kr.restore_sh_old_key_vola  kr_restore_sh_old_key_vola
    check kr.escrow_restores_vola     kr_escrow_restores_vola
}
# ============================================================
# Scenario: tenant-reassign
# ============================================================
# After first-year: move alice's units to bob, prove ownership actually
# moved, write a new version under bob, and prove alice's OLD volume
# (VOL-A) is still restorable both ways — reassignment changes DB
# ownership, not which key a slice was encrypted to.
tr_reassign() { TCTL tenant reassign --to bob alice; }

# `unit list --json` (src/db/queries.rs list_units) returns `tenant_id`,
# not a tenant name, so the move is checked via the `--tenant NAME` filter
# both ways rather than reading tenant_id numbers out of the JSON.
tr_unit_list_shows_move() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl unit list --tenant bob --json (assert photos,big present); --tenant alice --json (assert absent)"; return 0; }
    local bobf="$RUN/log-tr.unitlist.bob.json" alicef="$RUN/log-tr.unitlist.alice.json"
    TCTL unit list --tenant bob --json >"$bobf" 2>"$bobf.err" || { cat "$bobf.err" "$bobf"; return 1; }
    TCTL unit list --tenant alice --json >"$alicef" 2>"$alicef.err" || { cat "$alicef.err" "$alicef"; return 1; }
    python3 -c '
import json, sys
bob = {u.get("name") for u in json.load(open(sys.argv[1]))}
alice = {u.get("name") for u in json.load(open(sys.argv[2]))}
missing = {"photos", "big"} - bob
assert not missing, f"expected photos+big under bob after reassignment, missing: {missing} (bob has {bob})"
leftover = {"photos", "big"} & alice
assert not leftover, f"photos/big still listed under alice after reassignment: {leftover}"
' "$bobf" "$alicef"
}

tr_save_pristine() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: cp -a photos to pristine-v1-photos before mutating"; return 0; }
    local sd; sd="$(dirname "$HOME_DIR")"
    cp -a "$SRC/photos" "$sd/pristine-v1-photos"
}

tr_write_photos_v2() {
    mutate_source "$SRC/photos" "$SEED" modify || return 1
    TCTL snapshot create photos || return 1
    TCTL stage create photos || return 1
    next_tape VOL-D || return 1
    vinit VOL-D && TCTL volume write VOL-D --device "$TAPE_DEV"
}

tr_restore_vola_via_tctl() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "tr.restore_vola_via_tctl" "single-cartridge mode: VOL-A's cartridge was erased to become VOL-D"
        return $?
    fi
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: load_volume_tape VOL-A; tapectl restore unit --unit photos --from VOL-A --to DIR (alice's key still exists even though the DB now says bob owns photos)"
        return 0
    fi
    load_volume_tape VOL-A || return 1
    local sd to; sd="$(dirname "$HOME_DIR")"; to="$sd/restore-vola-photos"
    TCTL restore unit --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" || return 1
    assert_identical "$sd/pristine-v1-photos" "$to"
}

tr_restore_sh_vola_alice_key() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "tr.restore_sh_vola_alice_key" "single-cartridge mode: VOL-A's cartridge was erased to become VOL-D"
        return $?
    fi
    local sd work alicekey to
    sd="$(dirname "$HOME_DIR")"; work="$sd/heir-vola"; alicekey="$HOME_DIR/keys/alice-primary.age.key"; to="$sd/restore-sh-vola-photos"
    if [ "$DRY_RUN" != 1 ]; then load_volume_tape VOL-A || return 1; fi
    ensure_heir_restore_sh "$work" || return 1
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit photos --key $alicekey --to $to (VOL-A, alice's key — RESTORE.sh has no DB and knows nothing of the reassignment)"
        return 0
    fi
    (cd "$work/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit photos --key "$alicekey" --to "$to") \
        >"$RUN/log-tr.restore_sh_vola.txt" 2>&1 || { cat "$RUN/log-tr.restore_sh_vola.txt"; return 1; }
    local sd2; sd2="$(dirname "$HOME_DIR")"
    assert_identical "$sd2/pristine-v1-photos" "$to"
}

tr_catalog_locate() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl catalog locate photos --json (assert both VOL-A and VOL-D named)"; return 0; }
    local logf="$RUN/log-tr.locate.json"
    TCTL catalog locate photos --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    grep -q "VOL-A" "$logf" || { echo "VOL-A not named in catalog locate photos:"; cat "$logf"; return 1; }
    grep -q "VOL-D" "$logf" || { echo "VOL-D not named in catalog locate photos:"; cat "$logf"; return 1; }
}

scenario_tenant_reassign() {
    check tr.setup                    bootstrap_archive_v1
    check tr.reassign                 tr_reassign
    check tr.unit_list_shows_move     tr_unit_list_shows_move
    check tr.save_pristine            tr_save_pristine
    check tr.write_photos_v2          tr_write_photos_v2

    restore_matrix VOL-D photos bob "$SRC/photos" tr-photos-bob alice

    check tr.restore_vola_via_tctl    tr_restore_vola_via_tctl
    check tr.restore_sh_vola_alice_key tr_restore_sh_vola_alice_key
    check tr.catalog_locate           tr_catalog_locate
}
# ============================================================
# Scenario: tape-only-and-reclaim
# ============================================================
# After bootstrap_two_volumes (VOL-A/vault v1, VOL-B/offsite v2): every
# first-year unit has 2 sealed copies in 2 locations, so mark-tape-only
# must PASS for them; a freshly-written "solo" unit with only 1 copy must
# be REFUSED, naming the shortfall. Then reclaim v1 (mark-reclaimable ->
# purge), staging clean, and prove the LATEST version is still restorable.
#
# --single-cartridge note: next_tape's single-cartridge branch now retires
# the previous label in the DB before reusing its cartridge (added this
# commit — ADR-0004's copy count is DB-status-only, so without this a
# reused cartridge would go on being credited as a live copy it no longer
# physically is). That makes "2 copies in 2 locations" genuinely
# unreachable under --single-cartridge: VOL-A is retired the moment VOL-B
# is created. So only the one-copy REFUSAL is meaningful there; everything
# that assumes 2 real copies is SKIP, visibly, not a failure.
tor_solo_unit() {
    make_source "$SRC/solo" "plain" || return 1
    TCTL unit init "$SRC/solo" --tenant alice --name solo || return 1
    TCTL snapshot create solo || return 1
    TCTL stage create solo || return 1
    next_tape VOL-SOLO || return 1
    vinit VOL-SOLO || return 1
    TCTL volume write VOL-SOLO --device "$TAPE_DEV" || return 1
    # VOL-SOLO must be PLACED, or the next check fails for a reason this
    # scenario does not intend (issue #203, first `--all` run 2026-09-16).
    #
    # `volume write` writes every still-staged set (ADR-0006 stage-once /
    # write-N-copies), so VOL-SOLO carries photos v1 AND v2 as well as solo.
    # That left photos v2 on VOL-B (offsite) and VOL-SOLO (nowhere), i.e. two
    # copies across ONE named location — and since #153 a unit is as covered as
    # its least-covered live version, so `mark-tape-only photos` refused with
    # "insufficient locations: 1 < 2". The refusal was correct; the scenario
    # simply never gave the second copy a home. Placing VOL-SOLO supplies the
    # location without adding a copy of `solo`, so tor.mark_tape_only_solo_refused
    # still refuses — for insufficient COPIES, which is what it asserts.
    TCTL volume move VOL-SOLO --to vault || return 1
}

tor_mark_tape_only_photos_passes() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "tor.mark_tape_only_photos_passes" "single-cartridge mode: VOL-A was retired when VOL-B/VOL-SOLO reused its cartridge, so photos genuinely has <2 live copies here"
        return $?
    fi
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl unit mark-tape-only photos (>=2 copies in >=2 locations -> PASS)"; return 0; }
    TCTL unit mark-tape-only photos
}

tor_mark_tape_only_solo_refused() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl unit mark-tape-only solo (1 copy -> REFUSED, message names the shortfall)"; return 0; }
    local out rc
    out="$(TCTL unit mark-tape-only solo 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "mark-tape-only solo unexpectedly succeeded with only 1 copy: $out"; return 1; }
    echo "$out" | grep -qi "insufficient copies" || { echo "refusal did not name the shortfall ('insufficient copies'): $out"; return 1; }
}

tor_report_tape_only() { TCTL report tape-only; }

tor_mark_reclaimable_v1_photos() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "tor.mark_reclaimable_v1_photos" "single-cartridge mode: VOL-A (v1's only copy) was retired, not merely superseded"
        return $?
    fi
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl snapshot mark-reclaimable --version 1 photos (v2 exists and is written)"; return 0; }
    TCTL snapshot mark-reclaimable --version 1 photos
}

tor_purge_v1_photos() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "tor.purge_v1_photos" "depends on tor.mark_reclaimable_v1_photos, itself SKIP under --single-cartridge"
        return $?
    fi
    TCTL snapshot purge --version 1 photos
}

# `staging clean` at this point in the scenario is the ADR-0012 refusal, not
# housekeeping — and asserting both halves is worth more than adding --force
# and moving on.
#
# `solo` has exactly ONE completed copy against this suite's min_copies = 2 (the
# check immediately above, tor.mark_tape_only_solo_refused, asserts that very
# fact). Issue #244's ruling makes a bare `staging clean` REFUSE that, naming
# the under-copied unit, because releasing here destroys the only cheap route
# to the second copy the operator's own policy requires. `--force` is the
# documented override and the scenario genuinely means it: it is done with
# these staged bytes.
#
# Flagged before it could turn the gate red, by the #244 worker reading
# scripts/ read-only — it could not edit this file and correctly reported it
# instead. Without that, this check would have gone PASS -> FAIL on the next
# --all with the fix looking like the culprit.
tor_staging_clean() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl staging clean (expect SUCCESS, retaining solo at 1/2 copies and naming --force), assert solo still 'staged', then tapectl staging clean --force"; return 0; }
    local out rc
    out="$(TCTL staging clean 2>&1)"; rc=$?
    # ADR-0012's 2026-09-21 amendment (issue #262) corrected #244's ruling from
    # "the command refuses" to "the command retains the under-copied units and
    # succeeds". The protection is unchanged -- solo's staged bytes must still
    # be there afterwards, and that is what this check now proves directly
    # rather than inferring it from an exit code.
    #
    # The refusal was whole-command, so one stuck unit blocked every COVERED
    # unit's release until staging filled; and --force, the only escape, has a
    # wider candidate set than the gate it bypasses.
    [ "$rc" -eq 0 ] || {
        echo "staging clean must now SUCCEED while retaining the under-copied unit (issue #262), got rc=$rc: $out"
        return 1
    }
    echo "$out" | grep -q "solo" || {
        echo "staging clean retained data but did not name the under-copied unit, so an operator cannot tell which one: $out"
        return 1
    }
    echo "$out" | grep -q -- "--force" || {
        echo "staging clean retained data without naming --force as the override: $out"
        return 1
    }
    # The load-bearing assertion, and the reason the exit code was never the
    # real subject: solo's staged data must SURVIVE a bare clean. Asserted
    # against the catalog, not against the message.
    local still
    still="$(TCTL stage list --json 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin)
rows = d if isinstance(d, list) else d.get("stage_sets", [])
print(sum(1 for r in rows if isinstance(r, dict)
          and r.get("status") == "staged"
          and "solo" in str(r.get("unit", ""))))
' 2>/dev/null || echo 0)"
    [ "${still:-0}" -ge 1 ] || {
        echo "solo is at 1/2 copies and its staged data was released by a bare \`staging clean\` -- issue #244's protection is not holding: $out"
        return 1
    }
    # Put the retention notice in the report. Without this the log shows only
    # the --force call's "cleaned N stage set(s)" line, and a reader cannot
    # tell a real retention from a check that passed for some other reason --
    # the assertions above would be the only evidence, and evidence you cannot
    # see is how this suite produced five greens for the wrong reason.
    echo "the retention notice, verbatim:"
    echo "$out" | sed 's/^/    /'
    TCTL staging clean --force
}
tor_report_copies() { TCTL report copies; }

tor_restore_latest() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "tor.restore_latest" "single-cartridge mode: VOL-B's cartridge was reused for VOL-SOLO"
        return $?
    fi
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: load_volume_tape VOL-B; tapectl restore unit --unit photos --from VOL-B --to DIR; assert identical to the latest (mutated) source"
        return 0
    fi
    load_volume_tape VOL-B || return 1
    local sd to; sd="$(dirname "$HOME_DIR")"; to="$sd/restore-latest-photos"
    TCTL restore unit --unit photos --from VOL-B --to "$to" --device "$TAPE_DEV" || return 1
    assert_identical "$SRC/photos" "$to"
}

scenario_tape_only_and_reclaim() {
    # ORDER MATTERS, and it did not used to (issue #203, 2026-09-16).
    #
    # Reclaim runs BEFORE mark-tape-only. `snapshot mark-reclaimable` applies a
    # 2x copy multiplier once a unit is tape-only — delete the source and you
    # need more tape before discarding an older version — so with photos marked
    # tape-only first, reclaiming v1 demands FOUR copies of the superseding v2
    # and this scenario writes two. Correct behaviour; wrong order.
    #
    # It was hidden because the two failures cancelled: mark-tape-only was
    # itself failing (VOL-SOLO was never placed, so photos v2 had two copies in
    # one location), photos therefore never became tape-only, and reclaim got
    # the 1x rule and passed. Fixing the placement made the real conflict
    # visible. Both of the later checks had been GREEN FOR THE WRONG REASON —
    # they were passing because an earlier check was red.
    check tor.setup                        bootstrap_two_volumes
    check tor.solo_unit                    tor_solo_unit
    check tor.mark_reclaimable_v1_photos   tor_mark_reclaimable_v1_photos
    check tor.purge_v1_photos              tor_purge_v1_photos
    check tor.mark_tape_only_photos_passes tor_mark_tape_only_photos_passes
    check tor.mark_tape_only_solo_refused  tor_mark_tape_only_solo_refused
    check tor.report_tape_only             tor_report_tape_only
    check tor.staging_clean                tor_staging_clean
    check tor.report_copies                tor_report_copies
    check tor.restore_latest               tor_restore_latest
}
# ============================================================
# Scenario: compaction (mhvtl-only)
# ============================================================
# Needs FOUR simultaneously-distinct volumes (VOL-E, VOL-F, VOL-H, VOL-G) to
# mean anything — impossible under --single-cartridge, which destroys each
# previous volume's cartridge on next_tape (see tape-only-and-reclaim's
# next_tape fix). The whole scenario SKIPs there, visibly, rather than
# faking a single-cartridge shape that wouldn't test compaction at all.
#
# Sequence: VOL-E starts with photos/docs/big v1. `staging clean` then
# RELEASES those three stage sets — without it they stay 'staged' and every
# later `volume write` writes them again (see cp_release_staging; this is
# what made VOL-F a full second copy of everything, issue #198). photos v2
# goes to VOL-F, and a second COPY of that same v2 to VOL-H; release again,
# so the compaction destination carries compaction slices and nothing else.
# photos v1 on VOL-E is then supersedable; mark it reclaimable and purge it,
# leaving docs v1 and big v1 as VOL-E's only live content — under
# bootstrap_config's utilization_threshold=0.95 that is enough for
# `report compaction-candidates` to flag VOL-E. compact-read pulls those
# live slices to staging; compact-finish is asserted to REFUSE before
# compact-write has given docs/big a copy anywhere else, then to SUCCEED
# once VOL-G holds one.
cp_skip_single_cartridge() {
    skip "cp.scenario" "compaction needs 4 simultaneously-distinct volumes (VOL-E/F/H/G) — impossible under --single-cartridge"
    return $?
}

cp_write_photos_v2_on_volf() {
    mutate_source "$SRC/photos" "$SEED" modify || return 1
    TCTL snapshot create photos || return 1
    TCTL stage create photos || return 1
    next_tape VOL-F || return 1
    vinit VOL-F && TCTL volume write VOL-F --device "$TAPE_DEV"
}

# `staging clean` is the RELEASE half of tapectl's stage-once / write-N-
# copies / release design (CLAUDE.md, Collection layer). `volume write`
# writes every stage set that is still 'staged' and deliberately leaves it
# 'staged' (src/volume/write.rs `find_staged_data`), so until something
# releases them, each later write writes them AGAIN.
#
# This scenario never did that, which is why issue #198's diagnosis was
# incomplete: VOL-F was not "photos v2" as the comment claimed, it was
# photos v1 + docs v1 + big v1 + photos v2 — verified from the run DB.
# That also made cp.compact_finish_refused fail on its own merits rather
# than as a cascade: docs/big DID have a copy off VOL-E, so compact-finish
# had nothing to refuse.
#
# A coverage gate IS involved, since issue #244. `clean_staging`
# (src/staging/clean.rs) is still policy-free -- it releases a 'staged' set
# once it has at least one `writes` row and every one of them is
# 'completed', which is true of v1 the moment VOL-E is written -- but the
# CLI caller now refuses first when any unit's staged data sits below its
# policy's `min_copies`, and here all three do:
#
#     error: staging clean refused: ... (issue #244). Pass --force ...
#       big: 1/2 copies
#       docs: 1/2 copies
#       photos: 1/2 copies
#
# `--force` is the honest answer for THIS scenario rather than a way around
# the gate. v1 is deliberately left at one copy because it is about to be
# superseded by v2 and then reclaimed (`cp.reclaim_v1_photos` below) --
# which is precisely the case #244's `--force` exists for: the operator
# saying "I am giving up the cheap second copy of this version on purpose."
# Writing a second copy of v1 instead would make the scenario stop
# modelling supersession, and dropping the release would put the scenario
# back in the state issue #198 found it in.
#
# The refusal itself is asserted by `tor.staging_clean`
# (tape-only-and-reclaim), which owns that rule and fails if the gate ever
# stops holding. This is a setup step; it does not re-assert it.
cp_release_staging() { TCTL staging clean --force; }

# A second COPY of photos v2 — not a new version, and the reason this
# scenario was RED on master (issue #198).
#
# `snapshot mark-reclaimable` refuses to release v1 while the version that
# SUPERSEDES it is itself below `defaults.min_copies`
# (`policy::reclaimable`'s precondition 2 — ADR-0004's rule, and the point
# of issue #89's eligibility JOIN):
#
#     error: superseding v2 has 1 copies, needs 2 (use --force to override)
#
# tapectl is CORRECT there; the scenario was stale. It is fixed by giving
# v2 the copy the policy asks for, not by passing --force: `--force` on
# mark-reclaimable means "the operator is giving this version up on
# purpose" (ADR-0012 names it as the deliberate escape), so forcing here
# would make the scenario stop exercising the precondition altogether —
# and this scenario is called `compaction` precisely because compaction
# happens on adequately-covered data.
#
# The copy is a SECOND WRITE of the still-live stage set, not a re-stage.
# A re-stage is refused outright while the set is live ("unit \"photos\" v2
# already has a stage set with live slices"), so the `stage create` idiom
# in `rr_write_second_copy_volb` cannot ever have run green either. Writing
# the same staged slices twice is also the more faithful shape: it yields
# BYTE-IDENTICAL content, which is what ADR-0012 defines a Copy to be,
# where a re-stage would produce fresh bytes (dar timestamps, randomized
# age) for the same version.
cp_write_photos_v2_second_copy() {
    next_tape VOL-H || return 1
    vinit VOL-H && TCTL volume write VOL-H --device "$TAPE_DEV"
}

cp_reclaim_v1_photos() {
    TCTL snapshot mark-reclaimable --version 1 photos && TCTL snapshot purge --version 1 photos
}

cp_compaction_candidates_lists_vole() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl report compaction-candidates --json (assert VOL-E listed)"; return 0; }
    local logf="$RUN/log-cp.candidates.json"
    TCTL report compaction-candidates --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    grep -q "VOL-E" "$logf" || { echo "VOL-E not listed as a compaction candidate:"; cat "$logf"; return 1; }
}

cp_compact_read_vole() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: load_volume_tape VOL-E; tapectl volume compact-read VOL-E --device \$TAPE_DEV"
        return 0
    fi
    load_volume_tape VOL-E || return 1
    TCTL volume compact-read VOL-E --device "$TAPE_DEV"
}

# Read the refusal text from src/volume/write.rs's compact_finish: "cannot
# retire \"<label>\": <N> live slice(s) have no copy on another volume
# (...)" — fires here because docs/big's only completed write is still
# VOL-E itself; nothing has been written to VOL-G yet.
cp_compact_finish_refused_first() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume compact-finish VOL-E (expect refusal: 'have no copy on another volume' — docs/big have no copy yet)"; return 0; }
    local out rc
    out="$(TCTL volume compact-finish VOL-E 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "compact-finish VOL-E unexpectedly succeeded before VOL-G existed: $out"; return 1; }
    echo "$out" | grep -qi "have no copy on another volume" || { echo "unexpected refusal text: $out"; return 1; }
}

# `vinit` is not optional here, and its absence was a third latent defect
# (issue #198): `compact-write --destination VOL-G` resolves VOL-G as an
# existing volume row, so without `volume init` it fails with a bare
# "volume not found: VOL-G". Nothing caught it because the scenario had
# never reached this step — cp.reclaim_v1_photos died six checks earlier.
# Every other destination in this suite is `next_tape` + `vinit` + write;
# this one had dropped the middle term.
cp_write_volg() {
    next_tape VOL-G || return 1
    vinit VOL-G || return 1
    TCTL volume compact-write --destination VOL-G --device "$TAPE_DEV"
}

# --yes because this act is now ADR-0008 Tier 2 (issue #147): finishing the
# compaction retires VOL-E, which leaves big v1 and docs v1 at one copy each,
# below this suite's min_copies = 2. Degraded but non-zero is exactly what
# Tier 2 gates, and the suite is a non-interactive operator who has decided to
# proceed — every TCTL call already runs with </dev/null, so there is no
# prompt to answer and saying so explicitly is the truthful form.
#
# This is NOT a way past the floor: --yes does not reach Tier 3, and
# cp.compact_finish_refused_first still proves the unprotected-slice case is
# refused outright.
cp_compact_finish_succeeds() { TCTL volume compact-finish VOL-E --yes; }

scenario_compaction() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        check cp.scenario cp_skip_single_cartridge
        return 0
    fi

    check cp.setup                     bootstrap_archive_v1 VOL-E
    check cp.release_v1_staging        cp_release_staging
    check cp.write_photos_v2_on_volf   cp_write_photos_v2_on_volf
    check cp.photos_v2_second_copy     cp_write_photos_v2_second_copy
    check cp.release_v2_staging        cp_release_staging
    check cp.reclaim_v1_photos         cp_reclaim_v1_photos
    check cp.compaction_candidates     cp_compaction_candidates_lists_vole
    check cp.compact_read_vole         cp_compact_read_vole
    check cp.compact_finish_refused    cp_compact_finish_refused_first
    check cp.write_volg                cp_write_volg
    check cp.compact_finish_succeeds   cp_compact_finish_succeeds

    restore_matrix VOL-G docs bob   "$SRC/docs" cp-docs-volg alice
    restore_matrix VOL-G big  alice "$SRC/big"  cp-big-volg  bob
}
# ============================================================
# Scenario: retire-and-reuse
# ============================================================
# After first-year: retire VOL-A while it is the sole copy (refused, then
# driven with --yes... actually driven properly by first giving every unit
# a second copy so the SAME retire call succeeds without needing consent —
# ADR-0008 Tier 2 only fires on a zero-copy unit); then the documented
# cartridge-reuse procedure (docs/operator-guide.md: register -> retire ->
# physically erase -> `cartridge mark-erased` -> reuse); and the ADR-0003
# negative — a still-sealed VOL-A cartridge refuses `volume init` for a
# DIFFERENT label both with and without --force.
#
# Decided from src/cli/operations.rs:540 (`cartridge mark-erased`'s
# precondition is the cartridge's DB status == 'pending_erase', which
# `volume retire` sets — not whether the tape was physically erased, which
# the DB cannot observe): mark-erased attempted BEFORE `volume retire` (the
# cartridge is still 'active') hits the ADR-0008 Tier-2 consent gate and is
# refused without --yes/--force; attempted AFTER retire (pending_erase) it
# needs no consent at all, physical erase or not. This scenario tests the
# refusal in that true position (before retire), not "before the physical
# erase" as a separate gate — there isn't one.
rr_mark_erased_before_retire_refused() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl cartridge mark-erased \$barcode (cartridge still 'active', not 'pending_erase' -> Tier-2 refusal without --yes)"; return 0; }
    [ -n "${RR_VOLA_BARCODE:-}" ] || { echo "no barcode captured for VOL-A's cartridge"; return 1; }
    local out rc
    out="$(TCTL cartridge mark-erased "$RR_VOLA_BARCODE" 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "cartridge mark-erased unexpectedly succeeded before any retirement: $out"; return 1; }
}

# Two attempts, and the SECOND is the one that matters (issue #223).
#
# Until 2026-09-17 this ran `volume retire VOL-A` with no --yes and grepped for
# "ZERO copies remaining". That string is the impact analysis, which prints
# whether the refusal came from the ADR-0008 Tier-3 floor or merely from the
# ordinary non-interactive "nobody confirmed" guard — so the check could not
# tell those apart, and before #147 it would have passed for the wrong reason
# entirely. Meanwhile the comment above cp_compact_finish_succeeds asserts in
# prose that "--yes does not reach Tier 3", and nothing in this suite ever
# passed --yes to a Tier-3 case to find out.
#
# So: attempt 1 without consent (the operator-facing display), attempt 2 WITH
# --yes, which must still be refused and must cite the floor's own language.
# That is what makes the prose claim a measurement.
rr_retire_refused_sole_copy() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume retire VOL-A (no --yes -> refused, impact names a ZERO-copy unit); then --yes (ADR-0008 Tier 3 -> STILL refused, names the LAST eligible copy and says no --force reaches it)"; return 0; }
    local out rc
    out="$(TCTL volume retire VOL-A 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "volume retire VOL-A unexpectedly succeeded without consent while it is the sole copy: $out"; return 1; }
    echo "$out" | grep -qi "ZERO copies remaining" || { echo "impact analysis did not name a zero-copy unit ('ZERO copies remaining'): $out"; return 1; }

    out="$(TCTL volume retire VOL-A --yes 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || {
        echo "volume retire VOL-A --yes SUCCEEDED on the sole eligible copy of a live version -- ADR-0008 Tier 3 is absolute and no flag may reach it (issue #147): $out"
        return 1
    }
    echo "$out" | grep -qi "LAST eligible copy" || {
        echo "refused with --yes, but not by the Tier-3 floor -- the message does not name the LAST eligible copy, so this refusal is the non-interactive guard and the floor is unproven: $out"
        return 1
    }
    echo "$out" | grep -qi "no --force for this" || {
        echo "the Tier-3 refusal did not say that no flag reaches it, which is the property this check exists to pin: $out"
        return 1
    }
}

# A second COPY of the same v1 content — not a new version, and not
# `volume read-slices` (which MOVES slices into staging for a follow-on
# write, self-describing invariant preserved, rather than duplicating them).
#
# This used to re-stage each unit (`stage create <unit> --version 1`) before
# writing, and that CANNOT WORK — it is the same defect issue #198 found in
# the compaction scenario, and it made this scenario red on master too
# (measured 2026-09-16: 10 checks, 6 passed, 4 failed, first failure here).
# `volume write` leaves every stage set it writes `'staged'`
# (src/volume/write.rs `find_staged_data`; `staging clean` is the release
# half of that design), and `stage create` refuses a version that still has
# a live set:
#
#     error: unit "photos" v1 already has a stage set with live slices
#
# So the second copy is simply a SECOND WRITE of the sets that are still
# live from `bootstrap_archive_v1`. That is also the more faithful shape:
# it yields byte-identical content, which is what ADR-0012 defines a Copy to
# be, where a re-stage would produce fresh bytes (dar timestamps, randomized
# age) for the same version.
rr_write_second_copy_volb() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.write_second_copy_volb" "single-cartridge mode cannot hold a second, independent copy of VOL-A's content"
        return $?
    fi
    next_tape VOL-B || return 1
    vinit VOL-B && TCTL volume write VOL-B --device "$TAPE_DEV"
}

rr_retire_vola_succeeds_with_coverage() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.retire_vola_succeeds_with_coverage" "depends on rr.write_second_copy_volb, itself SKIP under --single-cartridge"
        return $?
    fi
    # Tier 2, and --yes says so (issue #147). The old dry-run line claimed "no
    # consent is needed" because every unit has a second copy on VOL-B — that
    # encoded the INVERTED tiers, where only zero coverage was gated. Under
    # ADR-0008 as ratified, dropping each unit from two copies to one is below
    # this suite's min_copies = 2 and is precisely what Tier 2 exists for.
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume retire VOL-A --yes (Tier 2: VOL-B carries a second copy, so this leaves every unit at 1, below min_copies=2)"; return 0; }
    TCTL volume retire VOL-A --yes
}

# ADR-0003 negative, run BEFORE the physical erase: VOL-A's cartridge is
# still loaded and still physically sealed even though the DB now says
# "retired" — File 0 and the seal marker are exactly as `volume write`
# left them. `volume init VOL-X` must refuse on THIS tape both with and
# without --force (src/volume/write.rs's check_fresh_write_contact_
# foreign_sealed_tape_refuses_even_with_force test; every AlreadySealed
# refusal cites "ADR-0003" in its message).
rr_volinit_volx_refused_no_force() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.volinit_volx_refused_no_force" "single-cartridge mode: VOL-A's cartridge was already reused by an earlier next_tape in this run"
        return $?
    fi
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: load_volume_tape VOL-A; tapectl volume init VOL-X --device \$TAPE_DEV (no --force; expect refused, message cites ADR-0003)"
        return 0
    fi
    load_volume_tape VOL-A || return 1
    local out rc
    out="$(TCTL volume init VOL-X --device "$TAPE_DEV" 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "volume init VOL-X unexpectedly succeeded on a cartridge still sealed as VOL-A: $out"; return 1; }
    echo "$out" | grep -q "ADR-0003" || { echo "refusal did not cite ADR-0003: $out"; return 1; }
}

rr_volinit_volx_refused_with_force() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.volinit_volx_refused_with_force" "single-cartridge mode: VOL-A's cartridge was already reused by an earlier next_tape in this run"
        return $?
    fi
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume init VOL-X --device \$TAPE_DEV --force (ADR-0003: force never overrides a sealed tape; expect STILL refused, ADR-0003 cited)"; return 0; }
    local out rc
    out="$(TCTL volume init VOL-X --device "$TAPE_DEV" --force 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "volume init VOL-X --force unexpectedly succeeded over a sealed cartridge (violates ADR-0003): $out"; return 1; }
    echo "$out" | grep -q "ADR-0003" || { echo "refusal did not cite ADR-0003: $out"; return 1; }
}

# Now the documented reuse procedure for real: physically erase, THEN
# `cartridge mark-erased` (needs no consent now — status is 'pending_erase'
# since the retire above), THEN `volume init` on the reused cartridge
# succeeds WITHOUT --force (the tape is genuinely blank now).
# `blank_tape`, not `erase_tape`: this step stands in for the operator
# physically erasing the cartridge, and the next check asserts `volume init`
# then succeeds WITHOUT --force. Under `--erase short` (the mode the
# autopilot Policy runs), `erase_tape` is `weof 1` at BOT, which UNSEALS a
# tape but does not blank one — a read at BOT still returns the previous
# volume's bytes and init correctly refuses:
#
#     error: refusing to write volume "VOL-H": the loaded cartridge's File 0
#     already identifies a DIFFERENT volume ... re-run with --force
#
# That is the issue #194 finding, in a scenario that had never reached this
# step to show it (issue #198). `blank_tape` is the helper #194 added for
# precisely this: a real erase whatever `--erase` says. Giving the init
# `--force` instead would defeat the check this scenario exists to prove.
rr_physical_erase() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.physical_erase" "single-cartridge mode: VOL-A's cartridge was already reused by an earlier next_tape in this run"
        return $?
    fi
    blank_tape
}

rr_mark_erased_after_retire_succeeds() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.mark_erased_after_retire_succeeds" "depends on rr.physical_erase, itself SKIP under --single-cartridge"
        return $?
    fi
    # Tier 2 as well (issue #147), and this one the original #147 harness
    # commit did not predict — it was found by running --all against the
    # rebased branch rather than by reasoning about which checks would move.
    # mark-erased declares VOL-A's bytes gone, which is a Tier-2 statement
    # whenever there is anything to warn about.
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl cartridge mark-erased \$barcode --yes (Tier 2: declares VOL-A's bytes gone)"; return 0; }
    [ -n "${RR_VOLA_BARCODE:-}" ] || { echo "no barcode captured for VOL-A's cartridge"; return 1; }
    TCTL cartridge mark-erased "$RR_VOLA_BARCODE" --yes
}

rr_volinit_volh_on_reused_cartridge_succeeds() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.volinit_volh_on_reused_cartridge_succeeds" "depends on rr.physical_erase, itself SKIP under --single-cartridge"
        return $?
    fi
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume init VOL-H --device \$TAPE_DEV (no --force; blank tape now -> succeeds)"; return 0; }
    TCTL volume init VOL-H --device "$TAPE_DEV"
}

scenario_retire_and_reuse() {
    check rr.setup bootstrap_archive_v1 VOL-A
    # The barcode is whatever `volume init` bound (the MAM medium serial when
    # one is readable), NOT the changer's VolumeTag - ADR-0010.
    if [ "$DRY_RUN" = 1 ]; then
        RR_VOLA_BARCODE="$LOADED_TAG"
    else
        RR_VOLA_BARCODE="$(python3 - "$HOME_DIR/tapectl.db" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT c.barcode FROM cartridges c "
    "JOIN cartridge_volumes cv ON cv.cartridge_id = c.id "
    "JOIN volumes v ON v.id = cv.volume_id "
    "WHERE v.label = 'VOL-A' ORDER BY cv.id DESC LIMIT 1").fetchone()
print(row[0] if row else "")
PY
)"
        [ -n "$RR_VOLA_BARCODE" ] || RR_VOLA_BARCODE="$LOADED_TAG"
    fi

    check rr.mark_erased_before_retire_refused rr_mark_erased_before_retire_refused
    check rr.retire_refused_sole_copy          rr_retire_refused_sole_copy
    check rr.write_second_copy_volb            rr_write_second_copy_volb
    check rr.retire_vola_succeeds_with_coverage rr_retire_vola_succeeds_with_coverage
    check rr.volinit_volx_refused_no_force     rr_volinit_volx_refused_no_force
    check rr.volinit_volx_refused_with_force   rr_volinit_volx_refused_with_force
    check rr.physical_erase                    rr_physical_erase
    check rr.mark_erased_after_retire_succeeds rr_mark_erased_after_retire_succeeds
    check rr.volinit_volh_on_reused_cartridge_succeeds rr_volinit_volh_on_reused_cartridge_succeeds
}
# ============================================================
# Scenario: db-loss
# ============================================================
# After first-year: three "what if $HOME_DIR is gone" arms, none of which
# ever touch $HOME_DIR again once bootstrapped. VOL-A stays loaded
# throughout (no next_tape call happens after bootstrap), so this scenario
# needs no --single-cartridge guards at all.
#
# Decided from src/main.rs:118-120: EVERY command except `init` and
# `completions` requires `paths.is_initialized()`, so even `restore
# raw-volume` — which touches nothing in the DB — needs a bare `tapectl
# init` on the new home first. Recorded here rather than assumed.
dl_backup() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl db backup --to \$sd/backup.db --include-keys"; return 0; }
    local sd; sd="$(dirname "$HOME_DIR")"
    TCTL db backup --to "$sd/backup.db" --include-keys
}

# (a) NEW empty home + `db import`: restores the FULL catalog (units,
# snapshots, writes — everything `restore unit` needs), so this arm is
# expected to work end-to-end.
dl_scenario_a_db_import() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: new home; tapectl init; tapectl db import \$sd/backup.db; copy keys; tapectl catalog locate photos (assert VOL-A named); tapectl restore unit --unit photos --from VOL-A --to DIR (assert identical)"
        return 0
    fi
    local sd newhome; sd="$(dirname "$HOME_DIR")"; newhome="$sd/newhome-a"
    mkdir -p "$newhome"
    NEWHOME_TCTL "$newhome" init --operator "$OPERATOR" --no-escrow >"$sd/dl.a.init.txt" 2>&1 || { cat "$sd/dl.a.init.txt"; return 1; }
    NEWHOME_TCTL "$newhome" db import "$sd/backup.db" --yes >"$sd/dl.a.import.txt" 2>&1 || { cat "$sd/dl.a.import.txt"; return 1; }
    mkdir -p "$newhome/keys"
    cp -a "$sd/backup.keys/." "$newhome/keys/" 2>/dev/null || { echo "could not copy backup.keys into the new home"; return 1; }
    local logf="$sd/dl.a.locate.json"
    NEWHOME_TCTL "$newhome" catalog locate photos --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    grep -q "VOL-A" "$logf" || { echo "VOL-A not named in catalog locate photos:"; cat "$logf"; return 1; }
    local to="$sd/dl.a.restore-photos"
    NEWHOME_TCTL "$newhome" restore unit --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" \
        >"$sd/dl.a.restore.txt" 2>&1 || { cat "$sd/dl.a.restore.txt"; return 1; }
    assert_identical "$SRC/photos" "$to"
}

# (b) NEW empty home, NO backup at all: `restore raw-volume` is DB-less by
# design and must verify every file from the tape's own front index alone.
# Then top-level `tapectl import` (`volume_import` in src/cli/operations.rs)
# — decided by reading it: it inserts ONLY a bare `volumes` row (label,
# backend, media type, capacity; status 'active') with NO units, snapshots,
# stage_sets or writes. `restore unit --unit photos` resolves the unit by
# name FIRST, so it has nothing to find.
#
# This arm USED TO BE an expected failure. The CTO settled #136 the other
# way: `import` registers a cartridge and that is all it was ever for, and
# rebuilding the catalog is its own command (`catalog rebuild`, arm (d)).
# So this now asserts the correct behaviour POSITIVELY — import succeeds,
# restore refuses, and the refusal names the unit — rather than logging a
# failure. An arm that merely fails cannot tell "still broken" from "broken
# in a new way".
dl_scenario_b_raw_and_import() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: new home (init only); tapectl restore raw-volume --to DIR --json (assert all_verified, DB-less); tapectl import --label VOL-A; assert restore unit --unit photos REFUSES and names the unit (import registers a cartridge, it does not rebuild a catalog — that is arm (d))"
        return 0
    fi
    local sd newhome; sd="$(dirname "$HOME_DIR")"; newhome="$sd/newhome-b"
    mkdir -p "$newhome"
    NEWHOME_TCTL "$newhome" init --operator "$OPERATOR" --no-escrow >"$sd/dl.b.init.txt" 2>&1 || { cat "$sd/dl.b.init.txt"; return 1; }

    local rawto="$sd/dl.b.raw" rawlog="$sd/dl.b.raw.json"
    NEWHOME_TCTL "$newhome" restore raw-volume --to "$rawto" --device "$TAPE_DEV" --json >"$rawlog" 2>"$rawlog.err"
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("mismatched_count", 1) == 0 and d.get("all_verified", False), d
' "$rawlog" || { echo "raw-volume did not verify cleanly:"; cat "$rawlog"; return 1; }

    NEWHOME_TCTL "$newhome" import --label VOL-A --generation LTO-6 >"$sd/dl.b.import.txt" 2>&1 \
        || { echo "top-level 'tapectl import' itself failed:"; cat "$sd/dl.b.import.txt"; return 1; }
    local to="$sd/dl.b.restore-photos"
    if NEWHOME_TCTL "$newhome" restore unit --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" \
        >"$sd/dl.b.restore.txt" 2>&1; then
        echo "'restore unit' SUCCEEDED after a bare 'import'. That means import is now"
        echo "doing more than registering a cartridge — which is the job #136 gave to"
        echo "'catalog rebuild' instead. Reconcile the two before relaxing this check."
        cat "$sd/dl.b.restore.txt"
        return 1
    fi
    grep -qi "photos" "$sd/dl.b.restore.txt" || {
        echo "restore refused, but without naming the unit it could not resolve:"
        cat "$sd/dl.b.restore.txt"
        return 1
    }
}

# (d) NEW empty home, NO backup, and the OPERATOR key: `catalog rebuild`
# reads the tape's envelopes and inserts the rows arm (b) proved `import`
# does not. This is the arm #136 exists for, and it is the one that proves
# the whole chain — envelope decrypt, manifest parse, row synthesis, and a
# real `restore unit` off real tape through the rebuilt catalog.
#
# The key copied in is the OPERATOR's, not alice's: the operator envelope's
# recipients are operator + escrow only, so a tenant key cannot open it.
# That refusal has its own unit test; what this arm proves is the path that
# works.
#
# Review finding 4 (2026-09-11): this arm used to `init --no-escrow` and
# never import one, so escrow state was simply absent from the scenario it
# exists to test. It now follows the DR procedure exactly — init WITHOUT a
# new escrow identity, then import the ORIGINAL escrow public key — and
# proves both halves: BEFORE the import the rebuilt rows cannot be covered
# and `audit` names the mismatch; AFTER it they read `yes` (the receipt rode
# the tape in catalog.db) and the mismatch is gone.
dl_scenario_d_catalog_rebuild() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: new home (init only); copy operator key; tapectl catalog rebuild --from-volume --key OPKEY --label VOL-A --json (assert units>0); tapectl catalog locate photos (assert VOL-A); tapectl restore unit --unit photos --from VOL-A (assert identical); tapectl volume verify VOL-A --full; key import --escrow the ORIGINAL public key and assert locate says yes + audit has no escrow findings; rebuild AGAIN (assert no_changes); then in a SECOND home with a plain init (replacement escrow identity) rebuild and assert locate says NO + audit names escrow_identity_mismatch exactly once with the import command"
        return 0
    fi
    local sd newhome; sd="$(dirname "$HOME_DIR")"; newhome="$sd/newhome-d"
    mkdir -p "$newhome"
    # `--no-escrow` here is the DR recipe, not a shortcut: a fresh `init`
    # would mint a replacement escrow identity, and every escrow check
    # compares against the REGISTERED recipient. The original is imported
    # below, after the rebuild, so the "forgot to import" state is measured
    # first.
    NEWHOME_TCTL "$newhome" init --operator "$OPERATOR" --no-escrow >"$sd/dl.d.init.txt" 2>&1 || { cat "$sd/dl.d.init.txt"; return 1; }

    local opkey="$sd/dl.d.operator.age.key"
    cp "$HOME_DIR/keys/$OPERATOR-primary.age.key" "$opkey" || {
        echo "no operator key at $HOME_DIR/keys/$OPERATOR-primary.age.key"; ls -1 "$HOME_DIR/keys"; return 1; }

    local rlog="$sd/dl.d.rebuild.json"
    NEWHOME_TCTL "$newhome" catalog rebuild --from-volume --device "$TAPE_DEV" \
        --key "$opkey" --label VOL-A --json >"$rlog" 2>"$rlog.err" || { cat "$rlog.err" "$rlog"; return 1; }
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert d["label"] == "VOL-A", d
assert d["volume_inserted"], d
assert d["units"] > 0, d
assert d["positions"] > 0, d
assert not d["no_changes"], d
assert d["units_without_tenant_envelope"] == [], d
' "$rlog" || { echo "catalog rebuild did not report a real rebuild:"; cat "$rlog"; return 1; }

    local logf="$sd/dl.d.locate.json"
    NEWHOME_TCTL "$newhome" catalog locate photos --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    grep -q "VOL-A" "$logf" || { echo "VOL-A not named in catalog locate photos after rebuild:"; cat "$logf"; return 1; }

    mkdir -p "$newhome/keys"
    cp -a "$HOME_DIR/keys/." "$newhome/keys/" || { echo "could not copy keys into the new home"; return 1; }
    local to="$sd/dl.d.restore-photos"
    NEWHOME_TCTL "$newhome" restore unit --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" \
        >"$sd/dl.d.restore.txt" 2>&1 || { cat "$sd/dl.d.restore.txt"; return 1; }
    assert_identical "$SRC/photos" "$to"

    # The command `catalog rebuild` itself tells the operator to run next.
    # It was never exercised until now: the rebuilt rows must be good enough
    # for a full integrity chain walk, and `volumes.backend_name` must name a
    # backend that resolves rather than an invented one.
    NEWHOME_TCTL "$newhome" volume verify VOL-A --device "$TAPE_DEV" --full \
        >"$sd/dl.d.verify.txt" 2>&1 || {
        echo "'volume verify' — which the rebuild's own output tells the operator to run — failed:"
        cat "$sd/dl.d.verify.txt"; return 1; }

    # --- Escrow, review finding 4. Two homes, because ADR-0005 permits
    # exactly one escrow identity and NO command replaces it: the state an
    # operator reaches by running a plain `init` cannot be repaired in place,
    # only avoided. So the happy path (this home) imports the ORIGINAL escrow
    # public key into a home that never minted one, and the mistake is
    # measured in a second home below.
    NEWHOME_TCTL "$newhome" key import --escrow "$HOME_DIR/keys/$OPERATOR-escrow.age.pub" >"$sd/dl.d.escrow-import.txt" 2>&1 \
        || { echo "could not import the original escrow key into the rebuilt home:"; cat "$sd/dl.d.escrow-import.txt"; return 1; }
    local llog2="$sd/dl.d.locate-after.json"
    NEWHOME_TCTL "$newhome" catalog locate photos --json >"$llog2" 2>"$llog2.err" || { cat "$llog2.err" "$llog2"; return 1; }
    grep -q '"escrow": *"yes"' "$llog2" || { echo "with the ORIGINAL escrow key registered, the rebuilt rows must be covered — the receipt rode the tape in catalog.db:"; cat "$llog2"; return 1; }
    local alog2="$sd/dl.d.audit-after.json"
    NEWHOME_TCTL "$newhome" audit --json >"$alog2" 2>"$alog2.err" || true   # advisory exit codes are fine
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
checks = [f["check"] for f in d.get("findings", [])]
assert "escrow_identity_mismatch" not in checks, ("mismatch reported with the original key registered", checks)
assert "escrow_coverage" not in checks, ("coverage warning with the original key registered", checks)
' "$alog2" || { cat "$alog2"; return 1; }

    # Idempotence, against a real tape rather than a MemStore: the second
    # pass must change nothing. This is what makes it safe to walk a shelf.
    local rlog2="$sd/dl.d.rebuild2.json"
    NEWHOME_TCTL "$newhome" catalog rebuild --from-volume --device "$TAPE_DEV" \
        --key "$opkey" --label VOL-A --json >"$rlog2" 2>"$rlog2.err" || { cat "$rlog2.err" "$rlog2"; return 1; }
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert d["no_changes"], d
' "$rlog2" || { echo "a second catalog rebuild was not a no-op:"; cat "$rlog2"; return 1; }

    # --- The mistake, measured: a plain `init` mints a REPLACEMENT escrow
    # identity. Every receipt on the tape names the original, so the rebuilt
    # rows cannot be covered, and audit must say so ONCE and name the key —
    # not N times without ever saying why.
    local wrong="$sd/newhome-d-wronginit"
    mkdir -p "$wrong"
    NEWHOME_TCTL "$wrong" init --operator "$OPERATOR" >"$sd/dl.d2.init.txt" 2>&1 || { cat "$sd/dl.d2.init.txt"; return 1; }
    local rlog3="$sd/dl.d2.rebuild.json"
    NEWHOME_TCTL "$wrong" catalog rebuild --from-volume --device "$TAPE_DEV" \
        --key "$opkey" --label VOL-A --json >"$rlog3" 2>"$rlog3.err" || { cat "$rlog3.err" "$rlog3"; return 1; }
    local llog3="$sd/dl.d2.locate.json"
    NEWHOME_TCTL "$wrong" catalog locate photos --json >"$llog3" 2>"$llog3.err" || { cat "$llog3.err" "$llog3"; return 1; }
    grep -q '"escrow": *"NO"' "$llog3" || { echo "with a replacement escrow identity registered, locate must say NO:"; cat "$llog3"; return 1; }
    local alog3="$sd/dl.d2.audit.json"
    NEWHOME_TCTL "$wrong" audit --json >"$alog3" 2>"$alog3.err" || true
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
hits = [f for f in d.get("findings", []) if f["check"] == "escrow_identity_mismatch"]
assert len(hits) == 1, ("expected exactly one escrow_identity_mismatch", [f["check"] for f in d.get("findings", [])])
action = hits[0]["action"]
# The REMEDY ADR-0005 allows, and the one it forbids. This used to assert
# `key import --escrow age1`, which commit 9b42828 correctly stopped
# recommending: this home already minted a replacement escrow identity at
# `init`, so importing over it would REPLACE a registered identity, and
# ADR-0005 says no command does that. The recipe is a fresh home adopting the
# original key at init (#139). Asserting both halves pins the ADR rather than
# whichever sentence currently expresses it -- a string pin is what let this
# check go stale unnoticed in the first place.
assert "init --escrow-public-key age1" in action, action
assert "key import --escrow" not in action, (
    "audit recommends a key import that ADR-0005 refuses", action)
' "$alog3" || { echo "audit did not diagnose the replaced escrow identity once, naming the ADR-0005 recipe:"; cat "$alog3"; return 1; }
}

# (c) The pure heir path: a directory holding ONLY RESTORE.sh and the
# tenant's key file, HOME pointed at an empty directory so nothing of
# tapectl's own state (~/.tapectl, $HOME_DIR, keys elsewhere) is reachable.
dl_scenario_c_pure_heir() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: dd RESTORE.sh + copy alice's key into an EMPTY dir; HOME=<empty> ./RESTORE.sh --restore --unit photos --key ./alice-primary.age.key --to DIR"
        return 0
    fi
    local sd heir emptyhome to
    sd="$(dirname "$HOME_DIR")"; heir="$sd/pure-heir"; emptyhome="$sd/pure-heir-emptyhome"; to="$sd/dl.c.restore-photos"
    mkdir -p "$heir" "$emptyhome"
    devcmd mt -f "$TAPE_DEV" rewind || return 1
    devcmd mt -f "$TAPE_DEV" setblk 524288 || return 1
    devcmd mt -f "$TAPE_DEV" fsf 2 || return 1
    dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' >"$heir/RESTORE.sh"
    chmod +x "$heir/RESTORE.sh"
    cp "$HOME_DIR/keys/alice-primary.age.key" "$heir/alice-primary.age.key" || return 1
    (cd "$heir" && HOME="$emptyhome" TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit photos --key ./alice-primary.age.key --to "$to") \
        >"$sd/dl.c.restore.txt" 2>&1 || { cat "$sd/dl.c.restore.txt"; return 1; }
    assert_identical "$SRC/photos" "$to"
}

scenario_db_loss() {
    check dl.setup     bootstrap_archive_v1 VOL-A
    check dl.backup    dl_backup
    check dl.scenario_a dl_scenario_a_db_import
    check dl.scenario_b dl_scenario_b_raw_and_import
    check dl.scenario_c dl_scenario_c_pure_heir
    check dl.scenario_d dl_scenario_d_catalog_rebuild
}
# ============================================================
# Scenario: escrow-ordering (issue #115 regression)
# ============================================================
# The exact defect docs/lto6-session-journal-2026-09-10.md's Phase 4/5
# found on real hardware: staging BEFORE an escrow recipient is registered
# produces slices the escrow key can never decrypt, and pre-write
# validation only checked the REGISTRY, not the slices. The fix (issue
# #115, landing in a parallel branch) is to refuse `stage create` itself
# when no escrow recipient exists yet. This scenario asserts the
# POST-FIX behavior; until #115 lands, eo.stage_before_escrow_refused is
# expected to FAIL (stage create currently succeeds) — that is not a bug
# in this suite, see docs/lifecycle-suite.md.
eo_init()      { bootstrap_config; }
eo_tenants()   { TCTL tenant add alice && TCTL tenant add bob; }
eo_units() {
    make_source "$SRC/unitA" "plain" "$CANARY" || return 1
    make_source "$SRC/unitB" "unicode" || return 1
    TCTL unit init "$SRC/unitA" --tenant alice --name unitA \
    && TCTL unit init "$SRC/unitB" --tenant bob --name unitB
}
eo_snapshots() { TCTL snapshot create unitA && TCTL snapshot create unitB; }

eo_stage_before_escrow_refused() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: tapectl stage create unitA (expect refusal containing 'no escrow recipient is registered'); tapectl stage list --json (expect [])"
        return 0
    fi
    local out rc
    out="$(TCTL stage create unitA 2>&1)"; rc=$?
    if [ "$rc" -eq 0 ]; then
        echo "stage create unexpectedly SUCCEEDED with no escrow recipient registered (issue #115 not yet fixed on this branch):"
        echo "$out"
        return 1
    fi
    if ! echo "$out" | grep -qi "no escrow recipient is registered"; then
        echo "refused, but not with the expected message ('no escrow recipient is registered'):"
        echo "$out"
        return 1
    fi
    local listing
    listing="$(TCTL stage list --json 2>&1)"
    echo "$listing" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert isinstance(d, list) and len(d) == 0, d
' || { echo "stage list not empty after a refused stage create: $listing"; return 1; }
}

# Pre-existing behaviour (not part of #115): key rotate already refuses
# without an escrow recipient (src/cli/key.rs "key rotate refuses: no
# escrow recipient is registered").
eo_rotate_before_escrow_refused() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl key rotate --tenant alice (expect refusal: 'no escrow recipient is registered')"; return 0; }
    local out rc
    out="$(TCTL key rotate --tenant alice 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "key rotate unexpectedly succeeded with no escrow recipient: $out"; return 1; }
    echo "$out" | grep -qi "no escrow recipient is registered" || { echo "unexpected refusal text: $out"; return 1; }
}

eo_escrow()       { TCTL key generate --escrow; }
eo_stage()        { TCTL stage create unitA && TCTL stage create unitB; }
eo_write() {
    next_tape VOL-EO || return 1
    vinit VOL-EO && TCTL volume write VOL-EO --device "$TAPE_DEV"
}

scenario_escrow_ordering() {
    check eo.init                          eo_init
    check eo.tenants                       eo_tenants
    check eo.units                         eo_units
    check eo.snapshots                     eo_snapshots
    check eo.stage_before_escrow_refused   eo_stage_before_escrow_refused
    check eo.rotate_before_escrow_refused  eo_rotate_before_escrow_refused
    check eo.escrow                        eo_escrow
    capture_escrow_secret eo.escrow
    check eo.stage                         eo_stage
    check eo.write                         eo_write

    restore_matrix VOL-EO unitA alice "$SRC/unitA" eo-unitA bob
    restore_matrix VOL-EO unitB bob   "$SRC/unitB" eo-unitB alice
}
# ============================================================
# Scenario: restore-file-and-catalog
# ============================================================
# After first-year: catalog browsing cross-checked against `find` on the
# real source, single-file restore for the awkward cases (nested unicode,
# 0-byte, symlink), and `unit check-integrity` clean vs. after a mutation.
#
# File/link entries are the rows whose raw `is_directory` is false.
#
# This used to discriminate on a literal "d " prefix baked into the JSON
# `path`, because it once was: `catalog ls --json` emitted a display marker
# inside a raw fact. Issue #236 finding 5 removed it (commit f7f2431) --
# correctly, per the C2b rule that a `--json` value is the raw fact and
# `display_with` renders it for the table only -- and this check silently
# started counting directories as files, because nothing starts with "d "
# any more. Measured 2026-09-17: 10 counted against 9 found, the difference
# being the one directory.
#
# So it now reads the boolean, and asserts the prefix is really gone rather
# than merely not matching -- a discriminator that matches nothing looks
# exactly like a discriminator that is stale.
rfc_catalog_ls_matches_find() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl catalog ls photos --json; compare non-directory entry count to find \$SRC/photos -type f -o -type l"; return 0; }
    local logf="$RUN/log-rfc.catalog_ls.json"
    TCTL catalog ls photos --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    local expect actual
    expect="$(find "$SRC/photos" \( -type f -o -type l \) | wc -l)"
    actual="$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert not any(r.get("path", "").startswith("d ") for r in d), (
    "a display marker is baked into the raw JSON path again (issue #236 finding 5)", d)
print(sum(1 for r in d if not r.get("is_directory")))
' "$logf")" || { echo "catalog ls --json is not the shape this check reads:"; cat "$logf"; return 1; }
    [ "$expect" = "$actual" ] || { echo "catalog ls photos: expected $expect file/link entries (find), got $actual (catalog)"; cat "$logf"; return 1; }
}

rfc_catalog_search() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl catalog search target --json (assert target.txt found)"; return 0; }
    local logf="$RUN/log-rfc.search.json"
    TCTL catalog search target --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    grep -q "target" "$logf" || { echo "catalog search 'target' found nothing:"; cat "$logf"; return 1; }
}

rfc_catalog_locate() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl catalog locate photos --json (assert VOL-A named)"; return 0; }
    local logf="$RUN/log-rfc.locate.json"
    TCTL catalog locate photos --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    grep -q "VOL-A" "$logf" || { echo "VOL-A not named in catalog locate photos:"; cat "$logf"; return 1; }
}

# Deliberately not cross-checked against `find` (catalog stats is
# whole-database, not per-unit — an exact cross-check would need to sum
# every unit's file count and would be more fragile than informative).
rfc_catalog_stats() { TCTL catalog stats; }

rfc_restore_file_nested_unicode() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl restore file --file nested/déjà-vu.txt --unit photos --from VOL-A --to DIR; assert sha256 match"; return 0; }
    local to="$RUN/rfc-file-nested"
    TCTL restore file --file "nested/déjà-vu.txt" --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" || return 1
    local expect actual
    expect="$(sha256sum "$SRC/photos/nested/déjà-vu.txt" | awk '{print $1}')"
    actual="$(sha256sum "$to/déjà-vu.txt" 2>/dev/null | awk '{print $1}')"
    [ -n "$actual" ] && [ "$expect" = "$actual" ] || { echo "nested unicode file mismatch or missing at $to/déjà-vu.txt"; return 1; }
}

rfc_restore_file_empty() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl restore file --file empty.bin --unit photos --from VOL-A --to DIR; assert present and 0 bytes"; return 0; }
    local to="$RUN/rfc-file-empty"
    TCTL restore file --file empty.bin --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" || return 1
    [ -f "$to/empty.bin" ] || { echo "empty.bin missing after restore file"; return 1; }
    [ ! -s "$to/empty.bin" ] || { echo "empty.bin is not empty after restore"; return 1; }
}

# Decided from src/volume/restore.rs:369 (`restore_file`'s single-file path
# copies via `fs::copy(&source_file, &dest)` after a full temp-dir unit
# restore) — `fs::copy` opens the source path and copies bytes; on a
# symlink source that FOLLOWS the link, so the destination is always a
# plain file, never a symlink. This check is therefore expected to FAIL
# today — a real gap this suite surfaces (parallel to raw-volume's `import`
# gap in db-loss), not a mistake in the assertion.
rfc_restore_file_symlink() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl restore file --file link-ok --unit photos --from VOL-A --to DIR; assert restored AS A SYMLINK"; return 0; }
    local to="$RUN/rfc-file-link"
    TCTL restore file --file link-ok --unit photos --from VOL-A --to "$to" --device "$TAPE_DEV" || return 1
    [ -L "$to/link-ok" ] || {
        echo "restore file dereferenced a symlink: link-ok came back as a plain file."
        echo "  'restore unit' preserves it, so the two commands disagree about the same"
        echo "  archive entry. place_restored_entry (src/volume/restore.rs) is the fix site."
        return 1
    }
}

rfc_check_integrity_clean() { TCTL unit check-integrity photos; }

rfc_check_integrity_after_modify() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: mutate photos (modify); tapectl unit check-integrity photos --json (assert bitrot+size_mismatch > 0, naming the changed file)"; return 0; }
    mutate_source "$SRC/photos" "$SEED" modify || return 1
    local logf="$RUN/log-rfc.integrity_modify.json"
    TCTL unit check-integrity photos --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert (d.get("bitrot", 0) + d.get("size_mismatch", 0)) > 0, d
' "$logf" || { cat "$logf"; return 1; }
}

scenario_restore_file_and_catalog() {
    check rfc.setup                        bootstrap_archive_v1 VOL-A
    check rfc.catalog_ls_matches_find      rfc_catalog_ls_matches_find
    check rfc.catalog_search               rfc_catalog_search
    check rfc.catalog_locate               rfc_catalog_locate
    check rfc.catalog_stats                rfc_catalog_stats
    check rfc.restore_file_nested_unicode  rfc_restore_file_nested_unicode
    check rfc.restore_file_empty           rfc_restore_file_empty
    check rfc.restore_file_symlink         rfc_restore_file_symlink
    check rfc.check_integrity_clean        rfc_check_integrity_clean
    check rfc.check_integrity_after_modify rfc_check_integrity_after_modify
}
# ============================================================
# Scenario: quick-archive
# ============================================================
# The one-shot create+stage+write flow (src/cli/operations.rs:1536
# quick_archive: unit init -> snapshot -> stage -> write). `quick-archive`
# has no `--name` override, so the unit name is whatever
# `unit::auto_name_from_path` derives — every Normal path component
# joined with "/" (src/unit/mod.rs:221), which for a path under $RUN is
# the whole absolute path minus the leading "/". Captured from the run's
# own log rather than guessed, so this scenario works regardless of where
# $OUT_DIR happens to be.
qa_tenants() { TCTL tenant add alice && TCTL tenant add bob; }

qa_escrow() { TCTL key generate --escrow; }

qa_run() {
    make_source "$SRC/qa-unit" "plain+links" "$CANARY" || return 1
    next_tape VOL-Q || return 1
    # `quick-archive --volume L` writes to an EXISTING volume; it does not
    # initialize one. Without this the whole scenario died with "volume not
    # found: VOL-Q" on every drive, single- or multi-cartridge — which #128 read
    # as a cartridge-reuse limitation. vinit supplies --force when a cartridge
    # is being reused.
    vinit VOL-Q || return 1
    TCTL quick-archive --tenant alice --volume VOL-Q "$SRC/qa-unit" --device "$TAPE_DEV"
}

scenario_quick_archive() {
    check qa.init    bootstrap_config
    check qa.tenants qa_tenants
    check qa.escrow  qa_escrow
    capture_escrow_secret qa.escrow
    check qa.run     qa_run

    local qa_unit_name
    if [ "$DRY_RUN" = 1 ]; then
        qa_unit_name="<auto-named-from-path>"
    else
        qa_unit_name="$(grep -oE 'unit "[^"]+" initialized' "$RUN/log-qa.run.txt" 2>/dev/null \
            | head -1 | sed -E 's/unit "([^"]+)".*/\1/')"
        [ -n "$qa_unit_name" ] || { echo "qa.unit_name: could not capture the auto-named unit from qa.run's log"; qa_unit_name="qa-unit"; }
    fi

    restore_matrix VOL-Q "$qa_unit_name" alice "$SRC/qa-unit" qa-unit bob
}
# ============================================================
# Scenario: collection
# ============================================================
# A folder-per-unit collection: 4 pre-existing folders, `collection sync`
# registers them, `collection run` writes one batch, then a 5th folder
# appears and a 3rd is renamed — `collection sync` must resolve the rename
# by dotfile uuid (unit count goes to 5, not 6).
#
# Config keys read from src/config.rs's CollectionConfig (`[[collections]]`
# name/root/tenant/unit_depth/exclude/archive_set/dotfiles): unit names are
# "{collection-name}/{relative_path}" (the struct's own doc comment), so
# with name="media" and unit_depth=1 the four folders become
# "media/alpha", "media/bravo", "media/charlie", "media/delta" —
# deterministic, no log-scraping needed (unlike quick-archive's
# path-derived name).
col_setup() {
    bootstrap_config || return 1
    TCTL tenant add alice || return 1
    # ADR-0005: staging refuses without an escrow recipient, so this is a
    # precondition of `collection run`, not optional setup. quick-archive has
    # always had this step; collection did not, and col.run_batch failed with
    # "no escrow recipient is registered" on ANY drive — which a
    # single-cartridge SKIP would have hidden as a media limitation.
    if [ "$DRY_RUN" = 1 ]; then
        TCTL key generate --escrow
    else
        TCTL key generate --escrow >"$RUN/log-col_escrow.txt" 2>&1 || return 1
        capture_escrow_secret col_escrow
    fi
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: create 4 folders under \$SRC/col-root; append [[collections]] (name=media, root=\$SRC/col-root, tenant=alice, unit_depth=1) to \$CFG"
        return 0
    fi
    mkdir -p "$SRC/col-root"
    make_source "$SRC/col-root/alpha" "plain" || return 1
    make_source "$SRC/col-root/bravo" "unicode" || return 1
    make_source "$SRC/col-root/charlie" "deep" || return 1
    make_source "$SRC/col-root/delta" "links" || return 1
    python3 - "$CFG" "$SRC/col-root" <<'PY'
import re
import sys
cfg, root = sys.argv[1:3]
t = open(cfg).read()
# `Config::default` serializes an empty `collections = []` at the document
# root; TOML forbids that alongside a `[[collections]]` array-of-tables, so
# strip the empty key first (mirrors how the backend block is added).
t = re.sub(r'(?m)^collections *= *\[\] *\n', "", t)
t += f'''
[[collections]]
name = "media"
root = "{root}"
tenant = "alice"
unit_depth = 1
'''
open(cfg, "w").write(t)
PY
}

col_sync_registers_four() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl collection sync --json; tapectl unit list --tenant alice --json (assert 4 units)"; return 0; }
    local logf="$RUN/log-col.sync.json"
    TCTL collection sync --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    local n
    n="$(TCTL unit list --tenant alice --json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' 2>/dev/null || echo 0)"
    [ "$n" = 4 ] || { echo "expected 4 units after collection sync, got $n:"; cat "$logf"; return 1; }
}

col_status() { TCTL collection status; }
col_plan()   { TCTL collection plan; }

col_run_batch() {
    next_tape VOL-COL1 || return 1
    vinit VOL-COL1 || return 1
    TCTL collection run --collection media --batch 0 --label VOL-COL1 --device "$TAPE_DEV"
}

# Rename-by-uuid: add a 5th folder AND rename the 3rd, in one sync. If the
# rename were resolved as delete+add instead, the count would be 6 (4
# original minus 1 vanished... no: 4 existing + 1 stray "new" for the
# renamed path + 1 genuinely new = 6); resolved correctly by dotfile uuid,
# the renamed folder keeps its unit identity and the count is 5.
col_add_and_rename() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: make_source \$SRC/col-root/echo; mv charlie -> charlie-renamed; tapectl collection sync (assert unit count is 5, not 6)"
        return 0
    fi
    make_source "$SRC/col-root/echo" "plain" || return 1
    mv "$SRC/col-root/charlie" "$SRC/col-root/charlie-renamed" || return 1
    TCTL collection sync || return 1
    local n
    n="$(TCTL unit list --tenant alice --json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' 2>/dev/null || echo 0)"
    [ "$n" = 5 ] || { echo "expected 5 units after adding a folder + renaming another (rename resolved by dotfile uuid), got $n"; return 1; }
}

# The 5th folder added by col_add_and_rename must be registered AND counted as
# pending.
#
# This used to grep `collection status --json` for "echo". That output carries
# only aggregate per-collection counts — it has never contained a unit name in
# either the json or the text form — so the check could not pass whatever the
# tool did. Assert each half against a command that actually reports it:
# `unit list` for registration, the pending count for state.
col_status_shows_new_pending() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: unit list names media/echo; collection status --json reports 5 pending"; return 0; }
    local logf="$RUN/log-col.status2.json" units="$RUN/log-col.status2.units.txt"
    TCTL unit list >"$units" 2>&1 || { cat "$units"; return 1; }
    grep -q 'media/echo' "$units" || {
        echo "media/echo was not registered by collection sync:"; cat "$units"; return 1; }
    local n; n="$(grep -cE 'media/[a-z]+' "$units")"
    [ "$n" -eq 5 ] || { echo "expected 5 units in collection media after add+rename, got $n:"; cat "$units"; return 1; }
    TCTL collection status --json >"$logf" 2>"$logf.err" || { cat "$logf.err" "$logf"; return 1; }
    # `pending` moves as units get staged/written, so assert the collection is
    # tracked and still short of full coverage rather than pinning a count that
    # legitimately varies between a single- and multi-cartridge run.
    python3 - "$logf" <<'PY2' || { echo "collection status did not report media as under-copied:"; cat "$logf"; return 1; }
import json, sys
d = json.load(open(sys.argv[1]))
media = next((c for c in d if c.get("collection") == "media"), None)
assert media is not None, d
assert media.get("under_copied", 0) >= 1, media
PY2
}

scenario_collection() {
    check col.setup                 col_setup
    check col.sync_registers_four   col_sync_registers_four
    check col.status                col_status
    check col.plan                  col_plan
    check col.run_batch             col_run_batch

    restore_matrix VOL-COL1 media/alpha alice "$SRC/col-root/alpha" col-alpha
    restore_matrix VOL-COL1 media/bravo alice "$SRC/col-root/bravo" col-bravo

    check col.add_and_rename        col_add_and_rename
    check col.status_shows_new_pending col_status_shows_new_pending
}
# ============================================================
# Scenario: permute
# ============================================================
# A seeded random walk over the command surface, starting from first-year.
# The op sequence is generated ONCE, deterministically, from --seed via
# python's random.Random (same reproducibility contract as mutate_source):
# same seed + same --steps => the exact same walk, printed to REPORT.md so
# a failure can be replayed by re-running with the same --seed.
#
# Op pool and weights are the task spec's table verbatim. Preconditioned
# ops (write-next-volume, restore-latest-and-diff, mark-reclaimable-oldest)
# SKIP visibly with a reason when their precondition isn't met, rather than
# being excluded from the draw — an ineligible draw still consumes a step
# and is logged, exactly as the spec asks.
pm_generate_sequence() {
    python3 -c '
import random, sys
seed, steps = int(sys.argv[1]), int(sys.argv[2])
pool = (
    ["mutate:add"] * 3 + ["mutate:modify"] * 3 + ["mutate:delete"] * 1 + ["mutate:rename"] * 1
    + ["snapshot"] * 3 + ["stage"] * 3 + ["write-next-volume"] * 2
    + ["restore-latest-and-diff"] * 2 + ["key-rotate"] * 1 + ["report-random"] * 2
    + ["audit"] * 2 + ["staging-clean"] * 1 + ["db-fsck"] * 1
    + ["mark-reclaimable-oldest"] * 1 + ["check-integrity"] * 1
)
units = ["photos", "docs", "big"]
reports = ["summary", "fire-risk", "copies", "tape-only", "dirty", "pending",
           "verify-status", "health", "capacity", "age", "events",
           "compaction-candidates", "supersedable"]
rng = random.Random(seed)
for _ in range(steps):
    op = rng.choice(pool)
    if op in ("mutate:add", "mutate:modify", "mutate:delete", "mutate:rename",
              "snapshot", "stage", "check-integrity"):
        print(f"{op} {rng.choice(units)}")
    elif op == "report-random":
        print(f"report-random {rng.choice(reports)}")
    else:
        print(op)
' "$SEED" "$STEPS"
}

# Per-scenario walk state. PM_SNAPSHOT_COUNT starts at 1 for every
# first-year unit (bootstrap_archive_v1 already took v1). PM_WRITTEN
# tracks labels this walk has written, in order (last = most recent).
declare -A PM_SNAPSHOT_COUNT
PM_WRITTEN=()
PM_VOL_SEQ=0
PM_CHECK_NAME=""

pm_skip_never_written() { skip "pm-final-$1.unit" "unit $1 never ended up on any volume this walk"; return $?; }
# Issue #282: the end-of-walk matrix's own skip, for a unit whose latest
# copy is on a cartridge this run cannot put back in the drive.
pm_skip_unreachable() { skip "pm-final-$1.unit" "$2"; return $?; }

pm_op_mutate() { mutate_source "$SRC/$2" "$SEED" "${1#mutate:}"; }
pm_op_snapshot() {
    TCTL snapshot create "$1" || return 1
    PM_SNAPSHOT_COUNT["$1"]=$(( ${PM_SNAPSHOT_COUNT["$1"]:-1} + 1 ))
}
# `stage create` requires an unstaged snapshot. The walk picks ops at random
# and can land on `stage` for a unit whose snapshot is already staged, where
# tapectl correctly refuses. That is a precondition the walk failed to meet, not
# a defect — SKIP it, the way mark-reclaimable-oldest already does.
pm_op_stage() {
    local out rc
    out="$(TCTL stage create "$1" 2>&1)"; rc=$?
    printf '%s\n' "$out"
    # Match the refusal itself rather than pre-checking snapshot state: a
    # pre-check that silently stopped matching would skip EVERY stage step and
    # quietly delete this op's coverage. This way an unexpected failure still
    # FAILs, and only the documented precondition becomes a SKIP.
    if [ "$rc" -ne 0 ] && grep -q 'no unstaged snapshot for unit' <<<"$out"; then
        skip "$PM_CHECK_NAME" "unit \"$1\" has no unstaged snapshot — the walk picked stage with its precondition unmet"
        return $?
    fi
    # The source drifted after the snapshot was taken (a mutate:rename/delete
    # landed in between), so the manifest names a file that is gone and staging
    # refuses — correct behaviour (design 2.13), and a precondition the random
    # walk failed to meet. Do what a real operator does: re-snapshot and stage
    # again. Retried ONCE, and a second failure still FAILs, so this cannot
    # paper over a genuine staging defect.
    if [ "$rc" -ne 0 ] && grep -q 'source file missing' <<<"$out"; then
        echo "pm_op_stage: snapshot went stale (source drifted) — re-running snapshot create and retrying"
        # Via pm_op_snapshot, not `TCTL snapshot create` directly: it also bumps
        # PM_SNAPSHOT_COUNT, which mark-reclaimable-oldest gates on. Calling
        # tapectl straight here would undercount versions and make that op SKIP
        # a unit that really does have >=2.
        pm_op_snapshot "$1" || return 1
        out="$(TCTL stage create "$1" 2>&1)"; rc=$?
        printf '%s\n' "$out"
    fi
    [ "$rc" -eq 0 ] && pm_capture_staged "$1"
    return "$rc"
}

# The restore baseline, captured when a unit is STAGED.
#
# It used to be `cp -a "$SRC"` at write time, which is the wrong instant: a
# volume is written from a stage set built earlier, so any mutation landing
# between stage and write made the "pristine" copy disagree with the tape and
# restore-latest-and-diff failed on content the tape was never supposed to have.
# The tape holds what was staged, so the baseline must be the source as it was
# when staged.
pm_capture_staged() { # pm_capture_staged <unit>
    local sd; sd="$(dirname "$HOME_DIR")"
    mkdir -p "$sd/pm-staged"
    rm -rf "${sd:?}/pm-staged/${1:?}"
    cp -a "$SRC/$1" "$sd/pm-staged/$1"
}
pm_op_check_integrity() { TCTL unit check-integrity "$1"; }
pm_op_key_rotate()      { TCTL key rotate --tenant alice; }
pm_op_audit()           { local rc; TCTL audit; rc=$?; [ "$rc" -le 2 ]; }
# `--force` because this is a RANDOMISED walk: whether the staged data is
# below `min_copies` when this operation fires depends on where the RNG
# placed the preceding writes, so a bare `staging clean` passes or fails by
# seed after issue #244. A seed-dependent check proves nothing on the seed
# it passes and blocks the run on the seed it does not -- the same trap the
# `permute` copy_count assertion already hit. The walk is not testing
# #244's gate (`tor.staging_clean` is); it is testing that a long random
# sequence of operations leaves the archive restorable, and "release
# whatever is staged" is the operator intent at this step.
pm_op_staging_clean()   { TCTL staging clean --force; }
pm_op_db_fsck()         { TCTL db fsck; }
pm_op_report_random()   { TCTL report "$1"; }

pm_op_write_next_volume() {
    local n; n="$(pending_count)"
    if [ "$n" -le 0 ]; then skip "$PM_CHECK_NAME" "nothing staged"; return $?; fi
    PM_VOL_SEQ=$((PM_VOL_SEQ + 1))
    local label="VOL-PM$PM_VOL_SEQ" sd; sd="$(dirname "$HOME_DIR")"
    next_tape "$label" || return 1
    vinit "$label" || return 1
    TCTL volume write "$label" --device "$TAPE_DEV" || return 1
    # Freeze the baseline for THIS volume from what was staged, not from the
    # live source (see pm_capture_staged).
    mkdir -p "$sd/pm-snapshot-$label"
    cp -a "$sd/pm-staged/." "$sd/pm-snapshot-$label/"
    PM_WRITTEN+=("$label")
}

pm_op_restore_latest_and_diff() {
    if [ "${#PM_WRITTEN[@]}" -eq 0 ]; then skip "$PM_CHECK_NAME" "no volume written yet this walk"; return $?; fi
    local label="${PM_WRITTEN[-1]}" sd u to
    sd="$(dirname "$HOME_DIR")"
    # $label is the MOST RECENT write, which in single-cartridge mode is exactly
    # what is still in the drive — there is nothing to reload. load_volume_tape
    # refuses unconditionally under --single-cartridge (it exists to fetch
    # SUPERSEDED cartridges, which really are gone), so calling it here failed a
    # check that should simply proceed against the loaded tape.
    if [ "$SINGLE_CARTRIDGE" != 1 ]; then
        load_volume_tape "$label" || return 1
    fi
    for u in photos docs big; do
        to="$sd/pm-restore-$label-$u"
        if TCTL restore unit --unit "$u" --from "$label" --to "$to" --device "$TAPE_DEV" >/dev/null 2>&1; then
            assert_identical "$sd/pm-snapshot-$label/$u" "$to" || return 1
        fi
        # A unit simply not present on this particular write is expected in
        # a random walk (not every write bundles every unit) — not a
        # failure, just nothing to check for that unit this time.
    done
}

# "≥2 written versions" tracked via PM_SNAPSHOT_COUNT rather than a DB
# query, kept in sync by pm_op_snapshot above. Both refusal and success are
# logged as PASS (this op's contract is "the exit code matches the
# documented precondition rule", and the rule itself — enforced
# server-side by `snapshot mark-reclaimable` — is what is under test, not
# guessed here).
pm_op_mark_reclaimable_oldest() {
    local candidate="" u
    for u in photos docs big; do
        if [ "${PM_SNAPSHOT_COUNT[$u]:-1}" -ge 2 ]; then candidate="$u"; break; fi
    done
    if [ -z "$candidate" ]; then skip "$PM_CHECK_NAME" "no unit has >=2 snapshot versions yet"; return $?; fi
    TCTL snapshot mark-reclaimable --version 1 "$candidate" 2>&1
    return 0
}

pm_run_step() { # pm_run_step <line...>
    local op="$1"; shift
    case "$op" in
        mutate:add|mutate:modify|mutate:delete|mutate:rename) pm_op_mutate "$op" "$1" ;;
        snapshot)                 pm_op_snapshot "$1" ;;
        stage)                    pm_op_stage "$1" ;;
        check-integrity)          pm_op_check_integrity "$1" ;;
        report-random)            pm_op_report_random "$1" ;;
        write-next-volume)        pm_op_write_next_volume ;;
        restore-latest-and-diff)  pm_op_restore_latest_and_diff ;;
        key-rotate)               pm_op_key_rotate ;;
        audit)                    pm_op_audit ;;
        staging-clean)            pm_op_staging_clean ;;
        db-fsck)                  pm_op_db_fsck ;;
        mark-reclaimable-oldest)  pm_op_mark_reclaimable_oldest ;;
        *) echo "permute: unknown op \"$op\""; return 1 ;;
    esac
}

# After EVERY step: db fsck must be clean, and audit must not exit 2
# unless the step just applied was a mutate:* (a fresh mutation
# legitimately trips a dirty/under-copied finding until re-archived).
pm_post_step_invariants() { # pm_post_step_invariants <op>
    local op="$1" fsck_json
    fsck_json="$(TCTL db fsck --json 2>&1)"
    echo "$fsck_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d.get("integrity_ok"), d
' || { echo "db fsck not clean after op \"$op\": $fsck_json"; return 1; }

    # Per-step, not a single overwritten file: the old shared path meant the
    # only surviving json was the LAST step's, so a failing step's evidence was
    # gone by the time anyone read the report. And stderr must NOT be folded in
    # (`2>&1`) — one stray line makes the file unparseable, which audit_passes
    # reports as an undeclared violation, blaming the policy for a plumbing
    # problem.
    local audit_rc pm_audit_json="$RUN/log-pm.${PM_STEP_TAG:-step}.audit.json"
    TCTL audit --json >"$pm_audit_json" 2>"$RUN/log-pm.${PM_STEP_TAG:-step}.audit.stderr"; audit_rc=$?
    case "$op" in
        mutate:*) audit_passes "$audit_rc" "$pm_audit_json" "${PM_ALLOWED_VIOLATIONS[@]}" || [ "$audit_rc" -le 2 ] || { echo "audit exited $audit_rc (>2) after \"$op\""; return 1; } ;;
        *)        audit_passes "$audit_rc" "$pm_audit_json" "${PM_ALLOWED_VIOLATIONS[@]}" || {
                      echo "audit exited $audit_rc after \"$op\"; audit_passes rejected it. json:"
                      cat "$pm_audit_json"
                      echo "--- stderr ---"; cat "$RUN/log-pm.${PM_STEP_TAG:-step}.audit.stderr"
                      return 1; } ;;
    esac
}

# ---------- pm_final_copy_count_is_honest (issue #203, rewritten #288) ----------
# The end-of-walk assertion that per-step copy_count checking was never able to
# be. It does NOT ask "is audit quiet" -- the walk cannot control how many
# copies the RNG gave it.
#
# WHAT THIS USED TO DO, AND WHY IT COULD NOT WORK (issue #288). The original
# expectation was |PM_WRITTEN n volumes `catalog locate` names for the unit| --
# "the walk's own record intersected with the catalog's", chosen so it would
# not be a re-derivation of tapectl's SQL and could therefore actually
# disagree with it. The intent was right; the quantity was not. audit's
# copy_count is the count of ELIGIBLE copies of a CURRENT version (ADR-0012:
# "a unit is as covered as its least-covered live version" -- literally a MIN
# over current snapshots in `policy::coverage::copy_count_expr`). The old
# expression counted VOLUMES EVER ASSOCIATED WITH THE UNIT, across every
# version and every status, then intersected with this walk's writes. Those
# are different quantities, and they diverge in BOTH directions:
#   * too low  -- a copy written by the bootstrap is not in PM_WRITTEN at all
#                 (seed 2: "audit says 1, this walk wrote 0");
#   * too high -- a volume this walk wrote and later superseded or erased is
#                 still named by `catalog locate`, and an older version's
#                 volume is named too (seed 3: "audit says 1, this walk wrote
#                 3").
# So it was not merely mis-tuned. Do NOT reinstate PM_WRITTEN here: a count
# the harness derives independently cannot account for versions and erasures
# without interpreting catalog semantics, and interpreting them IS the
# re-derivation the original comment was right to avoid.
#
# WHAT IT ASSERTS NOW -- two statements, both seed-independent:
#
#  1. THE SAFETY BOUND, independent of tapectl's own eligibility verdict:
#     audit may not claim more copies than there are distinct volumes carrying
#     that version. Overstatement is the direction that loses data -- it is
#     what makes a `volume retire` look safe when it is not -- so this half
#     deliberately uses no tapectl predicate at all, only "how many volumes is
#     this version on".
#
#  2. CROSS-SURFACE AGREEMENT: audit's count must equal the number of volumes
#     `catalog locate` marks Serviceable at the least-covered current version.
#     That is not circular: `audit` reaches its number through
#     `coverage::copy_count_expr` and `locate` marks rows through
#     `coverage::eligible` -- two derivations sharing a module, which is
#     exactly the pair that can drift. Nothing compared two coverage surfaces
#     before, and that is how issue #153 shipped a wrong count that every
#     individual surface agreed with itself about.
#
# `snapshot list --status current` supplies which versions are current; the MIN
# is taken over those, mirroring copy_count_expr. Deposits are deliberately not
# modelled -- `permute`'s op pool contains no warehouse operation, so a deposit
# can never exist here, and the check FAILS LOUDLY rather than silently
# mis-counting if one ever appears.
#
# Not asserted here, on purpose: that `catalog locate` never names a volume the
# walk did not write. That is a real property and a different check; it is out
# of scope rather than forgotten.
pm_final_copy_count_is_honest() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: audit's copy_count must equal locate's serviceable count at the least-covered current version, and never exceed the volumes carrying it"; return 0; }
    local af="$RUN/log-pm.final.audit.json"
    TCTL audit --json >"$af" 2>"$RUN/log-pm.final.audit.stderr"
    local u rc=0
    for u in photos docs big; do
        local locate_json cur_json derived expected upper
        locate_json="$(TCTL catalog locate "$u" --json 2>/dev/null)" || continue
        cur_json="$(TCTL snapshot list --unit "$u" --status current --json 2>/dev/null)" || {
            echo "snapshot list --status current unreadable for $u"; rc=1; continue
        }
        # Emits "<expected> <upper>", or "ERR <reason>".
        derived="$(printf '%s\n' "$locate_json" | CUR="$cur_json" python3 -c '
import json, os, sys

def fail(msg):
    print("ERR " + msg)
    raise SystemExit

try:
    rows = json.load(sys.stdin)
except Exception as e:
    fail("catalog locate --json unparseable: %s" % e)
try:
    cur = json.loads(os.environ["CUR"])
except Exception as e:
    fail("snapshot list --json unparseable: %s" % e)

if isinstance(rows, dict):
    rows = rows.get("volumes", [])
current = set()
for c in (cur if isinstance(cur, list) else cur.get("snapshots", [])):
    if not isinstance(c, dict) or "version" not in c:
        fail("snapshot list row has no version field")
    current.add(c["version"])

# No current version at all: audit has nothing to count, expect 0.
if not current:
    print("0 0")
    raise SystemExit

serviceable = {v: set() for v in current}
carrying    = {v: set() for v in current}
for r in rows:
    if not isinstance(r, dict):
        fail("catalog locate row is not an object")
    for f in ("volume", "version", "serviceable"):
        if f not in r:
            fail("catalog locate row has no %s field" % f)
    if r.get("warehouse"):
        fail("a warehouse deposit exists; permute has no deposit op, so this "
             "check no longer models what audit counts -- update it")
    v = r["version"]
    if v not in current:
        continue
    carrying[v].add(r["volume"])
    if r["serviceable"]:
        serviceable[v].add(r["volume"])

# copy_count_expr takes the MIN over current snapshots; the safety bound is
# read at that same version, so the two halves speak about one version.
worst = min(current, key=lambda v: (len(serviceable[v]), v))
print("%d %d" % (len(serviceable[worst]), len(carrying[worst])))
')"
        case "$derived" in
            ERR\ *) echo "${derived#ERR }  (unit $u)"; rc=1; continue ;;
        esac
        expected="${derived%% *}"; upper="${derived##* }"
        local claimed
        claimed="$(U="$u" python3 -c '
import json, os, re, sys
d = json.load(open(sys.argv[1]))
for f in (d.get("findings") or []):
    if f.get("check") == "copy_count" and f.get("unit") == os.environ["U"]:
        m = re.search(r"has (\d+) copies", f.get("message", ""))
        print(m.group(1) if m else "unparsed"); break
else:
    print("none")
' "$af")"
        # "none" means audit raised no copy_count finding for this unit, which
        # is only consistent with the walk if policy is already satisfied. We
        # cannot read min_copies from here, so a silent audit is accepted; the
        # assertion is about the number audit DOES state.
        if [ "$claimed" = "unparsed" ]; then
            echo "copy_count message for $u did not carry a count"; rc=1
        elif [ "$claimed" != "none" ]; then
            if [ "$claimed" -gt "$upper" ]; then
                echo "copy_count OVERSTATED for $u: audit says $claimed, but only $upper volume(s) carry that version at all"
                rc=1
            elif [ "$claimed" != "$expected" ]; then
                echo "copy_count disagreement for $u: audit says $claimed, catalog locate marks $expected volume(s) serviceable at the least-covered current version"
                rc=1
            fi
        fi
    done
    return $rc
}

scenario_permute() {
    check pm.setup bootstrap_archive_v1 VOL-A
    # bootstrap_archive_v1 stages every unit itself, so seed the staged baseline
    # from it — otherwise the first write would have nothing to compare against.
    if [ "$DRY_RUN" != 1 ]; then
        for _u in photos docs big; do
            [ -d "$SRC/$_u" ] && pm_capture_staged "$_u"
        done
    fi

    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: seed=$SEED steps=$STEPS op sequence:"
        pm_generate_sequence | nl -ba
        return 0
    fi

    local sd; sd="$(dirname "$HOME_DIR")"
    local seqfile="$sd/permute-sequence.txt"
    pm_generate_sequence >"$seqfile"
    {
        echo "### permute sequence (seed=$SEED, steps=$STEPS) — reproduce with:"
        echo "###   scripts/lifecycle-suite.sh --scenario permute --seed $SEED --steps $STEPS [...]"
        echo '```'
        cat "$seqfile"
        echo '```'
    } >>"$REPORT"

    local i=0 line op
    while IFS= read -r line; do
        i=$((i + 1))
        op="${line%% *}"
        PM_CHECK_NAME="pm.step$i.$op"
        # shellcheck disable=SC2086  # word-splitting the (unit|report-name) arg is intentional
        check "$PM_CHECK_NAME" pm_run_step $line
        PM_STEP_TAG="step$i"
        check "pm.step$i.invariants" pm_post_step_invariants "$op"
    done <"$seqfile"

    # Restore matrix for the latest version of every unit that ended up on
    # some written volume this walk — "the latest version" here means
    # whichever written volume most recently carried that unit, found via
    # `catalog locate` rather than re-deriving it from the walk.
    local u tenant locate_json labels last pm_sd
    pm_sd="$(dirname "$HOME_DIR")"
    for u in photos docs big; do
        case "$u" in photos|big) tenant=alice ;; docs) tenant=bob ;; esac
        if [ "${#PM_WRITTEN[@]}" -eq 0 ]; then
            check "pm-final-$u.unit" pm_skip_never_written "$u"
            continue
        fi
        locate_json="$(TCTL catalog locate "$u" --json 2>/dev/null)"
        labels="$(echo "$locate_json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
vols = d if isinstance(d, list) else d.get("volumes", [])
for v in vols:
    if not isinstance(v, dict):
        print(v); continue
    # Issue #252: this read `v.get("label", v)`. `catalog locate --json`
    # emits its volume label under "volume" (src/cli/catalog.rs pins the
    # shape), so "label" was always absent, the default returned the whole
    # dict, no candidate ever matched, and the end-of-walk restore matrix
    # was SKIPPED on every run of `permute` -- silently, because the skip
    # branch is a legitimate outcome for a never-written unit.
    # Raise rather than fall back to a second key name: a fallback would let
    # the same drift go quiet again, and this check exists precisely because
    # it went quiet once.
    if "volume" not in v:
        raise SystemExit("catalog locate --json row has no 'volume' key: %r" % (v,))
    print(v["volume"])
' 2>/dev/null)"
        last=""
        for cand in "${PM_WRITTEN[@]}"; do
            echo "$labels" | grep -qx "$cand" && last="$cand"
        done
        if [ -n "$last" ]; then
            # Issue #252, second half. Compare against the baseline FROZEN
            # for that volume, never the live source: this is a randomised
            # walk that mutates $SRC between writes, so a tape written at
            # step 3 cannot match a source tree edited at step 7, and the
            # matrix would fail by construction on every seed. The harness
            # already freezes it -- `pm_write_next_volume` does
            # `cp -a "$sd/pm-staged/." "$sd/pm-snapshot-$label/"` under the
            # comment "Freeze the baseline for THIS volume from what was
            # staged, not from the live source".
            #
            # This line said `$SRC/$u` and nobody saw it, because the
            # `v.get("label")` bug above meant `last` was always empty and
            # this branch never ran. A skip was hiding a wrong comparison.
            # Issue #282: this matrix read whatever cartridge happened to be
            # in the drive -- it never loaded one. `last` is the most recent
            # volume `catalog locate` names for THIS unit, which need not be
            # the volume written last overall: a `staging-clean` op can
            # release every stage set and the next `write-next-volume` carry
            # a strict subset of the units. So whether the right tape was
            # loaded was decided by the RNG -- red on the seeds where it was
            # not, and green on the others for no reason this check
            # controls. The scenario header two thousand lines up already
            # names that as the worst property a gate can have; it was
            # written about the copy_count assertion that was moved out for
            # exactly this, and then the matrix was enabled (issue #252)
            # with the same flaw.
            pm_latest="${PM_WRITTEN[-1]}"
            if [ "$SINGLE_CARTRIDGE" = 1 ]; then
                # One cartridge: `next_tape` erased it in place before each
                # later write, so only the volume written LAST still has
                # bytes. `load_volume_tape` refuses here unconditionally and
                # is right to -- there is nothing to fetch.
                if [ "$last" != "$pm_latest" ]; then
                    check "pm-final-$u.unit" pm_skip_unreachable "$u" \
                        "$last holds $u's latest copy but was erased in place by a later write under --single-cartridge"
                    continue
                fi
            elif [ "$last" != "$pm_latest" ] && ! load_volume_tape "$last"; then
                # Multi-cartridge: the cartridge is in a library slot, so it
                # must be loaded before reading or `binding::corroborate_volume`
                # refuses the contact. Only load when it is not already in the
                # drive -- reloading the loaded volume is needless tape motion.
                check "pm-final-$u.unit" pm_skip_unreachable "$u" \
                    "could not load $last, the cartridge holding $u's latest copy"
                continue
            fi
            restore_matrix "$last" "$u" "$tenant" "$pm_sd/pm-snapshot-$last/$u" "pm-final-$u"
        else
            check "pm-final-$u.unit" pm_skip_never_written "$u"
        fi
    done

    # Runs last, after every write this walk is going to do (issue #203).
    check pm.final_copy_count_is_honest pm_final_copy_count_is_honest
}

# ---------- REPORT.md ----------
# The verdict line is the string that gets pasted into issues, commit messages
# and this Policy block, and on its own it claims more than the run proved:
# "LIFECYCLE-SUITE GREEN" says nothing about which seed, which erase mode, or
# whether the weaker --single-cartridge allowance was in force. This suite has
# already been bitten twice in exactly that gap — `retire-and-reuse` passes
# under `--erase long` and fails under `--erase short` (issue #198), and
# `--single-cartridge`'s copy_count allowance hid a `permute` failure
# completely. `permute` adds a third: it is one sample of a seeded walk, so a
# green run is evidence about that seed and no other (issue #288).
#
# So the mode travels with the number. A reader who sees only the last line
# still knows what was and was not exercised.
run_shape() {
    local scope perm=""
    if [ "$RUN_ALL" = 1 ]; then
        scope="all ${#SCENARIO_NAMES[@]} scenarios"
    else
        scope="scenario $SCENARIO"
    fi
    # SEED drives `mutate_source` in six scenarios, not just `permute`, so it
    # is always named; the step count only means anything when permute ran.
    if [ "$RUN_ALL" = 1 ] || [ "$SCENARIO" = "permute" ]; then
        perm=" ($STEPS permute steps)"
    fi
    printf '%s, seed %s%s, erase %s, %s, %s' \
        "$scope" "$SEED" "$perm" "$ERASE_MODE" \
        "$([ "$SINGLE_CARTRIDGE" = 1 ] && echo single-cartridge || echo multi-cartridge)" \
        "$TAPE_DEV"
}

write_report_header() {
    [ "$DRY_RUN" = 1 ] && return 0
    {
        echo "# lifecycle-suite run — $STAMP"
        echo
        echo "- device: \`$TAPE_DEV\`"
        if [ "$MHVTL_DISCOVERY" = 1 ]; then
            echo "- drive: $DRIVE_MODEL (sg \`$DRIVE_SG\`, changer \`$CHG_SG\`, dte $DTE)"
        else
            echo "- drive: real (non-mhvtl), \`$DRIVE_MODEL\`"
        fi
        echo "- erase mode: $ERASE_MODE"
        echo "- single-cartridge: $([ "$SINGLE_CARTRIDGE" = 1 ] && echo yes || echo no)"
        echo "- seed: $SEED"
        echo
    } >>"$REPORT"
}

write_report_checks() {
    [ "$DRY_RUN" = 1 ] && return 0
    echo "## Checks" >>"$REPORT"
    echo >>"$REPORT"
    echo "| name | result | note |" >>"$REPORT"
    echo "|---|---|---|" >>"$REPORT"
    local n
    for n in "${CHECKS[@]}"; do
        echo "| $n | ${RESULT[$n]} | ${NOTE[$n]:-} |" >>"$REPORT"
    done
    echo >>"$REPORT"
}

write_report_footer() {
    [ "$DRY_RUN" = 1 ] && return 0
    {
        echo "## Slot -> label map"
        echo
        echo '```'
        cat "$SLOT_LABEL_MAP"
        echo '```'
        echo
        echo "## Skips"
        echo
        echo '```'
        cat "$SKIPPED_FILE"
        echo '```'
        echo
        echo "Commands journal: \`$COMMANDS_LOG\`"
    } >>"$REPORT"
}

# ---------- dispatch ----------
# Each scenario gets its OWN home/config/source tree and its own tape-slot
# tracking (spec: "a fresh HOME_DIR unless stated"; "slot tracking is
# per-scenario") — required for --all to run every scenario in one
# invocation without one scenario's state leaking into the next. Check
# names and restore-matrix tags are scenario-prefixed by convention, so the
# shared $CHECKS/$RESULT arrays and $RUN/log-<name>.txt paths stay unique
# without needing a second layer of namespacing.
# ---------------------------------------------------------------------------
# Scenario: stale-catalog-sealed-tape (issue #208)
#
# The ADR-0003 case that NO scenario covered until 2026-09-17, and the reason
# the pre-production review ranked #208 high rather than medium.
#
# Every other sealed-tape check in this suite drives the identity-MISMATCH
# branch of `check_tape_contact`: `volume init VOL-X` over VOL-A's cartridge,
# where File 0 names a different volume. That branch has always consulted the
# TAPE's own seal pointer, so it always refused.
#
# The identity-MATCHES branch did not. There, the only seal probe was the
# CALLER's `seal_position` — a position in the layout `volume write` has just
# built from whatever is staged NOW, which coincides with the tape's real seal
# only if the new content lays out identically. Different content, and the
# probe reads a position with no marker, returns Matches, and a SEALED tape is
# overwritten. ADR-0003 forbids that outright and `--force` cannot even reach
# it, so nothing downstream would have refused.
#
# Reaching it needs the catalog and the tape to disagree: the row must still
# say `initialized` while the tape is sealed. That is not contrived — it is a
# database restored from a backup predating the write, which `db backup` makes
# a first-class operation and `docs/handoff.md` treats as a supported recovery
# path. So this scenario builds it with tapectl's own commands and one file
# copy, because restoring a backup IS copying the backup DB into place.
#
# The assertion that matters is the LAST one: the refusal must cite ADR-0003.
# A refusal citing anything else means the catalog-side guard fired first and
# the scenario never reached the tape check it exists to exercise.
sct_setup() {
    bootstrap_config || return 1
    TCTL location add vault --description "Home vault" || return 1
    TCTL tenant add alice || return 1
    if [ "$DRY_RUN" = 1 ]; then
        TCTL key generate --escrow
    else
        TCTL key generate --escrow >"$RUN/log-_sct_escrow.txt" 2>&1 || return 1
        capture_escrow_secret _sct_escrow
    fi
    make_source "$SRC/photos" "plain" || return 1
    TCTL unit init "$SRC/photos" --tenant alice --name photos || return 1
    TCTL snapshot create photos || return 1
    TCTL stage create photos || return 1
    next_tape VOL-S || return 1
    vinit VOL-S
}

# Taken while VOL-S is `initialized` and File 0 is stamped but no data is
# written. This is the state the stale catalog will be rolled back to.
sct_backup_while_writable() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl db backup --to \$sd/sct-pre-write.db --include-keys (VOL-S still 'initialized')"; return 0; }
    local sd; sd="$(dirname "$HOME_DIR")"
    TCTL db backup --to "$sd/sct-pre-write.db" --include-keys
}

sct_write_and_seal() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume write VOL-S (seals the tape; catalog now says 'sealed')"; return 0; }
    TCTL volume write VOL-S --device "$TAPE_DEV"
}

sct_stale_catalog_write_refused() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: restore the pre-write backup into a copy of the home, then tapectl volume write VOL-S (expect REFUSED citing ADR-0003 — the tape-side check, not the catalog guard)"
        return 0
    fi
    local sd newhome; sd="$(dirname "$HOME_DIR")"; newhome="$sd/sct-restored-home"
    rm -rf "$newhome"
    # A copy of the home, not a fresh `init`: `volume write` resolves its
    # backend STRICTLY (config::resolve_lto_backend), so a home without this
    # run's [[backends.lto]] would fail on the backend long before the tape
    # check and the scenario would assert the wrong refusal.
    cp -a "$HOME_DIR" "$newhome" || { echo "could not copy the home"; return 1; }
    cp "$sd/sct-pre-write.db" "$newhome/tapectl.db" || { echo "could not restore the pre-write backup"; return 1; }
    # Sanity: the restored catalog really must believe VOL-S is writable,
    # otherwise the refusal below proves nothing about the tape check.
    local status
    status="$(python3 - "$newhome/tapectl.db" <<'PY2'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT status FROM volumes WHERE label = 'VOL-S'").fetchone()
print(row[0] if row else "MISSING")
PY2
)"
    [ "$status" = "initialized" ] || {
        echo "restored catalog says VOL-S is \"$status\", not \"initialized\" — this scenario cannot reach the tape-side check, so its refusal would prove nothing"
        return 1
    }
    # The layout must DIFFER from the one already on the tape, and this is
    # the whole point of the scenario rather than a detail. `volume write`
    # probes the seal at ITS OWN layout's seal-marker position; re-writing
    # byte-identical content puts that position exactly where the real seal
    # is, so the probe finds it by coincidence and the bug hides. Measured:
    # a first draft of this scenario re-wrote the same stage set and PASSED
    # against the unfixed code -- green for the wrong reason, which is the
    # failure mode this suite has produced three times (#198, #203).
    #
    # Staging a second unit in the restored home grows the layout, so the new
    # seal-marker entry lands past the real seal and the caller's probe reads
    # an unwritten position.
    make_source "$SRC/extra" "plain" || return 1
    NEWHOME_TCTL "$newhome" unit init "$SRC/extra" --tenant alice --name extra \
        >"$sd/sct.extra-init.txt" 2>&1 || { cat "$sd/sct.extra-init.txt"; return 1; }
    NEWHOME_TCTL "$newhome" snapshot create extra >"$sd/sct.extra-snap.txt" 2>&1 \
        || { cat "$sd/sct.extra-snap.txt"; return 1; }
    NEWHOME_TCTL "$newhome" stage create extra >"$sd/sct.extra-stage.txt" 2>&1 \
        || { cat "$sd/sct.extra-stage.txt"; return 1; }

    local out rc
    out="$(NEWHOME_TCTL "$newhome" volume write VOL-S --device "$TAPE_DEV" 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || {
        echo "volume write VOL-S SUCCEEDED against an already-sealed tape — ADR-0003 violation (issue #208): $out"
        return 1
    }
    echo "$out" | grep -q "ADR-0003" || {
        echo "refused, but not by the tape-side seal check: the message does not cite ADR-0003, so the catalog guard fired first and this scenario did not exercise what it claims: $out"
        return 1
    }
}

scenario_stale_catalog_sealed_tape() {
    check sct.setup                  sct_setup
    check sct.backup_while_writable  sct_backup_while_writable
    check sct.write_and_seal         sct_write_and_seal
    check sct.stale_catalog_refused  sct_stale_catalog_write_refused
}

# ============================================================
# Scenario: cartridge-displacement
# ============================================================
# ADR-0010's re-initialisation path, on media (issue #226 scenario A).
#
# `volume init` BINDS the cartridge it is talking to by its MAM medium serial,
# and when that cartridge already carries another volume it RECORDS the
# displacement rather than refusing it: the open mount closes, the displaced
# volume moves to `erased`, a `displaced` event is written, and the warning
# names every unit the displacement leaves without a copy
# (`binding::mount_and_record` + `binding::render_displacement`, issue #235).
# In-module tests cover the catalog half; nothing has ever driven it from a
# drive.
#
# THE SHAPE IS DECIDED BY ADR-0003, not by preference. Displacement is not
# gated by the displaced volume being sealed -- that refusal lives on the TAPE
# side, in `check_tape_contact`'s File 0 probe, not in the catalog binding. So
# a re-init over a still-readable sealed tape is refused (correctly, and
# --force does not help), and the catalog-layer displacement path is reachable
# only once File 0 is gone. This scenario therefore erases the cartridge IN
# PLACE and re-inits it WITHOUT --force: there is no second consent gate, and
# asserting that there isn't one is half the point.
#
# TWO units, deliberately. `solo`'s only copy is on the displaced volume;
# `kept` has another copy on a different cartridge. The warning has two arms
# (`*** ZERO copies ***` versus "N other copy/copies remain") and a
# single-unit scenario cannot tell "named the right unit" from "named every
# unit". The matrix at the end then proves the surviving copy is real rather
# than merely counted -- the warning's claim is what an operator acts on.
#
# MEASURED 2026-09-17, before this scenario was written: `mt erase` on mhvtl
# leaves the MAM medium serial intact (E01001L8_1775794348, byte-identical
# before and after). That is what makes the binding survive the erase and the
# displacement reachable at all. If it ever stops holding, this scenario goes
# red at cd.init_d2_displaces with no displacement warning at all -- read this
# note before blaming the code.
cd_skip_single_cartridge() {
    skip "cd.scenario" "displacement needs a real erase of a cartridge already bound to a sealed volume (mt erase -- instant on mhvtl, HOURS on a real LTO, and --erase short cannot reach the no-force path) plus a second cartridge to hold the surviving copy that gives the warning two arms"
    return $?
}

cd_setup() {
    bootstrap_config || return 1
    TCTL tenant add alice || return 1
    # ADR-0005: staging refuses without an escrow recipient.
    if [ "$DRY_RUN" = 1 ]; then
        TCTL key generate --escrow
    else
        TCTL key generate --escrow >"$RUN/log-cd_escrow.txt" 2>&1 || return 1
        capture_escrow_secret cd_escrow
    fi
    make_source "$SRC/kept" "plain" || return 1
    make_source "$SRC/solo" "unicode" || return 1
    TCTL unit init "$SRC/kept" --tenant alice --name kept || return 1
    TCTL unit init "$SRC/solo" --tenant alice --name solo || return 1
}

cd_stage_kept() { TCTL snapshot create kept && TCTL stage create kept; }

cd_write_kept_on_d0() {
    next_tape VOL-D0 || return 1
    vinit VOL-D0 || return 1
    TCTL volume write VOL-D0 --device "$TAPE_DEV"
}

cd_stage_solo() { TCTL snapshot create solo && TCTL stage create solo; }

# `volume write` consumes every stage set still at status='staged', and
# nothing in the write path moves one out of it (verified 2026-09-17: the only
# writers of `stage_sets.status` are `staging::stage_create` -> 'staged',
# `staging::clean` -> 'cleaned', `db::open`'s crash sweep -> 'failed', and
# `read-slices`' restore-to-'staged'). So this one call puts kept's SECOND
# copy and solo's FIRST on the same cartridge -- which is the state the
# displacement has to reason about.
cd_write_both_on_d1() {
    next_tape VOL-D1 || return 1
    vinit VOL-D1 || return 1
    TCTL volume write VOL-D1 --device "$TAPE_DEV"
}

# The precondition the whole scenario rests on. If kept does not really have
# two copies here, the "1 other copy/copies remain" arm below would pass for
# the wrong reason -- and a displacement warning that says ZERO for everything
# is indistinguishable from one that is right.
cd_preconditions() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl report copies --json (assert kept=2, solo=1 BEFORE the erase)"; return 0; }
    local f="$RUN/log-cd.copies-before.json"
    TCTL report copies --json >"$f" 2>"$f.err" || { cat "$f.err" "$f"; return 1; }
    python3 - "$f" <<'PY2' || { echo "copies before the erase are not 2/1:"; cat "$f"; return 1; }
import json, sys
d = {r["unit"]: r["copies"] for r in json.load(open(sys.argv[1]))}
assert d.get("kept") == 2, d
assert d.get("solo") == 1, d
PY2
}

# blank_tape, never erase_tape: `--erase short` (weof at BOT) leaves a present
# but unparseable File 0, which `volume init` refuses WITHOUT --force -- and
# this scenario's point is that displacement needs no second consent, so it
# must not be run with one. A real erase removes File 0 and leaves the
# ADR-0003 tape-side gate nothing to refuse, while the MAM serial (and so the
# catalog binding) survives untouched.
cd_erase_c1_in_place() { blank_tape; }

# Deliberately NOT `vinit`: vinit appends $REUSE_FORCE, and a --force here
# would prove nothing -- the claim under test is that ADR-0010 records a
# displacement rather than demanding consent for it.
cd_init_d2_displaces() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: tapectl volume init VOL-D2 --device $TAPE_DEV (NO --force) -- expect exit 0, a warning naming VOL-D1, solo at ZERO copies, kept with one copy remaining, and kept NOT named as zero"
        return 0
    fi
    local out rc
    out="$(TCTL volume init VOL-D2 --device "$TAPE_DEV" 2>&1)"; rc=$?
    printf '%s\n' "$out" >"$RUN/log-cd.init-d2.txt"
    [ "$rc" -eq 0 ] || {
        echo "volume init VOL-D2 was REFUSED on a blanked cartridge (exit $rc). ADR-0010 records a displacement, it never gates one; a second consent point was deliberately rejected: $out"
        return 1
    }
    printf '%s\n' "$out" | grep -q 'previously held volume "VOL-D1"' || {
        echo "init succeeded but recorded no displacement of VOL-D1 -- the cartridge binding did not survive the erase, or mount_and_record never ran: $out"
        return 1
    }
    printf '%s\n' "$out" | grep -qE 'unit "solo" \[[^]]*\] now has ZERO copies' || {
        echo "the displacement warning did not name solo as left with ZERO copies -- this is the half of render_displacement that #235 found dropped on the rebuild path: $out"
        return 1
    }
    printf '%s\n' "$out" | grep -qE 'unit "kept" \[[^]]*\]: 1 other copy' || {
        echo "the displacement warning did not report kept's surviving copy: $out"
        return 1
    }
    printf '%s\n' "$out" | grep -E 'unit "kept"' | grep -q 'ZERO copies' && {
        echo "the warning called kept zero-copy as well as solo -- it is naming every unit on the volume, not the ones actually left uncovered: $out"
        return 1
    }
    return 0
}

# The catalog half of the same act, through `volume list` (issue #195) rather
# than sqlite3: VOL-D1 is `erased`, VOL-D2 is live, and BOTH still name the
# same cartridge barcode -- a displaced volume keeps naming the cartridge it
# lived on, which is what lets an operator see where the bytes went.
cd_catalog_records_the_displacement() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume list --json (VOL-D1 erased, VOL-D2 live, both on the SAME barcode, VOL-D0 on a different one)"; return 0; }
    local f="$RUN/log-cd.volumes.json"
    TCTL volume list --json >"$f" 2>"$f.err" || { cat "$f.err" "$f"; return 1; }
    python3 - "$f" <<'PY2' || { cat "$f"; return 1; }
import json, sys
v = {r["label"]: r for r in json.load(open(sys.argv[1]))}
for lbl in ("VOL-D0", "VOL-D1", "VOL-D2"):
    assert lbl in v, (lbl, sorted(v))
assert v["VOL-D1"]["status"] == "erased", v["VOL-D1"]
assert v["VOL-D2"]["status"] != "erased", v["VOL-D2"]
c1, c2, c0 = v["VOL-D1"]["cartridge"], v["VOL-D2"]["cartridge"], v["VOL-D0"]["cartridge"]
assert c1 and c2 and c0, (c0, c1, c2)
assert c1 == c2, ("displaced and displacing volumes must name the SAME cartridge", c1, c2)
assert c0 != c1, ("VOL-D0 must be on a different cartridge", c0, c1)
PY2
}

cd_displaced_event() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl report events --json (assert an action='displaced' row for VOL-D1)"; return 0; }
    local f="$RUN/log-cd.events.json"
    TCTL report events --json >"$f" 2>"$f.err" || { cat "$f.err" "$f"; return 1; }
    python3 - "$f" <<'PY2' || { echo "no displaced event for VOL-D1:"; cat "$f"; return 1; }
import json, sys
rows = json.load(open(sys.argv[1]))
hit = [r for r in rows if r.get("action") == "displaced"]
assert hit, rows[:5]
assert any("VOL-D1" in json.dumps(r) for r in hit), hit
PY2
}

cd_copies_after() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl report copies --json (assert kept=1, solo=0 after the displacement)"; return 0; }
    local f="$RUN/log-cd.copies-after.json"
    TCTL report copies --json >"$f" 2>"$f.err" || { cat "$f.err" "$f"; return 1; }
    python3 - "$f" <<'PY2' || { echo "copy counts did not follow the displacement:"; cat "$f"; return 1; }
import json, sys
d = {r["unit"]: r["copies"] for r in json.load(open(sys.argv[1]))}
assert d.get("solo") == 0, ("solo's only copy was on the displaced volume", d)
assert d.get("kept") == 1, ("kept must keep exactly the copy on VOL-D0", d)
PY2
}

cd_reload_d0() { load_volume_tape VOL-D0; }

scenario_cartridge_displacement() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        check cd.scenario cd_skip_single_cartridge
        return 0
    fi

    check cd.setup             cd_setup
    check cd.stage_kept        cd_stage_kept
    check cd.write_kept_on_d0  cd_write_kept_on_d0
    check cd.stage_solo        cd_stage_solo
    check cd.write_both_on_d1  cd_write_both_on_d1
    check cd.preconditions     cd_preconditions
    check cd.erase_c1          cd_erase_c1_in_place
    check cd.init_d2_displaces cd_init_d2_displaces
    check cd.catalog_records   cd_catalog_records_the_displacement
    check cd.displaced_event   cd_displaced_event
    check cd.copies_after      cd_copies_after

    # The warning said kept still has a copy. Prove that copy is real, every
    # way tapectl can reach it -- an evidence line an operator acts on is
    # worth only as much as the bytes behind it (ADR-0004).
    check cd.reload_d0         cd_reload_d0
    restore_matrix VOL-D0 kept alice "$SRC/kept" cd-kept
}
# ============================================================
# Scenario: collection-second-copy
# ============================================================
# The per-copy flow, on media (issue #226 scenario B, rescoped by #229).
#
# This scenario replaces the multi-label one #226 originally asked for. That
# one is gone: `collection run --label A --label B` could never complete --
# `execute_batch`'s write loop drives ONE device with no prompt, eject, pause
# or changer anywhere in src/, so copy 2 always met copy 1's cartridge still
# loaded -- and the CTO ruled the command takes exactly one destination and
# NAMES the per-copy `volume write` invocations instead (#229). A test written
# against the original description would now be testing a refusal.
#
# So this tests the recipe the refusal prints, end to end, which is strictly
# more valuable: it is the only supported route to min_copies = 2 through the
# collection path, and nothing had ever run it.
#
#   1. `collection run` writes copy 1 and RETAINS staging, because the units
#      resolve min_copies = 2 (#238: `execute_batch` gates release on each
#      unit's own resolved min_copies, not on `clean_staging`'s guard, which
#      passes vacuously once one write row is 'completed').
#   2. Swap cartridges, `volume write VOL-CS2` -- and it consumes the SAME
#      staged bytes rather than re-staging. That is the property the whole
#      ruling rests on, and it was false when the ruling was written.
#   3. Two copies, then staging releases.
#
# Needs two distinct cartridges, so it skips visibly under --single-cartridge,
# the same way `compaction` does.
csc_skip_single_cartridge() {
    skip "csc.scenario" "the per-copy flow #229's refusal names needs two simultaneously distinct cartridges (VOL-CS1 then VOL-CS2) -- impossible under --single-cartridge"
    return $?
}

# The ruling itself, pinned cheaply and FIRST: refused, for the stated reason,
# and before anything is staged. `cmd_run` calls `plan_for_run` before any
# side effect and `plan_for_run` checks the label count before it resolves a
# destination budget, so this cannot be passing because the labels happen not
# to exist.
csc_multi_label_refused() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: tapectl collection run --collection media --batch 0 --label VOL-CS1 --label VOL-CS2 (expect REFUSED citing issue #229) then tapectl stage list --json (assert still empty -- refused BEFORE staging)"
        return 0
    fi
    local out rc
    out="$(TCTL collection run --collection media --batch 0 \
              --label VOL-CS1 --label VOL-CS2 --device "$TAPE_DEV" 2>&1)"; rc=$?
    printf '%s\n' "$out" >"$RUN/log-csc.multi-label.txt"
    [ "$rc" -ne 0 ] || { echo "collection run accepted two --label values; #229 ruled it refuses: $out"; return 1; }
    printf '%s\n' "$out" | grep -q "more than one destination label" || {
        echo "refused, but not by #229's rule -- the message does not name the label count, so this proves nothing about the ruling: $out"
        return 1
    }
    printf '%s\n' "$out" | grep -q "issue #229" || {
        echo "the refusal does not cite issue #229: $out"; return 1; }
    local f="$RUN/log-csc.staged-after-refusal.json"
    TCTL stage list --json >"$f" 2>"$f.err" || { cat "$f.err" "$f"; return 1; }
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d == [], d' "$f" || {
        echo "the refusal happened AFTER staging -- a refused run must cost nothing:"; cat "$f"; return 1; }
}

csc_run_first_copy() {
    next_tape VOL-CS1 || return 1
    vinit VOL-CS1 || return 1
    if [ "$DRY_RUN" = 1 ]; then
        TCTL collection run --collection media --batch 0 --label VOL-CS1 --device "$TAPE_DEV" --json
        echo "PLAN: assert staging_released=false and every under_copied entry is 1/2"
        return 0
    fi
    # stdout and stderr to SEPARATE files, deliberately. The suite's usual
    # `>"$f" 2>&1` idiom would merge `volume_write`'s staged-selection
    # announcement into the JSON and this check would fail on a parse error
    # while the tool was behaving correctly (measured 2026-09-17, first run of
    # this scenario). That announcement is on stderr precisely so `--json`
    # stdout stays parseable (`announce_staged_selection`), and asserting the
    # WHOLE stdout parses is the live guard against the #56 trailer defect --
    # so the fix is to stop merging, never to grep the JSON object out of a
    # mixed stream. Copy this shape, not the `2>&1` one, for any command that
    # writes to stderr.
    local f="$RUN/log-csc.run1.json" e="$RUN/log-csc.run1.err"
    TCTL collection run --collection media --batch 0 --label VOL-CS1 \
        --device "$TAPE_DEV" --json >"$f" 2>"$e" || { cat "$e" "$f"; return 1; }
    python3 - "$f" <<'PY2' || { cat "$e" "$f"; return 1; }
import json, sys
d = json.load(open(sys.argv[1]))
assert d["copies_written"] == 1, d
assert d["units_staged"] >= 1, d
assert d["staging_released"] is False, (
    "one copy released staging while min_copies is 2 -- issue #238's gate is not holding", d)
assert d["under_copied"], d
for p in d["under_copied"]:
    assert p["copies"] == 1 and p["min_copies"] == 2, p
PY2
}

# csc_fingerprint <outfile> -- a canonical record of every stage set and the
# CIPHERTEXT sha256 of each of its slices, taken from `stage list` and `stage
# info` (never sqlite3: both facts have a command that reports them).
#
# This is the whole proof that the second copy consumed the SAME staged bytes
# instead of re-staging, and a re-stage would show in all three halves at
# once: a new `stage_sets` row (new id), `stage_set_count` above 1 for that
# version, and DIFFERENT sha256s -- dar stamps timestamps and age is
# randomised per call, so identical input never re-encrypts to identical bytes
# (CLAUDE.md, "Re-staging vs read-slices"). Comparing the ciphertext hash is
# what makes this an assertion about BYTES rather than about row counts.
csc_fingerprint() { # csc_fingerprint <outfile>
    local out="$1" listf="$1.list" units="$1.units"
    TCTL stage list --json >"$listf" 2>"$listf.err" || { cat "$listf.err" "$listf"; return 1; }
    python3 - "$listf" "$out" "$units" <<'PY2' || { cat "$listf"; return 1; }
import json, sys
rows = json.load(open(sys.argv[1]))
rows.sort(key=lambda r: (r["unit"], r["version"], r["id"]))
with open(sys.argv[2], "w") as f, open(sys.argv[3], "w") as u:
    for r in rows:
        f.write("set %s v%s %s id=%s\n" % (r["unit"], r["version"], r["status"], r["id"]))
        u.write("%s %s\n" % (r["unit"], r["version"]))
PY2
    local unit ver
    while read -r unit ver; do
        TCTL stage info "$unit" --version "$ver" --json >"$listf.info" 2>"$listf.info.err" \
            || { cat "$listf.info"; return 1; }
        python3 - "$listf.info" "$out" <<'PY2' || return 1
import json, sys
d = json.load(open(sys.argv[1]))
with open(sys.argv[2], "a") as f:
    f.write("info %s v%s id=%s %s sets=%s\n"
            % (d["unit"], d["version"], d["stage_set_id"], d["status"], d["stage_set_count"]))
    for s in d["slices"]:
        f.write("  slice %s %s\n" % (s["slice"], s["sha256"]))
PY2
    done <"$units"
    [ -s "$out" ] || { echo "csc_fingerprint: no stage sets at all to fingerprint"; return 1; }
    # EVERY set must still be LIVE ('staged'), and this is not decoration.
    # Without it the fingerprint is satisfied by two identical records of
    # NOTHING: run the pre-#238 negative control (release staging
    # unconditionally in `execute_batch`) and both captures read "cleaned" for
    # every set, so `diff` is empty and csc.same_staged_bytes passes while the
    # property it exists to prove is false. Measured 2026-09-17 -- it passed,
    # in exactly that control, before this guard was added. A comparison is
    # only evidence if both sides are known to be non-vacuous.
    # Issue #258: this guard was `grep -vq ' cleaned id=' "$out" || fail`, and
    # under GNU grep -- which is what a non-interactive bash resolves here --
    # `-vq` exits 0 as soon as ANY line is not selected. The fingerprint always
    # carries `info ...` and `  slice ...` lines, none of which can match
    # ' cleaned id=', so the guard exited 0 unconditionally and never fired.
    # The guard written to stop a vacuous comparison was itself vacuous.
    #
    # Written positively now, which is also flavour-independent: `grep -q PAT`
    # exits 0 if and only if PAT matches, under every grep. (Worth knowing: an
    # interactive shell on this box resolves `grep` to ugrep, whose `-vq`
    # disagrees with GNU's on exactly this case -- so testing a `-v` assertion
    # by hand can give the opposite answer to what the suite sees.)
    if grep -q ' cleaned id=' "$out"; then
        echo "csc_fingerprint: a stage set is already 'cleaned' -- there are no live staged bytes left to consume, so comparing this fingerprint to another would prove nothing"
        grep ' cleaned id=' "$out"
        return 1
    fi
    # And at least one set must actually be live, or the comparison has no
    # subject at all. Positive assertion for the same reason.
    grep -q ' staged id=' "$out" || {
        echo "csc_fingerprint: no stage set is 'staged' -- the fingerprint has no live bytes to compare"
        return 1
    }
}

csc_capture_staged() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: record stage list --json + per-unit stage info --json (stage_set_id, stage_set_count, every slice sha256) as the pre-second-copy fingerprint"; return 0; }
    csc_fingerprint "$RUN/csc.fingerprint.before"
}

csc_second_copy() {
    next_tape VOL-CS2 || return 1
    vinit VOL-CS2 || return 1
    TCTL volume write VOL-CS2 --device "$TAPE_DEV"
}

csc_same_staged_bytes() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: re-take the fingerprint and diff it against the pre-second-copy one (must be identical -- the second copy consumed the staged bytes, it did not re-stage)"; return 0; }
    csc_fingerprint "$RUN/csc.fingerprint.after" || return 1
    diff -u "$RUN/csc.fingerprint.before" "$RUN/csc.fingerprint.after" || {
        echo "the second copy did not consume the same staged bytes -- 'volume write' re-staged, and #229's whole recipe (run 'tapectl volume write <label>' directly against the same staged data) is false"
        return 1
    }
}

csc_two_copies() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl collection status --json (under_copied=0) and report copies --json (every media/* unit at 2)"; return 0; }
    local sf="$RUN/log-csc.status2.json" cf="$RUN/log-csc.copies.json"
    TCTL collection status --json >"$sf" 2>"$sf.err" || { cat "$sf.err" "$sf"; return 1; }
    python3 - "$sf" <<'PY2' || { echo "collection status still reports media under-copied after two copies:"; cat "$sf"; return 1; }
import json, sys
d = json.load(open(sys.argv[1]))
media = next((c for c in d if c.get("collection") == "media"), None)
assert media is not None, d
assert media.get("under_copied", 1) == 0, media
PY2
    TCTL report copies --json >"$cf" 2>"$cf.err" || { cat "$cf.err" "$cf"; return 1; }
    python3 - "$cf" <<'PY2' || { echo "policy::coverage does not see two copies:"; cat "$cf"; return 1; }
import json, sys
rows = [r for r in json.load(open(sys.argv[1])) if r["unit"].startswith("media/")]
assert rows, "no media/* units in report copies"
for r in rows:
    assert r["copies"] == 2, r
PY2
}

# Only NOW does staging release -- and it takes the operator asking. `volume
# write` never calls `clean_staging`; `collection run` did not because #238's
# gate held. This is the third state, after "retained" and "consumed".
csc_staging_released() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl staging clean; assert every stage set is 'cleaned' and no .age file survives under the staging directory"; return 0; }
    local f="$RUN/log-csc.clean.txt" lf="$RUN/log-csc.staged-final.json"
    # Prove there is something to release before releasing it, so "everything
    # is cleaned afterwards" cannot be satisfied by "everything was already
    # cleaned beforehand" (same vacuity trap as csc_fingerprint's guard).
    TCTL stage list --json >"$lf" 2>"$lf.err" || { cat "$lf.err" "$lf"; return 1; }
    python3 - "$lf" <<'PY2' || { echo "nothing was still staged before staging clean ran -- the release happened earlier than the second copy:"; cat "$lf"; return 1; }
import json, sys
rows = json.load(open(sys.argv[1]))
assert rows and all(r["status"] == "staged" for r in rows), rows
PY2
    TCTL staging clean >"$f" 2>&1 || { cat "$f"; return 1; }
    TCTL stage list --json >"$lf" 2>"$lf.err" || { cat "$lf.err" "$lf"; return 1; }
    python3 - "$lf" <<'PY2' || { echo "stage sets are not 'cleaned' after both copies sealed:"; cat "$lf"; return 1; }
import json, sys
rows = json.load(open(sys.argv[1]))
assert rows, rows
for r in rows:
    assert r["status"] == "cleaned", r
PY2
    local staging_dir; staging_dir="$(dirname "$HOME_DIR")/staging"
    local left; left="$(find "$staging_dir" -name '*.age' 2>/dev/null | wc -l)"
    [ "$left" = 0 ] || { echo "$left .age file(s) survived staging clean under $staging_dir"; find "$staging_dir" -name '*.age'; return 1; }
}

scenario_collection_second_copy() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        check csc.scenario csc_skip_single_cartridge
        return 0
    fi

    check csc.setup               col_setup
    check csc.sync                col_sync_registers_four
    check csc.multi_label_refused csc_multi_label_refused
    check csc.run_first_copy      csc_run_first_copy
    check csc.capture_staged      csc_capture_staged
    check csc.second_copy         csc_second_copy
    check csc.same_staged_bytes   csc_same_staged_bytes
    check csc.two_copies          csc_two_copies

    # A second copy that cannot be restored from is not a copy. The matrix
    # runs against VOL-CS2 -- the one written by the per-copy invocation, not
    # by `collection run`.
    restore_matrix VOL-CS2 media/alpha alice "$SRC/col-root/alpha" csc-alpha

    check csc.staging_released    csc_staging_released
}
run_scenario() { # run_scenario <name>
    local name="$1" fn="scenario_${1//-/_}"
    HOME_DIR="$RUN/$name/home"
    CFG="$HOME_DIR/config.toml"
    SRC="$RUN/$name/src"
    ESCROW_KEY_PATH="$RUN/$name/escrow.key"
    USED_SLOTS=""
    PREV_LABEL=""
    if [ "$DRY_RUN" != 1 ]; then
        mkdir -p "$HOME_DIR" "$SRC"
    fi
    # `[ "$DRY_RUN" = 1 ]`, not `${DRY_RUN:+...}` (issue #246). The parameter
    # expansion tests whether DRY_RUN is set and NON-EMPTY, and a real run sets
    # it to "0" — non-empty — so the "PLAN: " prefix was emitted on every run
    # and never distinguished the two modes it exists to distinguish. Purely
    # cosmetic (no check, exit code or report line reads it), but `--dry-run`'s
    # contract is that its ordered trace IS the real execution order, so a
    # transcript of a real run read as a plan that was never executed.
    if [ "$DRY_RUN" = 1 ]; then
        echo "=== PLAN: scenario $name ==="
    else
        echo "=== scenario $name ==="
    fi
    "$fn"
}

write_report_header
if [ "$RUN_ALL" = 1 ]; then
    for s in "${SCENARIO_NAMES[@]}"; do run_scenario "$s"; done
else
    run_scenario "$SCENARIO"
fi

if [ "$DRY_RUN" = 1 ]; then
    exit 0
fi

write_report_checks
write_report_footer

echo
echo "== lifecycle-suite verdict =="
rc=0
fails=0
skips=0
for n in "${CHECKS[@]}"; do
    case "${RESULT[$n]}" in
        FAIL) fails=$((fails + 1)); rc=1 ;;
        SKIP) skips=$((skips + 1)) ;;
    esac
done
echo "  ${#CHECKS[@]} checks: $((${#CHECKS[@]} - fails - skips)) passed, $fails failed, $skips skipped"
echo "  report: $REPORT"
if [ "$fails" -gt 0 ]; then
    echo "LIFECYCLE-SUITE RED — $(run_shape)"
else
    echo "LIFECYCLE-SUITE GREEN — $(run_shape), $skips visible skip(s)"
fi
exit $rc
