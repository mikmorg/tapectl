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
                                   short = mt rewind+weof 1+rewind (empty File 0 at BOT).
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
    cargo build --quiet || die "cargo build failed"
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

# ---------- erase_tape: the ONE place scenarios reuse a tape ----------
erase_tape() {
    case "$ERASE_MODE" in
        long)  devcmd mt -f "$TAPE_DEV" rewind && devcmd mt -f "$TAPE_DEV" erase ;;
        short) devcmd mt -f "$TAPE_DEV" rewind && devcmd mt -f "$TAPE_DEV" weof 1 && devcmd mt -f "$TAPE_DEV" rewind ;;
    esac
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
        # The DB has no other way to learn the previous volume's cartridge is
        # gone: `volumes.status` stays 'sealed' forever unless something
        # retires it, and ADR-0004's copy derivation is DB-status-only. Retire
        # it here so copy-count/mark-tape-only/audit see the truth a real
        # single-cartridge operator lives with, instead of crediting a
        # cartridge that no longer physically exists.
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
    erase_tape
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

# ---------- leak-scan media path (mhvtl only; real drive -> caller SKIPs) ----------
mhvtl_media_dir() {
    [ "$MHVTL_DISCOVERY" = 1 ] || return 1
    [ -n "${LOADED_TAG:-}" ] || return 1
    local guess
    for guess in "/scratch/mhvtl/$LOADED_TAG" "/opt/mhvtl/$LOADED_TAG"; do
        [ -d "$guess" ] && { echo "$guess"; return 0; }
    done
    return 1
}

# ---------- globals shared by fixtures / restore matrix ----------
# CANARY: embedded in one file per unit (like the gate) for the leak scan.
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
files = sorted(p for p in root.rglob("*") if p.is_file() and not p.is_symlink())

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

rm_step_restore_sh_primary() {
    ensure_heir_restore_sh || return 1
    local key to="$RM_WORK/primary"
    key="$(active_key_path "$RM_TENANT" primary)"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit $RM_UNIT --key $key --to $to"
        return 0
    fi
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit "$RM_UNIT" --key "$key" --to "$to") \
        >"$RM_WORK/restore_primary.txt" 2>&1 || { cat "$RM_WORK/restore_primary.txt"; return 1; }
    assert_identical "$RM_SRC" "$to"
}

