#!/usr/bin/env bash
# first-run-reentry-mhvtl.sh — issue #414's acceptance, on mhvtl: a
# `scripts/first-run.sh` run whose step-13 write is KILLED mid-tape, then
# re-entered with `--from 13`, finishes the post-write block (verify --full,
# audit, the shelf move, the Heir Kit).
#
# What it does, in order (every first-run.sh invocation is the real script
# driven by a profile, --auto, against an mhvtl drive; nothing is stubbed):
#
#   setup  discover the drive (scripts/mhvtl-device.sh --ensure-media: fails
#          closed on anything that is not an mhvtl drive), build the debug
#          binary under the shared build lock, copy it where the service user
#          can execute it, write a private profile — contrib/hosts/
#          vm-desk1-mhvtl.profile with every path moved into this run's
#          workspace — and a payload of several units of random data.
#   A      first-run.sh --to 12: the whole install, the step-12 rehearsal
#          included (step 13 refuses under --auto without one for this binary;
#          the marker is earned, never written by hand). Then a small slice
#          size (many tape entries, so the kill lands with some written and
#          some not), and the cartridge erased IN THE DRIVE (the rehearsal
#          left a sealed foreign volume on it, which init refuses: ADR-0003).
#   B      first-run.sh --from 13 in the background; a poller running as the
#          service user watches the catalog and, once at least one slice is
#          `written` and at least one is not, signals the `tapectl volume
#          write` process itself (KILL by default — the crash, power-loss
#          case; sudo cannot forward a KILL, so the signal goes to tapectl,
#          never to sudo or the script). first-run.sh must then die, and the
#          catalog must show the session cut short — the positive control:
#          without it a write that finished before the signal would take the
#          `sealed` branch below and prove nothing about resume.
#   C      first-run.sh --from 13 again, the re-entry: it must find the
#          interrupted session, `volume resume` it, and run the post-write
#          block — the volume sealed with every write completed, a full verify
#          recorded after the resume, the volume at the profile's first
#          location, the Heir Kit regenerated — and exit 0.
#   D      first-run.sh --from 13 a third time: the `sealed` path goes straight
#          to the post-write block, writes nothing, and exits 0.
#
# It never takes /tmp/tapectl-tape.lock itself: first-run.sh step 13 takes it
# with `flock -n` (and step 12's lifecycle suite takes it too), so a parent
# holding it would make every child die. It only checks the lock is free
# before it starts.
#
# Usage:
#   TAPECTL_MHVTL=1 scripts/first-run-reentry-mhvtl.sh --device /dev/tape/by-id/scsi-XYZZY_A1-nst
#   options: --signal KILL|INT  (default KILL; INT is the operator's Ctrl-C,
#                                a clean interrupt that marks the session itself)
#            --label L          (default FR414-1)
#            --unit-mib N       (MiB of random data per unit, 4 units; default 24)
#            --workspace DIR    (default /scratch/tapectl-fr414)
#            --keep             (keep the run directory even when green)
#
# Needs: mhvtl loaded, sudo, the `tapectl` service user (first-run.sh step 5
# creates it when absent), dar, age, lsscsi, mtx, mt, python3, cargo.
# Exit: 0 green, 1 a check failed (the run directory is kept), 2 a
# precondition failed (nothing was written).
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DEVICE="${TAPECTL_GATE_TAPE:-}"
SIG=KILL
LABEL=FR414-1
UNIT_MIB=24
WORKSPACE=/scratch/tapectl-fr414
KEEP=0
SVC_USER=tapectl

die() { echo "FR414 PRECONDITION FAILED: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
    case "$1" in
        --device) DEVICE="${2:-}"; shift 2 ;;
        --signal) SIG="${2:-}"; shift 2 ;;
        --label) LABEL="${2:-}"; shift 2 ;;
        --unit-mib) UNIT_MIB="${2:-}"; shift 2 ;;
        --workspace) WORKSPACE="${2:-}"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,/^set -uo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (--help lists them)" ;;
    esac
done

