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
        echo "PLAN: next_tape \"$label\" -> $([ "$SINGLE_CARTRIDGE" = 1 ] && echo "reuse loaded cartridge (single-cartridge mode)" || echo "unload current, load next unused $GEN slot")"
        erase_tape
        return 0
    fi
    if [ "$SINGLE_CARTRIDGE" = 1 ]; then
        echo "next_tape: single-cartridge mode — erasing $LOADED_TAG in place for \"$label\""
        erase_tape
        echo "$LOADED_TAG	$label	SAME_CARTRIDGE" >>"$SLOT_LABEL_MAP"
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
# ensure_heir_restore_sh — dd RESTORE.sh off the currently loaded tape into
# $RM_WORK/heir, once. Later matrix steps reuse it; self-healing if an
# earlier step failed to extract it.
ensure_heir_restore_sh() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "PLAN: mt rewind; setblk 524288; fsf 2; dd if=$TAPE_DEV bs=512k | tr -d '\\0' > RESTORE.sh; chmod +x; bash -n"
        return 0
    fi
    [ -f "$RM_WORK/heir/RESTORE.sh" ] && return 0
    mkdir -p "$RM_WORK/heir"
    devcmd mt -f "$TAPE_DEV" rewind || return 1
    devcmd mt -f "$TAPE_DEV" setblk 524288 || return 1
    devcmd mt -f "$TAPE_DEV" fsf 2 || return 1
    dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' > "$RM_WORK/heir/RESTORE.sh"
    chmod +x "$RM_WORK/heir/RESTORE.sh"
    bash -n "$RM_WORK/heir/RESTORE.sh"
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
    local key="$HOME_DIR/keys/$RM_TENANT-primary.age.key" to="$RM_WORK/primary"
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
    local key="$HOME_DIR/keys/$RM_TENANT-backup.age.key" to="$RM_WORK/backup"
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
    local opkey="$HOME_DIR/keys/${OPERATOR}-primary.age.key" to="$RM_WORK/operator"
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
        echo "PLAN: ./RESTORE.sh --restore --unit $RM_UNIT --key \$RUN/escrow.key --to $to"
        return 0
    fi
    [ -f "$RUN/escrow.key" ] || { echo "no escrow.key captured for this run — was key generate --escrow run?"; return 1; }
    (cd "$RM_WORK/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit "$RM_UNIT" --key "$RUN/escrow.key" --to "$to") \
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
    local otherkey="$HOME_DIR/keys/$other-primary.age.key"
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

# ---------- scenario stubs ----------
# Each is replaced with a real implementation in a later commit. Kept as
# real (if minimal) functions from the start so --list/--dry-run/--all can
# already enumerate and plan every scenario name.
scenario_first_year() { echo "PLAN: [first-year] not yet implemented"; }
scenario_evolving_source() { echo "PLAN: [evolving-source] not yet implemented"; }
scenario_key_rotation() { echo "PLAN: [key-rotation] not yet implemented"; }
scenario_tenant_reassign() { echo "PLAN: [tenant-reassign] not yet implemented"; }
scenario_tape_only_and_reclaim() { echo "PLAN: [tape-only-and-reclaim] not yet implemented"; }
scenario_compaction() { echo "PLAN: [compaction] not yet implemented"; }
scenario_retire_and_reuse() { echo "PLAN: [retire-and-reuse] not yet implemented"; }
scenario_db_loss() { echo "PLAN: [db-loss] not yet implemented"; }
scenario_escrow_ordering() { echo "PLAN: [escrow-ordering] not yet implemented"; }
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
run_scenario() { # run_scenario <name>
    local fn="scenario_${1//-/_}"
    echo "=== ${DRY_RUN:+PLAN: }scenario $1 ==="
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