rm_step_restore_sh_backup() {
    ensure_heir_restore_sh || return 1
    local key to="$RM_WORK/backup"
    key="$(active_key_path "$RM_TENANT" backup)"
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: ./RESTORE.sh --restore --unit $RM_UNIT --key $key --to $to (proves the backup key is a real recipient)"
        return 0
    fi
    [ -f "$key" ] || { echo "tenant backup key missing: $key"; return 1; }
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit "$RM_UNIT" --key "$key" --to "$to") \
        >"$RM_WORK/restore_backup.txt" 2>&1 || { cat "$RM_WORK/restore_backup.txt"; return 1; }
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
    TCTL restore raw-volume --to "$to" --device "$TAPE_DEV" --json >"$RM_WORK/raw.json" 2>&1
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
       ORDER BY sl.slice_number LIMIT 1""",
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
    TCTL volume verify "$RM_LABEL" --full --device "$TAPE_DEV" --json >"$RM_WORK/verify_full.json" 2>&1 || { cat "$RM_WORK/verify_full.json"; return 1; }
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0 and d.get("passed",0)>0, d' "$RM_WORK/verify_full.json" || return 1
    TCTL volume verify "$RM_LABEL" --quick --device "$TAPE_DEV" --json >"$RM_WORK/verify_quick.json" 2>&1 || { cat "$RM_WORK/verify_quick.json"; return 1; }
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0, d' "$RM_WORK/verify_quick.json" || return 1
    TCTL report verify-status --json >"$RM_WORK/verify_status.json" 2>&1 || return 1
    grep -q "\"$RM_LABEL\"" "$RM_WORK/verify_status.json" || { echo "volume $RM_LABEL not listed in report verify-status"; return 1; }
}

# restore_matrix <label> <unit> <tenant> <expected_src_dir> <tag> [other_tenant]
# Runs all 10 methods as separate `check`s named "<tag>.<method>". Call
# after a volume is sealed and while its cartridge is (or can be) reloaded.
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
    TCTL init --operator "$OPERATOR" || return 1
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: python3 rewrites $CFG (dar binary=dar, slice_size=1M, staging dir, backends.lto entry for $TAPE_DEV, compaction.utilization_threshold=0.95)"
        return 0
    fi
    local scenario_dir staging_dir
    scenario_dir="$(dirname "$HOME_DIR")"
    staging_dir="$scenario_dir/staging"
    mkdir -p "$staging_dir"
    python3 - "$CFG" "$staging_dir" "$TAPE_DEV" "$DRIVE_SG" <<'PY'
import re
import sys

cfg, staging, tape, sg = sys.argv[1:5]
t = open(cfg).read()
t = re.sub(r'(?m)^binary *=.*$', 'binary = "dar"', t, count=1)
t = re.sub(r'(?m)^slice_size *=.*$', 'slice_size = "1M"', t, count=1)
t = re.sub(r'(?m)^directory *=.*$', f'directory = "{staging}"', t, count=1)
t = re.sub(r'(?m)^utilization_threshold *=.*$', 'utilization_threshold = 0.95', t, count=1)
if "[[backends.lto]]" not in t:
    t = re.sub(r'(?m)^lto *= *\[\] *\n', "", t)
    t += f'''
[[backends.lto]]
name = "lifecycle"
device_tape = "{tape}"
device_sg = "{sg}"
media_type = "LTO-6"
nominal_capacity = "2.5T"
usable_capacity_factor = 0.95
manifest_reserve = "1G"
enospc_buffer = "2G"
block_size = "512K"
hardware_compression = false
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
    TCTL volume init "$label" --device "$TAPE_DEV" || return 1
    TCTL volume write "$label" --device "$TAPE_DEV" || return 1
    TCTL volume move "$label" --to vault || return 1
    if [ "$DRY_RUN" != 1 ]; then
        TCTL cartridge register --barcode "$LOADED_TAG" --media-type LTO-6 || return 1
    fi
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
    TCTL volume init VOL-B --device "$TAPE_DEV" || return 1
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
    TCTL volume init VOL-A --device "$TAPE_DEV" \
    && TCTL volume write VOL-A --device "$TAPE_DEV"
}
fy_move()      { TCTL volume move VOL-A --to vault; }
fy_cartridge() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl cartridge register --barcode \$LOADED_TAG --media-type LTO-6"; return 0; }
    TCTL cartridge register --barcode "$LOADED_TAG" --media-type LTO-6
}
fy_audit() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl audit --json (record exit code; 0 or 1 both PASS)"; return 0; }
    TCTL audit --json >"$RUN/log-fy.audit.txt" 2>&1
    local rc=$?
    [ "$rc" -eq 0 ] || [ "$rc" -eq 1 ] || { echo "audit exited $rc (expected 0 or 1)"; return 1; }
    return 0
}
fy_fsck()      { TCTL db fsck; }
fy_summary()   { TCTL report summary; }

scenario_first_year() {
    check fy.init       fy_init
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
    TCTL report dirty --json >"$logf" 2>&1 || { cat "$logf"; return 1; }
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
    TCTL volume init VOL-B --device "$TAPE_DEV" && TCTL volume write VOL-B --device "$TAPE_DEV"
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
    TCTL key list --tenant alice --json >"$logf" 2>&1 || { cat "$logf"; return 1; }
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
    TCTL volume init VOL-C --device "$TAPE_DEV" && TCTL volume write VOL-C --device "$TAPE_DEV"
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
    TCTL unit list --tenant bob --json >"$bobf" 2>&1 || { cat "$bobf"; return 1; }
    TCTL unit list --tenant alice --json >"$alicef" 2>&1 || { cat "$alicef"; return 1; }
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
    TCTL volume init VOL-D --device "$TAPE_DEV" && TCTL volume write VOL-D --device "$TAPE_DEV"
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
    TCTL catalog locate photos --json >"$logf" 2>&1 || { cat "$logf"; return 1; }
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
    TCTL volume init VOL-SOLO --device "$TAPE_DEV" && TCTL volume write VOL-SOLO --device "$TAPE_DEV"
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

tor_staging_clean() { TCTL staging clean; }
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
    check tor.setup                        bootstrap_two_volumes
    check tor.solo_unit                    tor_solo_unit
    check tor.mark_tape_only_photos_passes tor_mark_tape_only_photos_passes
    check tor.mark_tape_only_solo_refused  tor_mark_tape_only_solo_refused
    check tor.report_tape_only             tor_report_tape_only
    check tor.mark_reclaimable_v1_photos   tor_mark_reclaimable_v1_photos
    check tor.purge_v1_photos              tor_purge_v1_photos
    check tor.staging_clean                tor_staging_clean
    check tor.report_copies                tor_report_copies
    check tor.restore_latest               tor_restore_latest
}
# ============================================================
# Scenario: compaction (mhvtl-only)
# ============================================================
# Needs THREE simultaneously-distinct volumes (VOL-E, VOL-F, VOL-G) to mean
# anything — impossible under --single-cartridge, which destroys each
# previous volume's cartridge on next_tape (see tape-only-and-reclaim's
# next_tape fix). The whole scenario SKIPs there, visibly, rather than
# faking a single-cartridge shape that wouldn't test compaction at all.
#
# Sequence: VOL-E starts with photos/docs/big v1. photos v2 goes to VOL-F,
# which makes photos v1 on VOL-E supersedable; mark it reclaimable and
# purge it, leaving docs v1 and big v1 as VOL-E's only live content — under
# bootstrap_config's utilization_threshold=0.95 that is enough for
# `report compaction-candidates` to flag VOL-E. compact-read pulls those
# live slices to staging; compact-finish is asserted to REFUSE before
# compact-write has given docs/big a copy anywhere else, then to SUCCEED
# once VOL-G holds one.
cp_skip_single_cartridge() {
    skip "cp.scenario" "compaction needs 3 simultaneously-distinct volumes (VOL-E/F/G) — impossible under --single-cartridge"
    return $?
}

cp_write_photos_v2_on_volf() {
    mutate_source "$SRC/photos" "$SEED" modify || return 1
    TCTL snapshot create photos || return 1
    TCTL stage create photos || return 1
    next_tape VOL-F || return 1
    TCTL volume init VOL-F --device "$TAPE_DEV" && TCTL volume write VOL-F --device "$TAPE_DEV"
}

cp_reclaim_v1_photos() {
    TCTL snapshot mark-reclaimable --version 1 photos && TCTL snapshot purge --version 1 photos
}

cp_compaction_candidates_lists_vole() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl report compaction-candidates --json (assert VOL-E listed)"; return 0; }
    local logf="$RUN/log-cp.candidates.json"
    TCTL report compaction-candidates --json >"$logf" 2>&1 || { cat "$logf"; return 1; }
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

cp_write_volg() {
    next_tape VOL-G || return 1
    TCTL volume compact-write --destination VOL-G --device "$TAPE_DEV"
}

cp_compact_finish_succeeds() { TCTL volume compact-finish VOL-E; }

scenario_compaction() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        check cp.scenario cp_skip_single_cartridge
        return 0
    fi

    check cp.setup                     bootstrap_archive_v1 VOL-E
    check cp.write_photos_v2_on_volf   cp_write_photos_v2_on_volf
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

rr_retire_refused_sole_copy() {
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume retire VOL-A (no --yes; sole copy -> refused, impact analysis names a ZERO-copy unit)"; return 0; }
    local out rc
    out="$(TCTL volume retire VOL-A 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] || { echo "volume retire VOL-A unexpectedly succeeded without consent while it is the sole copy: $out"; return 1; }
    echo "$out" | grep -qi "ZERO copies remaining" || { echo "impact analysis did not name a zero-copy unit ('ZERO copies remaining'): $out"; return 1; }
}

# A second COPY of the same v1 content (re-stage the same version, write
# again) — not a new version, and not `volume read-slices` (which MOVES
# slices into staging for a follow-on write, self-describing invariant
# preserved, rather than duplicating them).
rr_write_second_copy_volb() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.write_second_copy_volb" "single-cartridge mode cannot hold a second, independent copy of VOL-A's content"
        return $?
    fi
    TCTL stage create photos --version 1 || return 1
    TCTL stage create docs --version 1 || return 1
    TCTL stage create big --version 1 || return 1
    next_tape VOL-B || return 1
    TCTL volume init VOL-B --device "$TAPE_DEV" && TCTL volume write VOL-B --device "$TAPE_DEV"
}

rr_retire_vola_succeeds_with_coverage() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.retire_vola_succeeds_with_coverage" "depends on rr.write_second_copy_volb, itself SKIP under --single-cartridge"
        return $?
    fi
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl volume retire VOL-A (now safe: every unit has a second copy on VOL-B, so no consent is needed)"; return 0; }
    TCTL volume retire VOL-A
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
rr_physical_erase() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.physical_erase" "single-cartridge mode: VOL-A's cartridge was already reused by an earlier next_tape in this run"
        return $?
    fi
    erase_tape
}

rr_mark_erased_after_retire_succeeds() {
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        skip "rr.mark_erased_after_retire_succeeds" "depends on rr.physical_erase, itself SKIP under --single-cartridge"
        return $?
    fi
    [ "$DRY_RUN" = 1 ] && { echo "PLAN: tapectl cartridge mark-erased \$barcode (pending_erase since the retire above -> succeeds, no consent needed)"; return 0; }
    [ -n "${RR_VOLA_BARCODE:-}" ] || { echo "no barcode captured for VOL-A's cartridge"; return 1; }
    TCTL cartridge mark-erased "$RR_VOLA_BARCODE"
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
    RR_VOLA_BARCODE="$LOADED_TAG"

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
scenario_db_loss() { echo "PLAN: [db-loss] not yet implemented"; }
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
    TCTL volume init VOL-EO --device "$TAPE_DEV" && TCTL volume write VOL-EO --device "$TAPE_DEV"
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
scenario_restore_file_and_catalog() { echo "PLAN: [restore-file-and-catalog] not yet implemented"; }
scenario_quick_archive() { echo "PLAN: [quick-archive] not yet implemented"; }
scenario_collection() { echo "PLAN: [collection] not yet implemented"; }
scenario_permute() { echo "PLAN: [permute] seed=$SEED steps=$STEPS not yet implemented"; }

# ---------- REPORT.md ----------
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
    echo "=== ${DRY_RUN:+PLAN: }scenario $name ==="
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
    echo "LIFECYCLE-SUITE RED"
else
    echo "LIFECYCLE-SUITE GREEN ($skips visible skip(s))"
fi
exit $rc