# ---------- preconditions: fail closed before anything opens a device ----------
[ "${TAPECTL_MHVTL:-}" = 1 ] || die "TAPECTL_MHVTL=1 not set — this scenario writes a tape and runs only against mhvtl"
[ -n "$DEVICE" ] || die "--device not given — name the mhvtl drive (e.g. /dev/tape/by-id/scsi-XYZZY_A1-nst); there is no default"
case "$SIG" in KILL|INT) ;; *) die "--signal must be KILL or INT, not '$SIG'" ;; esac
case "$UNIT_MIB" in ''|*[!0-9]*) die "--unit-mib must be a whole number" ;; esac
[ -e "$DEVICE" ] || die "$DEVICE does not exist"
# The drive must be one an mhvtl by-id link names (the real LTO-6's serial is
# not XYZZY). A path check only: nothing is opened yet.
DEV_REAL="$(readlink -f "$DEVICE")"
IS_MHVTL=0
for f in /dev/tape/by-id/*XYZZY*-nst; do
    [ -e "$f" ] && [ "$(readlink -f "$f")" = "$DEV_REAL" ] && IS_MHVTL=1
done
[ "$IS_MHVTL" = 1 ] || die "$DEVICE ($DEV_REAL) is not an mhvtl drive (no /dev/tape/by-id/*XYZZY*-nst names it) — refusing"
grep -q '^mhvtl ' /proc/modules || die "mhvtl module not loaded — systemctl start mhvtl.target"
for bin in lsscsi mtx mt dar age sha256sum python3 cargo flock sudo; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
id "$SVC_USER" >/dev/null 2>&1 || die "no '$SVC_USER' service user — run scripts/first-run.sh --from 5 --to 5 once, or create it"
sudo -n true 2>/dev/null || die "sudo needs a password here; first-run.sh and the catalog reads need non-interactive sudo"
# Free now, and NOT held by us from here on (see the header).
( flock -n 9 || exit 1 ) 9>/tmp/tapectl-tape.lock || die "another process holds /tmp/tapectl-tape.lock — a gate, a rehearsal or a fill is using a drive; wait for it"

DISCOVERY="$("$REPO/scripts/mhvtl-device.sh" --tape "$DEVICE" --ensure-media)" \
    || die "device discovery failed (above)"
eval "$DISCOVERY"
DGEN="LTO-${GEN#L}"
echo "fr414: drive=$DEVICE ($DRIVE_MODEL, $DGEN, sg=$DRIVE_SG) tape=$LOADED_TAG signal=$SIG label=$LABEL"

# ---------- workspace, binary, profile, payload ----------
RUN="$WORKSPACE/run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUN"/{bin,state,src/keepsake} || die "cannot create $RUN"
chmod 0755 "$WORKSPACE" "$RUN" "$RUN/bin" "$RUN/src" "$RUN/src/keepsake"
echo "fr414: workspace $RUN"
# -w with -E 99, as the gate does: a caller already holding the lock would
# otherwise wait on its own ancestor forever.
( cd "$REPO" && flock -w 1200 -E 99 /scratch/tapectl-build.lock cargo build --quiet )
rc=$?
[ "$rc" -eq 99 ] && die "timed out waiting for /scratch/tapectl-build.lock (run this script bare, not inside a flock)"
[ "$rc" -eq 0 ] || die "cargo build failed"
BUILT="${CARGO_TARGET_DIR:-$REPO/target}/debug/tapectl"
[ -x "$BUILT" ] || die "built binary not found at $BUILT"
install -m 0755 "$BUILT" "$RUN/bin/tapectl" || die "cannot copy the binary"
TAPECTL="$RUN/bin/tapectl"
sudo -u "$SVC_USER" -H test -x "$TAPECTL" || die "$SVC_USER cannot execute $TAPECTL (an ancestor of $RUN is not traversable?)"

OPS_GROUP=fr414-ops
PROFILE="$RUN/profile"
cat >"$PROFILE" <<EOF
# Generated by scripts/first-run-reentry-mhvtl.sh: the dev VM's mhvtl recipe,
# every path moved into this run's workspace.
. "$REPO/contrib/hosts/vm-desk1-mhvtl.profile"
HOME_DIR="$RUN/home"
TAPECTL="$TAPECTL"
DEVICE="$DEVICE"
DGEN="$DGEN"
OPERATOR="$USER"
WORK_DIR="$RUN/work"
STAGING_DIR="$RUN/staging"
DAR_BIN="$(command -v dar)"
BACKUP_DIR="$RUN/backups"
KIT_OUT="$RUN/kit"
LOCATIONS=(
  "fr414-rack|the re-entry scenario's shelf"
  "fr414-offsite|the re-entry scenario's second place"
)
COLLECTIONS=(
  "keepsake|$RUN/src/keepsake|family|1"
)
# #393 as home2 runs it: step 7 creates the group, grants it traverse where it
# lacks it, and checks status as the operator with the group.
OPS_GROUP="$OPS_GROUP"
STAGING_JOBS=2
EOF
SHELF=fr414-rack
for u in a b c d; do
    mkdir -p "$RUN/src/keepsake/$u"
    echo "unit $u of the #414 re-entry scenario" >"$RUN/src/keepsake/$u/README"
    head -c "$((UNIT_MIB * 1024 * 1024))" /dev/urandom >"$RUN/src/keepsake/$u/payload.bin"
done

# Every first-run.sh invocation: the profile, --auto, the label as a FLAG (the
# WRITE confirmation counts under --auto only when the word came from one), and
# a private XDG_STATE_HOME so its log and the rehearsal marker are this run's.
export XDG_STATE_HOME="$RUN/state"
FR_LOG="$RUN/state/tapectl/first-run.log"
first_run() { bash "$REPO/scripts/first-run.sh" --profile "$PROFILE" --auto --label "$LABEL" "$@"; }
DB="$RUN/home/tapectl.db"
# Catalog reads go through the service user (the home is 0700) and are
# read-only URIs: this script never writes the catalog.
sql() { # sql "<query>" [params...] — one line per row, its columns tab-joined
    sudo -u "$SVC_USER" -H python3 - "$DB" "$@" <<'PY'
import sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True, timeout=10)
for row in c.execute(sys.argv[2], sys.argv[3:]).fetchall():
    print("\t".join("" if v is None else str(v) for v in row))
PY
}

# ---------- check harness (the gate's shape) ----------
declare -A RESULT
CHECKS=()
check() { # check <name> <fn> [args...]
    local name="$1"; shift
    CHECKS+=("$name")
    if "$@" >"$RUN/check-$name.txt" 2>&1; then RESULT[$name]=PASS; else RESULT[$name]=FAIL; fi
    echo "  [$name] ${RESULT[$name]}"
    [ "${RESULT[$name]}" = PASS ] || sed 's/^/      /' "$RUN/check-$name.txt"
}
finish() {
    local failed=0 n
    echo
    for n in "${CHECKS[@]}"; do [ "${RESULT[$n]}" = PASS ] || failed=$((failed + 1)); done
    echo "fr414: ${#CHECKS[@]} checks, $failed failed — signal $SIG, drive $DEVICE"
    if [ "$failed" -eq 0 ] && [ "${1:-}" != stop ]; then
        echo "fr414: GREEN — issue #414's acceptance holds"
        if [ "$KEEP" = 0 ]; then sudo rm -rf "$RUN"; sudo groupdel "$OPS_GROUP" 2>/dev/null; echo "fr414: workspace removed (--keep keeps it)"; fi
        exit 0
    fi
    echo "fr414: RED — the workspace is kept for reading: $RUN (first-run log: $FR_LOG)"
    exit 1
}
# A phase whose check failed leaves the next one nothing to prove.
must() { [ "${RESULT[$1]}" = PASS ] || finish stop; }

# ---------- phase A: the install and the rehearsal ----------
echo "fr414: phase A — first-run.sh --to 12 (install + rehearsal); output $RUN/phase-A.out"
step_phase_a() {
    first_run --barcode "$LOADED_TAG" --to 12 </dev/null >"$RUN/phase-A.out" 2>&1 \
        || { tail -40 "$RUN/phase-A.out"; return 1; }
    ls "$RUN/state/tapectl/"rehearsal-ok-* >/dev/null 2>&1 \
        || { echo "step 12 recorded no rehearsal marker (was it skipped? see phase-A.out)"; return 1; }
    sudo -u "$SVC_USER" -H grep -E '^jobs *= *2$' "$RUN/home/config.toml" >/dev/null \
        || { echo "step 7 did not write STAGING_JOBS=2 as [staging] jobs"; return 1; }
    sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" config set defaults.slice_size 1M
}
check install_and_rehearsal step_phase_a
must install_and_rehearsal
# The rehearsal leaves a sealed volume of its own throwaway catalog on the
# cartridge; init refuses that (ADR-0003). Erase it IN THE DRIVE, as the gate's
# resume arms do, so step 13 starts from a blank tape.
step_blank_tape() {
    eval "$("$REPO/scripts/mhvtl-device.sh" --tape "$DEVICE" --ensure-media)" \
        && mt -f "$DEVICE" rewind && mt -f "$DEVICE" erase && mt -f "$DEVICE" rewind
}
# #393: after step 7, the operator reads `status` with the ops group alone
# (no sudo to the service user), and step 7 said it checked that itself.
step_ops_group_status() {
    local gid; gid="$(getent group "$OPS_GROUP" | cut -d: -f3)" || { echo "step 7 created no group $OPS_GROUP"; return 1; }
    sudo setpriv --reuid="$(id -u)" --regid="$gid" --clear-groups "$TAPECTL" --home "$RUN/home" status \
        || { echo "status as $USER with group $OPS_GROUP failed"; return 1; }
    # ok/note lines go to the terminal, not first-run.log: read phase A's output.
    grep -F "ops group $OPS_GROUP:" "$RUN/phase-A.out" | grep -F "checked as" >/dev/null \
        || { echo "step 7 did not report its own status check:"; grep -F "ops group" "$RUN/phase-A.out"; return 1; }
    # Positive control: without the group, the same read is refused.
    if sudo setpriv --reuid="$(id -u)" --regid="$(id -g nobody)" --clear-groups "$TAPECTL" --home "$RUN/home" status >/dev/null 2>&1; then
        echo "status worked WITHOUT the ops group: the check above proves nothing"; return 1
    fi
}
check ops_group_status step_ops_group_status
check blank_tape step_blank_tape
must blank_tape

# ---------- phase B: the write, cut short ----------
echo "fr414: phase B — first-run.sh --from 13, $SIG once the write is part-way; output $RUN/phase-B.out"
step_phase_b() {
    local fr_pid poll_rc fr_rc
    first_run --from 13 --to 13 </dev/null >"$RUN/phase-B.out" 2>&1 &
    fr_pid=$!
    # The poller runs as the service user so it can read the 0700 catalog and
    # signal the tapectl process (same uid) the moment the state is right.
    sudo -u "$SVC_USER" -H python3 - "$DB" "$LABEL" "$SIG" "$fr_pid" <<'PY'
import os, signal, sqlite3, sys, time
db, label, sig, fr_pid = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
deadline = time.time() + 3600  # staging a few hundred MiB on a debug build, then the write
Q = """SELECT SUM(wp.status IN ('written','verified')), SUM(wp.status IN ('pending','writing'))
       FROM write_positions wp JOIN writes w ON w.id = wp.write_id
       JOIN volumes v ON v.id = w.volume_id
       WHERE v.label = ? AND w.status = 'in_progress'"""
def writer_pid():
    me = os.getuid()
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            if os.stat(f"/proc/{d}").st_uid != me:
                continue
            argv = [a.decode("utf-8", "replace")
                    for a in open(f"/proc/{d}/cmdline", "rb").read().split(b"\0") if a]
        except OSError:
            continue
        if argv and os.path.basename(argv[0]) == "tapectl" and "volume" in argv:
            i = argv.index("volume")
            if argv[i + 1:i + 3] == ["write", label]:
                return int(d)
    return None
while time.time() < deadline:
    if not os.path.exists(f"/proc/{fr_pid}"):
        print("first-run.sh exited before the write was part-way: nothing was cut short")
        sys.exit(1)
    row = None
    try:
        c = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=1)
        row = c.execute(Q, (label,)).fetchone()
        c.close()
    except sqlite3.Error:
        pass
    if row and (row[0] or 0) >= 1 and (row[1] or 0) >= 1:
        pid = writer_pid()
        if pid:
            os.kill(pid, getattr(signal, "SIG" + sig))
            print(f"signalled tapectl pid {pid} with SIG{sig}: {row[0]} slice(s) written, {row[1]} not yet")
            sys.exit(0)
    time.sleep(0.05)
print("TIMEOUT: the write never reached a part-way state within the hour")
sys.exit(1)
PY
    poll_rc=$?
    wait "$fr_pid"; fr_rc=$?
    echo "first-run.sh exited $fr_rc"
    [ "$poll_rc" -eq 0 ] || return 1
    # first-run.sh must have died at the write, not carried on.
    [ "$fr_rc" -ne 0 ] || { echo "first-run.sh exited 0 although its write was signalled"; return 1; }
    grep "write did not seal" "$RUN/phase-B.out" >/dev/null \
        || { echo "phase-B.out does not show step 13 dying at the write"; tail -30 "$RUN/phase-B.out"; return 1; }
}
check write_cut_short step_phase_b
must write_cut_short
# The positive control: the catalog must show the session cut short. A KILL
# leaves the rows `in_progress` (nothing could mark them; the startup sweep
# turns them `interrupted` at the next catalog open, which is step 13's own
# `volume info`); an INT is a clean interrupt that marks them itself.
step_session_cut_short() {
    local vstatus by written pending want
    vstatus="$(sql "SELECT status FROM volumes WHERE label = ?" "$LABEL")"
    by="$(sql "SELECT w.status || '=' || COUNT(*) FROM writes w JOIN volumes v ON v.id = w.volume_id
               WHERE v.label = ? GROUP BY w.status" "$LABEL" | tr '\n' ' ')"
    written="$(sql "SELECT COUNT(*) FROM write_positions wp JOIN writes w ON w.id = wp.write_id
                    JOIN volumes v ON v.id = w.volume_id
                    WHERE v.label = ? AND wp.status IN ('written','verified')" "$LABEL")"
    pending="$(sql "SELECT COUNT(*) FROM write_positions wp JOIN writes w ON w.id = wp.write_id
                    JOIN volumes v ON v.id = w.volume_id
                    WHERE v.label = ? AND wp.status IN ('pending','writing')" "$LABEL")"
    echo "volume $LABEL: status=$vstatus writes: $by slices written=$written not written=$pending"
    [ "$vstatus" = initialized ] || { echo "expected the volume still 'initialized' (not sealed)"; return 1; }
    [ "$SIG" = KILL ] && want=in_progress || want=interrupted
    case " $by" in *" $want="*) ;; *) echo "expected a '$want' write row after SIG$SIG"; return 1 ;; esac
    [ "$written" -ge 1 ] || { echo "expected at least one slice written before the signal"; return 1; }
}
check session_cut_short step_session_cut_short
must session_cut_short

# ---------- phase C: the re-entry ----------
max_verify_id() { sql "SELECT COALESCE(MAX(s.id), 0) FROM verification_sessions s
                       JOIN volumes v ON v.id = s.volume_id WHERE v.label = ?" "$LABEL"; }
writes_count() { sql "SELECT COUNT(*) FROM writes w JOIN volumes v ON v.id = w.volume_id WHERE v.label = ?" "$LABEL"; }
# The post-write block, as the catalog and the kit record it, after a phase
# that began when the newest verification session was $1 and $2 is a file
# created when the phase began.
assert_post_write_block() { # assert_post_write_block <verify-id-before> <phase-start-file> <phase-out>
    local before="$1" since="$2" out="$3" bad sess loc
    bad="$(sql "SELECT w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
                WHERE v.label = ? AND w.status <> 'completed'" "$LABEL" | tr '\n' ' ')"
    [ "$(sql "SELECT status FROM volumes WHERE label = ?" "$LABEL")" = sealed ] \
        || { echo "volume $LABEL is not sealed"; return 1; }
    [ -z "${bad// /}" ] || { echo "writes not all completed: $bad"; return 1; }
    # The post-write `volume verify --full` is the phase's last tape contact,
    # so the newest session is it: full, passed, and newer than the phase's
    # start. (A full confirm records a full session too, which is why "a full
    # session exists" would prove nothing.)
    sess="$(sql "SELECT s.id || ' ' || s.verify_type || ' ' || s.outcome FROM verification_sessions s
                 JOIN volumes v ON v.id = s.volume_id WHERE v.label = ? ORDER BY s.id DESC LIMIT 1" "$LABEL")"
    echo "newest verification session: $sess (newest before this phase: $before)"
    local sid stype soutcome
    read -r sid stype soutcome <<<"$sess"
    [ "${sid:-0}" -gt "$before" ] && [ "${stype:-}" = full ] && [ "${soutcome:-}" = passed ] \
        || { echo "no full, passed verification recorded by this phase"; return 1; }
    grep "$LABEL sealed and verified" "$out" >/dev/null || { echo "$out does not say '$LABEL sealed and verified'"; return 1; }
    grep -E '^ +\$ .* audit$' "$out" >/dev/null || { echo "$out shows no audit run"; return 1; }
    loc="$(sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" volume info "$LABEL" --json \
           | python3 -c 'import json,sys; print(json.load(sys.stdin).get("location") or "")')"
    [ "$loc" = "$SHELF" ] || { echo "volume $LABEL is at '$loc', expected $SHELF"; return 1; }
    [ "$RUN/kit/COVER.txt" -nt "$since" ] || { echo "the Heir Kit was not regenerated ($RUN/kit/COVER.txt not newer than the phase)"; return 1; }
    echo "post-write block complete: sealed, verified in full, audited, at $loc, kit regenerated"
}

echo "fr414: phase C — the re-entry, first-run.sh --from 13; output $RUN/phase-C.out"
step_phase_c() {
    local before
    before="$(max_verify_id)"; touch "$RUN/phase-C.start"; sleep 1
    first_run --from 13 </dev/null >"$RUN/phase-C.out" 2>&1 \
        || { echo "the re-entry exited non-zero"; tail -40 "$RUN/phase-C.out"; return 1; }
    grep "step 13: volume $LABEL is resume" "$FR_LOG" >/dev/null \
        || { echo "step 13 did not classify $LABEL as an interrupted write (first-run log)"; return 1; }
    grep -E "^ +\\\$ .* volume resume $LABEL " "$RUN/phase-C.out" >/dev/null \
        || { echo "phase-C.out shows no 'volume resume $LABEL'"; return 1; }
    if grep -E "^ +\\\$ .* (volume write|stage create|snapshot create) " "$RUN/phase-C.out" >/dev/null; then
        echo "the re-entry staged or wrote again instead of resuming"; return 1
    fi
    assert_post_write_block "$before" "$RUN/phase-C.start" "$RUN/phase-C.out"
}
check reentry_resumes_and_finishes step_phase_c
must reentry_resumes_and_finishes

# ---------- phase D: the sealed path ----------
echo "fr414: phase D — first-run.sh --from 13 on the sealed volume; output $RUN/phase-D.out"
step_phase_d() {
    local before nwrites
    before="$(max_verify_id)"; nwrites="$(writes_count)"; touch "$RUN/phase-D.start"; sleep 1
    first_run --from 13 --to 13 </dev/null >"$RUN/phase-D.out" 2>&1 \
        || { echo "the sealed re-entry exited non-zero"; tail -40 "$RUN/phase-D.out"; return 1; }
    grep "step 13: volume $LABEL is sealed" "$FR_LOG" >/dev/null \
        || { echo "step 13 did not classify $LABEL as sealed (first-run log)"; return 1; }
    if grep -E "^ +\\\$ .* (volume write|volume resume|volume init|stage create) " "$RUN/phase-D.out" >/dev/null; then
        echo "the sealed re-entry touched the write path"; return 1
    fi
    [ "$(writes_count)" = "$nwrites" ] || { echo "the sealed re-entry added write rows"; return 1; }
    assert_post_write_block "$before" "$RUN/phase-D.start" "$RUN/phase-D.out"
}
check sealed_reentry_runs_only_the_post_write_block step_phase_d

finish
