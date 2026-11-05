#!/usr/bin/env bash
# first-run-reentry-mhvtl.sh — issue #414's acceptance, on mhvtl: a
# `scripts/first-run.sh` run whose step-13 write is KILLED mid-tape, then
# re-entered with `--from 13`, finishes the copy (`volume resume` with its full
# readback, the tape's first verify: ADR-0012 item 218) and the steps after it
# (the shelf move, the audit, the Heir Kit).
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
#          interrupted session, `volume resume` it with --full-confirm, and run
#          the post-write block — the volume sealed with every write completed,
#          a full readback recorded by the resume's own confirm (and no second
#          read: no `volume verify`), the volume at the profile's first
#          location, the Heir Kit regenerated — and exit 0.
#   D      first-run.sh --from 13 a third time: the `sealed` path finds the full
#          readback recorded (`report verify-status`) and reads nothing (OQ15):
#          no tape command at all, no new verification session; it goes
#          straight to the shelf, the audit and the kit, and exits 0.
#   E      copy 2, as the operator runs it after copy 1 (docs/handoff.md (i)
#          step 8): `--from 13 --label <L2> --location <the second place>`, with
#          one volume in the catalog and every unit still staged (one copy, a
#          copy floor of 2: the precondition check says so first).
#          E0  this catalog's own sealed L1 is still in the drive (and the
#              run names it with --erase-label, consent that must not reach
#              it): `volume init L2` refuses it as the catalog's live copy,
#              step 13 dies naming the eject, and nothing is erased or
#              written — File 0 the same bytes, no catalog row added.
#          E1  L1 goes back to its slot and the HIGHEST-numbered cartridge of
#              the drive's generation is loaded (the chain's upgrade harness
#              takes them from the low end), and a throwaway home of the run's
#              own binary writes and seals a volume FOREIGN to this catalog on
#              it, labelled L2 (as on home2, where the old tape's File 0 label
#              is the new label). Then copy 2: the "1 volume(s) already in
#              the catalog" confirm, nothing snapshotted or staged again,
#              init's refusal of the foreign sealed volume, `cartridge erase`
#              with the consent --erase-label L2 names, init with no --force
#              on the verified erase, the write with --full-confirm and
#              --move-to, its full readback and no `volume verify`, the
#              cartridge's barcode L2 (item 219), every unit's staging
#              released (the copy floor met), the audit and the kit, exit 0;
#              both volumes sealed, every unit 2 copies in 2 places, and step 14
#              asking for no third copy (phase C's step 14, one copy short, does
#              ask: the control). Then the cartridge goes back to its slot.
#          Under --auto every prompt of phase E is answered by first-run.sh
#          itself or by a flag (--label, --erase-label): its typed-word
#          prompts read /dev/tty, never stdin, so nothing is fed on stdin.
#
# Issue #432 rides on the same run: tape commands run at raised CPU and I/O
# priority from every launcher. Before phase A a `tapectl_op_priority` check runs
# scripts/check-tapectl-op.sh (the wrapper against a probe: no tape). first-run.sh
# raises its OWN shell once at the start of step 13 (sudo renice/ionice) and every
# command it starts inherits that, so in each phase a sampler running as the
# service user records the nice value and I/O class of each tapectl process the
# run starts (RUN/prio-<phase>.tsv), and a check per phase asserts: `volume write`
# (B) and `volume resume` (C) are at the profile's TAPE_NICE/TAPE_IONICE, phase D
# (which reads nothing, OQ15) starts no tape command at all, and the step's own
# notice says its shell went from the harness's priority to the profile's. Phase B also reads the write's
# priority from /proc/PID/stat and `ionice -p` just before it signals. The
# negative side is phase A, steps 1-12: every tapectl process the sampler sees is
# at the harness's own priority (non-tape commands INSIDE step 13 inherit the
# raise and are not judged), and a dummy `tapectl` the harness starts at base
# proves the sampler could see one.
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
#            --label2 L         (phase E's copy 2; default FR414-2)
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
LABEL2=FR414-2
UNIT_MIB=24
WORKSPACE=/scratch/tapectl-fr414
KEEP=0
SVC_USER=tapectl
# Issue #432: the profile's tape priority, and what `ionice -p` prints for it.
TAPE_NICE_RUN=-10
TAPE_IONICE_RUN="-c2 -n0"
WANT_IO="best-effort: prio 0"

die() { echo "FR414 PRECONDITION FAILED: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
    case "$1" in
        --device) DEVICE="${2:-}"; shift 2 ;;
        --signal) SIG="${2:-}"; shift 2 ;;
        --label) LABEL="${2:-}"; shift 2 ;;
        --label2) LABEL2="${2:-}"; shift 2 ;;
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
{ [ -n "$LABEL2" ] && [ "$LABEL2" != "$LABEL" ]; } || die "--label2 must name a label other than --label ($LABEL)"
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
# The positive control only means something if this shell is NOT already at the
# raised priority: a raised tape command and an unraised one would look alike.
BASE_NICE="$(nice)"; BASE_IO="$(ionice -p $$ | tr -d '\n')"
{ [ "$BASE_NICE" != "$TAPE_NICE_RUN" ] && [ "$BASE_IO" != "$WANT_IO" ]; } \
    || die "this shell already runs at nice $BASE_NICE / '$BASE_IO', the priority a tape command is raised to; run the scenario from a shell at normal priority"
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
CONTENDER_PROCESSES=(cargo rustc docker Runner.Worker java)
# #432: tape commands at raised priority, set explicitly so the assertions below
# compare against what the profile says, not against a default.
TAPE_NICE=$TAPE_NICE_RUN
TAPE_IONICE="$TAPE_IONICE_RUN"
EOF
# The #432 probe, run as the service user (its processes are the only ones whose
# /proc entries it needs): helper functions for phase B's poller, and, run
# directly, a sampler that records every tapectl process once, with the nice
# value and I/O class it was started with, until a stop file appears.
cat >"$RUN/tapectl_prio.py" <<'PY'
import os, subprocess, sys, time

def tapectl_procs(uid):
    """(pid, argv) of each process of uid whose argv[0] is a path ending in tapectl."""
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            if os.stat(f"/proc/{d}").st_uid != uid:
                continue
            argv = [a.decode("utf-8", "replace")
                    for a in open(f"/proc/{d}/cmdline", "rb").read().split(b"\0") if a]
        except OSError:
            continue
        if argv and os.path.basename(argv[0]) == "tapectl":
            yield int(d), argv

def words(argv):
    """The first two words after the binary that are not options (--home and
    --config take a value: the CLI's only two such global options)."""
    out, skip = [], False
    for a in argv[1:]:
        if skip:
            skip = False
        elif a in ("--home", "--config"):
            skip = True
        elif not a.startswith("-"):
            out.append(a)
            if len(out) == 2:
                break
    return " ".join(out)

def nice_of(pid):
    """Field 19 of /proc/PID/stat; the fields after the ')' that ends the
    command name start at field 3."""
    with open(f"/proc/{pid}/stat") as f:
        st = f.read()
    return int(st[st.rindex(")") + 2:].split()[16])

def io_of(pid):
    """ionice -p PID, e.g. 'best-effort: prio 0'; '' if the process is gone."""
    r = subprocess.run(["ionice", "-p", str(pid)], capture_output=True, text=True)
    return r.stdout.strip()

if __name__ == "__main__":
    stop, uid, seen = sys.argv[1], os.getuid(), set()
    while not os.path.exists(stop):
        for pid, argv in tapectl_procs(uid):
            if pid in seen:
                continue
            try:
                n, io = nice_of(pid), io_of(pid)
            except (OSError, ValueError, IndexError):
                continue
            if io:
                seen.add(pid)
                print(f"{pid}\t{words(argv)}\t{n}\t{io}", flush=True)
        time.sleep(0.02)
PY
chmod 0644 "$RUN/tapectl_prio.py"
# The sampler's own positive control: a long-lived process named `tapectl` (a copy
# of sleep) that the harness starts at its own priority. Phase A's sampler must
# record it, or its silence about the real commands proves nothing.
mkdir -p "$RUN/dummy" && chmod 0755 "$RUN/dummy" && cp "$(command -v sleep)" "$RUN/dummy/tapectl" && chmod 0755 "$RUN/dummy/tapectl"
sampler_start() { # sampler_start <name>: record each tapectl process's priority until sampler_stop
    rm -f "$RUN/prio-$1.stop"
    # shellcheck disable=SC2024  # the redirects are ours on purpose: the service user cannot write $RUN
    sudo -u "$SVC_USER" -H python3 "$RUN/tapectl_prio.py" "$RUN/prio-$1.stop" >"$RUN/prio-$1.tsv" 2>"$RUN/prio-$1.err" &
    SAMPLER_PID=$!
}
sampler_stop() { touch "$RUN/prio-$1.stop"; chmod 0644 "$RUN/prio-$1.stop"; wait "$SAMPLER_PID" 2>/dev/null; }
assert_no_tape() { assert_prio "$1" --none; }
# assert_prio <name> <tape commands that must have been seen>...: the sampler's
# record against the profile. Every tape command (the list `tapectl-op` raises)
# is at TAPE_NICE/TAPE_IONICE, and each named one was seen. Other tapectl
# commands in step 13 inherit the raise from the shell and are not judged.
# `assert_prio <name> --none`: no tape command was started at all (the record
# is not empty: the phase's other tapectl commands are in it).
assert_prio() {
    local name="$1"; shift
    [ -s "$RUN/prio-$name.tsv" ] || { echo "the sampler recorded nothing (prio-$name.err: $(cat "$RUN/prio-$name.err" 2>/dev/null))"; return 1; }
    python3 - "$RUN/prio-$name.tsv" "$TAPE_NICE_RUN" "$WANT_IO" "$@" <<'PY'
import sys
tsv, want_nice, want_io, *required = sys.argv[1:]
none = required == ["--none"]
if none:
    required = []
TAPE = {
    "volume": {"write", "resume", "verify", "rewrite", "compact-write", "compact", "read-slices",
               "compact-read", "init", "identify"},
    "restore": {"unit", "file", "volume", "raw-volume"},
    "cartridge": {"erase"}, "collection": {"run"}, "catalog": {"rebuild"}, "drive": {"poll", "eject"},
}
def is_tape(cmd):
    first, _, second = cmd.partition(" ")
    return first == "quick-archive" or second in TAPE.get(first, ())
rows = [l.rstrip("\n").split("\t") for l in open(tsv) if l.strip()]
bad, tape, other = [], {}, 0
for pid, cmd, nice, io in rows:
    if is_tape(cmd):
        tape.setdefault(cmd, []).append((nice, io))
        if nice != want_nice or io != want_io:
            bad.append(f"NOT RAISED: pid {pid} '{cmd}' ran at nice {nice}, '{io}' (want {want_nice}, '{want_io}')")
    else:
        other += 1
for cmd in required:
    if cmd not in tape:
        bad.append(f"never saw '{cmd}' (saw tape commands: {sorted(tape) or 'none'})")
if none and tape:
    bad.append(f"a tape command ran where none may: {sorted(tape)}")
print("tape commands raised: " + (", ".join(f"{c} x{len(v)}" for c, v in sorted(tape.items())) or "none") + f"  (other tapectl commands seen, not judged: {other})")
print("\n".join(bad))
sys.exit(1 if bad else 0)
PY
}
# assert_raised_notice <phase output file>: first-run.sh's step 13 says its shell
# went from this harness's priority to the profile's (ps and ionice read it from
# the kernel before and after the one sudo renice), so the raise is shown to have
# started from base and to have taken.
assert_raised_notice() {
    grep -F "is now at nice $TAPE_NICE_RUN, ionice $TAPE_IONICE_RUN (was: $BASE_NICE $BASE_IO; now: $TAPE_NICE_RUN $WANT_IO)" "$1" >/dev/null \
        || { echo "$1 has no step-13 notice that the run went from '$BASE_NICE $BASE_IO' to '$TAPE_NICE_RUN $WANT_IO':"; grep -E 'priority|nice' "$1" | head; return 1; }
}
# assert_base <name> <the dummy's words>: nothing tapectl started in the phase
# was raised (the sampler's record, all at THIS shell's priority), and the dummy
# started at base was recorded, so the record could have shown a raised one.
assert_base() {
    local name="$1"
    python3 - "$RUN/prio-$name.tsv" "$BASE_NICE" "$BASE_IO" "$2" <<'PY'
import sys
tsv, base_nice, base_io, dummy = sys.argv[1:]
rows = [l.rstrip("\n").split("\t") for l in open(tsv) if l.strip()]
bad = [f"RAISED BEFORE STEP 13: pid {p} '{c}' ran at nice {n}, '{io}' (want {base_nice}, '{base_io}')"
       for p, c, n, io in rows if n != base_nice or io != base_io]
if not any(c == dummy for _, c, _, _ in rows):
    bad.append(f"the sampler never recorded the dummy '{dummy}' it was started to see (rows: {len(rows)})")
print(f"tapectl processes seen: {len(rows)}, all at nice {base_nice} / '{base_io}': {not bad}")
print("\n".join(bad))
sys.exit(1 if bad else 0)
PY
}

SHELF=fr414-rack
for u in a b c d; do
    mkdir -p "$RUN/src/keepsake/$u"
    echo "unit $u of the #414 re-entry scenario" >"$RUN/src/keepsake/$u/README"
    head -c "$((UNIT_MIB * 1024 * 1024))" /dev/urandom >"$RUN/src/keepsake/$u/payload.bin"
done

# Every first-run.sh invocation: the profile, --auto, the label as a FLAG (the
# WRITE confirmation counts under --auto only when the word came from one),
# --mint-escrow (step 7 mints the throwaway home's escrow identity, which --auto
# does only when the command line says so; ADR-0012 item 188), and a private
# XDG_STATE_HOME so its log, the profile record and the rehearsal marker are this run's.
export XDG_STATE_HOME="$RUN/state"
# #437: step 7 writes the home pointer here, never to this VM's /etc/tapectl/home
# (which would redirect every HOME-isolated test on the box to this run's home).
export TAPECTL_HOME_POINTER="$RUN/home-pointer"
FR_LOG="$RUN/state/tapectl/first-run.log"
first_run() { first_run_label "$LABEL" "$@"; }
# first_run_label <label> [args...]: the same, for another volume (phase E's copy 2).
first_run_label() { local l="$1"; shift; bash "$REPO/scripts/first-run.sh" --profile "$PROFILE" --auto --mint-escrow --label "$l" "$@"; }
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
        echo "fr414: GREEN — issue #414's acceptance holds, and so does #432's (tape commands raised, nothing raised before step 13)"
        if [ "$KEEP" = 0 ]; then sudo rm -rf "$RUN"; sudo groupdel "$OPS_GROUP" 2>/dev/null; echo "fr414: workspace removed (--keep keeps it)"; fi
        exit 0
    fi
    echo "fr414: RED — the workspace is kept for reading: $RUN (first-run log: $FR_LOG)"
    exit 1
}
# A phase whose check failed leaves the next one nothing to prove.
must() { [ "${RESULT[$1]}" = PASS ] || finish stop; }

# ---------- #432: the wrapper, with no tape ----------
echo "fr414: tapectl-op priority (scripts/check-tapectl-op.sh, no tape)"
step_wrapper_priority() {
    "$REPO/scripts/check-tapectl-op.sh" --user "$SVC_USER" --tape-nice "$TAPE_NICE_RUN" --tape-ionice "$TAPE_IONICE_RUN" --tapectl "$TAPECTL"
}
check tapectl_op_priority step_wrapper_priority

# ---------- phase A: the install and the rehearsal ----------
echo "fr414: phase A — first-run.sh --to 12 (install + rehearsal); output $RUN/phase-A.out"
step_phase_a() {
    local rc dummy
    sampler_start A
    sudo -u "$SVC_USER" -H "$RUN/dummy/tapectl" 3600 </dev/null >/dev/null 2>&1 &
    dummy=$!
    first_run --barcode "$LOADED_TAG" --to 12 </dev/null >"$RUN/phase-A.out" 2>&1; rc=$?
    kill "$dummy" 2>/dev/null; wait "$dummy" 2>/dev/null
    sampler_stop A
    [ "$rc" -eq 0 ] || { tail -40 "$RUN/phase-A.out"; return 1; }
    ls "$RUN/state/tapectl/"rehearsal-ok-* >/dev/null 2>&1 \
        || { echo "step 12 recorded no rehearsal marker (was it skipped? see phase-A.out)"; return 1; }
    sudo -u "$SVC_USER" -H grep -E '^jobs *= *2$' "$RUN/home/config.toml" >/dev/null \
        || { echo "step 7 did not write STAGING_JOBS=2 as [staging] jobs"; return 1; }
    sudo -u "$SVC_USER" -H grep -F 'contender_processes = ["cargo","rustc","docker","Runner.Worker","java"]' "$RUN/home/config.toml" >/dev/null \
        || { echo "step 7 did not write CONTENDER_PROCESSES as [host_check] contender_processes"; return 1; }
    sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" config set defaults.slice_size 1M
}
check install_and_rehearsal step_phase_a
must install_and_rehearsal
check nothing_raised_before_step_13 assert_base A 3600
# #469: step 13's erase branch, driven for real. The rehearsal leaves a SEALED
# volume of its own throwaway catalog on the cartridge; `volume init` refuses it
# (ADR-0003) and step 13 offers `cartridge erase`, which under --auto runs only
# when --erase-label names the label File 0 carries. A wrong word is refused
# with the tape untouched (here); phase B passes the right one, erases, inits
# with no --force (the verified-erase proof, #444) and writes. Until #469 this
# harness blanked the tape with `mt erase` first, so no harness ever ran the
# branch the production fresh start takes.
file0_label() { # the label File 0 carries, read with mt and dd alone
    mt -f "$DEVICE" rewind >/dev/null 2>&1 || return 1
    dd if="$DEVICE" bs=512k count=1 2>/dev/null | tr -d '\0' | sed -n 's/^label = "\(.*\)"$/\1/p' | head -1
}
REHEARSAL_LABEL=""
step_rehearsal_tape_sealed() {
    eval "$("$REPO/scripts/mhvtl-device.sh" --tape "$DEVICE" --ensure-media)" || return 1
    REHEARSAL_LABEL="$(file0_label)"
    [ -n "$REHEARSAL_LABEL" ] || { echo "File 0 names no label: the rehearsal left no volume for step 13 to erase"; return 1; }
    echo "the rehearsal's volume on the cartridge: $REHEARSAL_LABEL"
}
step_wrong_erase_label() {
    local rc
    first_run --from 13 --to 13 --erase-label "NOT-$REHEARSAL_LABEL" </dev/null >"$RUN/phase-A2.out" 2>&1; rc=$?
    [ "$rc" -ne 0 ] || { echo "a wrong --erase-label did not stop step 13"; return 1; }
    grep -F "does not match the tape in the drive" "$RUN/phase-A2.out" >/dev/null \
        || { echo "step 13 did not refuse the wrong word by name:"; tail -20 "$RUN/phase-A2.out"; return 1; }
    [ "$(file0_label)" = "$REHEARSAL_LABEL" ] \
        || { echo "File 0 changed although step 13 was refused before erasing"; return 1; }
    [ "$(sql "SELECT COUNT(*) FROM volumes WHERE label = ?" "$LABEL")" = 0 ] \
        || { echo "the refused run left a volume row for $LABEL"; return 1; }
}
# #393: after step 7, the operator reads `status` with the ops group alone
# (no sudo to the service user), and step 7 said it checked that itself.
step_ops_group_status() {
    local gid; gid="$(getent group "$OPS_GROUP" | cut -d: -f3)" || { echo "step 7 created no group $OPS_GROUP"; return 1; }
    # #437: step 7 wrote the pointer to this run's file, naming this run's home,
    # and the operator's plain `tapectl status` (no --home, no ~/.tapectl) finds it.
    [ "$(cat "$TAPECTL_HOME_POINTER" 2>/dev/null)" = "$RUN/home" ] \
        || { echo "step 7 wrote no pointer naming $RUN/home to $TAPECTL_HOME_POINTER"; return 1; }
    [ ! -e /etc/tapectl/home ] || { echo "/etc/tapectl/home exists on this VM: the pointer escaped the run"; return 1; }
    sudo setpriv --reuid="$(id -u)" --regid="$gid" --clear-groups \
        env -u TAPECTL_HOME HOME="$RUN/no-such-home" TAPECTL_HOME_POINTER="$TAPECTL_HOME_POINTER" "$TAPECTL" status \
        || { echo "plain status (via the pointer) as $USER with group $OPS_GROUP failed"; return 1; }
    # ok/note lines go to the terminal, not first-run.log: read phase A's output.
    grep -F "ops group $OPS_GROUP:" "$RUN/phase-A.out" | grep -F "checked as" >/dev/null \
        || { echo "step 7 did not report its own status check:"; grep -F "ops group" "$RUN/phase-A.out"; return 1; }
    # Positive control: without the group, the same read is refused.
    if sudo setpriv --reuid="$(id -u)" --regid="$(id -g nobody)" --clear-groups "$TAPECTL" --home "$RUN/home" status >/dev/null 2>&1; then
        echo "status worked WITHOUT the ops group: the check above proves nothing"; return 1
    fi
}
check ops_group_status step_ops_group_status
check rehearsal_tape_sealed step_rehearsal_tape_sealed
must rehearsal_tape_sealed
check wrong_erase_label_refused step_wrong_erase_label
must wrong_erase_label_refused

# ---------- phase B: the write, cut short ----------
echo "fr414: phase B — first-run.sh --from 13 --erase-label <the rehearsal's label>: erase, init with no --force, $SIG once the write is part-way; output $RUN/phase-B.out"
step_phase_b() {
    local fr_pid poll_rc fr_rc
    sampler_start B
    first_run --from 13 --to 13 --erase-label "$REHEARSAL_LABEL" </dev/null >"$RUN/phase-B.out" 2>&1 &
    fr_pid=$!
    # The poller runs as the service user so it can read the 0700 catalog and
    # signal the tapectl process (same uid) the moment the state is right.
    sudo -u "$SVC_USER" -H python3 - "$DB" "$LABEL" "$SIG" "$fr_pid" "$RUN" "$TAPE_NICE_RUN" "$WANT_IO" <<'PY'
import os, signal, sqlite3, sys, time
db, label, sig, fr_pid = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
run, want_nice, want_io = sys.argv[5], int(sys.argv[6]), sys.argv[7]
sys.path.insert(0, run)
import tapectl_prio as tp
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
            # #432: read the write's priority from the kernel BEFORE it is
            # signalled — /proc/PID/stat field 19 and ionice -p — and judge it
            # after, so a failure still cuts the write short.
            try:
                nice, io = tp.nice_of(pid), tp.io_of(pid)
            except (OSError, ValueError, IndexError):
                nice, io = None, ""
            os.kill(pid, getattr(signal, "SIG" + sig))
            print(f"signalled tapectl pid {pid} with SIG{sig}: {row[0]} slice(s) written, {row[1]} not yet")
            print(f"priority of the write (pid {pid}): nice {nice}, '{io}'; the profile says nice {want_nice}, '{want_io}'")
            if nice != want_nice or io != want_io:
                print("THE WRITE WAS NOT RAISED to the profile's TAPE_NICE/TAPE_IONICE (#432)")
                sys.exit(3)
            sys.exit(0)
    time.sleep(0.05)
print("TIMEOUT: the write never reached a part-way state within the hour")
sys.exit(1)
PY
    poll_rc=$?
    wait "$fr_pid"; fr_rc=$?
    sampler_stop B
    echo "first-run.sh exited $fr_rc"
    [ "$poll_rc" -eq 0 ] || return 1
    # first-run.sh must have died at the write, not carried on.
    [ "$fr_rc" -ne 0 ] || { echo "first-run.sh exited 0 although its write was signalled"; return 1; }
    grep "write did not seal" "$RUN/phase-B.out" >/dev/null \
        || { echo "phase-B.out does not show step 13 dying at the write"; tail -30 "$RUN/phase-B.out"; return 1; }
}
# #469 / #444: the write above began on a tape step 13 erased itself.
step_erased_then_inited_without_force() {
    local proof
    grep -F "the tape is erased, read back and recorded" "$RUN/phase-B.out" >/dev/null \
        || { echo "phase-B.out shows no cartridge erase"; return 1; }
    if grep -E 'volume init .*--force' "$RUN/phase-B.out" >/dev/null; then
        echo "step 13 forced volume init after the erase"; return 1
    fi
    proof="$(sql "SELECT json_extract(e.details, '\$.blank_proof.kind') FROM events e
                  JOIN volumes v ON v.id = e.entity_id
                  WHERE e.entity_type = 'volume' AND e.action = 'created' AND v.label = ?" "$LABEL")"
    [ "$proof" = verified_erase ] \
        || { echo "volume $LABEL was created with blank proof '${proof:-none}', want verified_erase"; return 1; }
}
check write_cut_short step_phase_b
must write_cut_short
check erased_then_inited_without_force step_erased_then_inited_without_force
check priority_phase_b assert_prio B "volume write"
check raise_notice_phase_b assert_raised_notice "$RUN/phase-B.out"
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
# created when the phase began. $4 says where the full readback comes from
# (ADR-0012 item 218, OQ15): `new`, recorded by this phase (the copy's own
# --full-confirm, $7: the `resume` or the `write`; there is no separate verify
# any more), or `recorded`, by an earlier phase, which this one must have read
# and not repeated. $5 and $6 are the volume and its place ($LABEL at $SHELF
# unless given).
assert_post_write_block() { # assert_post_write_block <verify-id-before> <phase-start-file> <phase-out> <new|recorded> [label] [place] [resume|write]
    local before="$1" since="$2" out="$3" readback="$4" label="${5:-$LABEL}" place="${6:-$SHELF}" what="${7:-resume}" bad sess loc
    bad="$(sql "SELECT w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
                WHERE v.label = ? AND w.status <> 'completed'" "$label" | tr '\n' ' ')"
    [ "$(sql "SELECT status FROM volumes WHERE label = ?" "$label")" = sealed ] \
        || { echo "volume $label is not sealed"; return 1; }
    [ -z "${bad// /}" ] || { echo "writes not all completed: $bad"; return 1; }
    # The newest verification session. In a `new` phase the confirm of the
    # copy is the phase's last tape contact, so it is that confirm: full,
    # passed, and newer than the phase's start. In a `recorded` phase nothing
    # read the tape, so it is the one the phase began with, full and passed.
    sess="$(sql "SELECT s.id || ' ' || s.verify_type || ' ' || s.outcome FROM verification_sessions s
                 JOIN volumes v ON v.id = s.volume_id WHERE v.label = ? ORDER BY s.id DESC LIMIT 1" "$label")"
    echo "newest verification session: $sess (newest before this phase: $before)"
    local sid stype soutcome
    read -r sid stype soutcome <<<"$sess"
    [ "${stype:-}" = full ] && [ "${soutcome:-}" = passed ] \
        || { echo "the newest verification session of $label is not a full, passed readback"; return 1; }
    case "$readback" in
        new)
            [ "${sid:-0}" -gt "$before" ] || { echo "no full, passed readback recorded by this phase"; return 1; }
            grep -F "$label sealed and read back in full: the $what's confirm read every file back" "$out" >/dev/null \
                || { echo "$out does not say the $what's confirm read $label back in full"; return 1; } ;;
        recorded)
            [ "${sid:-0}" -eq "$before" ] || { echo "this phase recorded a verification session ($sid, newest before: $before): it read the tape again"; return 1; }
            grep -E "^ +\\\$ .* report verify-status --volume $label --json" "$out" >/dev/null \
                || { echo "$out shows no 'report verify-status --volume $label --json': the recorded readback was not asked for"; return 1; }
            grep "$label sealed and read back in full: a full readback passed on " "$out" >/dev/null \
                || { echo "$out does not say a full readback of $label is recorded"; return 1; } ;;
        *) echo "assert_post_write_block: new or recorded, not '$readback'"; return 1 ;;
    esac
    if grep -E "^ +\\\$ .* volume verify " "$out" >/dev/null; then
        echo "$out runs a volume verify: the copy's full readback was read a second time (ADR-0012 item 218)"; return 1
    fi
    grep -E '^ +\$ .* audit$' "$out" >/dev/null || { echo "$out shows no audit run"; return 1; }
    loc="$(sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" volume info "$label" --json \
           | python3 -c 'import json,sys; print(json.load(sys.stdin).get("location") or "")')"
    [ "$loc" = "$place" ] || { echo "volume $label is at '$loc', expected $place"; return 1; }
    [ "$RUN/kit/COVER.txt" -nt "$since" ] || { echo "the Heir Kit was not regenerated ($RUN/kit/COVER.txt not newer than the phase)"; return 1; }
    echo "post-write block complete: $label sealed, read back in full once ($readback), audited, at $loc, kit regenerated"
}

echo "fr414: phase C — the re-entry, first-run.sh --from 13; output $RUN/phase-C.out"
step_phase_c() {
    local before rc
    before="$(max_verify_id)"; touch "$RUN/phase-C.start"; sleep 1
    sampler_start C
    first_run --from 13 </dev/null >"$RUN/phase-C.out" 2>&1; rc=$?
    sampler_stop C
    [ "$rc" -eq 0 ] || { echo "the re-entry exited non-zero"; tail -40 "$RUN/phase-C.out"; return 1; }
    grep "step 13: volume $LABEL is resume" "$FR_LOG" >/dev/null \
        || { echo "step 13 did not classify $LABEL as an interrupted write (first-run log)"; return 1; }
    grep -E "^ +\\\$ .* volume resume $LABEL .*--full-confirm" "$RUN/phase-C.out" >/dev/null \
        || { echo "phase-C.out shows no 'volume resume $LABEL ... --full-confirm'"; return 1; }
    grep -F "volume \"$LABEL\" write resumed and completed" "$RUN/phase-C.out" >/dev/null \
        || { echo "phase-C.out does not show the resume completing"; return 1; }
    if grep -E "^ +\\\$ .* (volume write|stage create|snapshot create) " "$RUN/phase-C.out" >/dev/null; then
        echo "the re-entry staged or wrote again instead of resuming"; return 1
    fi
    assert_post_write_block "$before" "$RUN/phase-C.start" "$RUN/phase-C.out" new
}
check reentry_resumes_and_finishes step_phase_c
must reentry_resumes_and_finishes
check priority_phase_c assert_prio C "volume resume"
check raise_notice_phase_c assert_raised_notice "$RUN/phase-C.out"

# ---------- phase D: the sealed path ----------
echo "fr414: phase D — first-run.sh --from 13 on the sealed volume; output $RUN/phase-D.out"
step_phase_d() {
    local before nwrites rc
    before="$(max_verify_id)"; nwrites="$(writes_count)"; touch "$RUN/phase-D.start"; sleep 1
    sampler_start D
    first_run --from 13 --to 13 </dev/null >"$RUN/phase-D.out" 2>&1; rc=$?
    sampler_stop D
    [ "$rc" -eq 0 ] || { echo "the sealed re-entry exited non-zero"; tail -40 "$RUN/phase-D.out"; return 1; }
    grep "step 13: volume $LABEL is sealed" "$FR_LOG" >/dev/null \
        || { echo "step 13 did not classify $LABEL as sealed (first-run log)"; return 1; }
    if grep -E "^ +\\\$ .* (volume write|volume resume|volume init|volume verify|stage create) " "$RUN/phase-D.out" >/dev/null; then
        echo "the sealed re-entry touched the write path, or read the tape"; return 1
    fi
    [ "$(writes_count)" = "$nwrites" ] || { echo "the sealed re-entry added write rows"; return 1; }
    assert_post_write_block "$before" "$RUN/phase-D.start" "$RUN/phase-D.out" recorded
}
check sealed_reentry_runs_only_the_post_write_block step_phase_d
# OQ15: with the full readback recorded, phase D starts no tape command at all
# (phase C's record, which saw `volume resume`, is the positive control).
check no_tape_command_phase_d assert_no_tape D
check raise_notice_phase_d assert_raised_notice "$RUN/phase-D.out"

# ---------- phase E: copy 2, as the operator runs it ----------
# docs/handoff.md (i) step 8: after copy 1, the operator loads the second
# cartridge and runs step 13 again with the next label and the second place.
# Every first_run_label below names $LABEL2 by flag (the WRITE confirmation
# under --auto counts only then) and --location $PLACE2, as the operator does.
PLACE2=fr414-offsite
# The units, and the catalog rows phase E must not add to where it writes nothing.
units_count() { sql "SELECT COUNT(*) FROM units"; }
catalog_rows() { sql "SELECT 'volumes=' || (SELECT COUNT(*) FROM volumes) || ' writes=' || (SELECT COUNT(*) FROM writes)
                      || ' cartridges=' || (SELECT COUNT(*) FROM cartridges) || ' snapshots=' || (SELECT COUNT(*) FROM snapshots)
                      || ' stage_sets=' || (SELECT COUNT(*) FROM stage_sets)"; }
stage_sets_by_status() { sql "SELECT status || '=' || COUNT(*) FROM stage_sets GROUP BY status ORDER BY status" | tr '\n' ' '; }
# File 0's whole first block, hashed: read with mt and dd alone, as file0_label.
file0_hash() {
    mt -f "$DEVICE" rewind >/dev/null 2>&1 || return 1
    dd if="$DEVICE" bs=512k count=1 2>/dev/null | sha256sum | cut -d' ' -f1
}
# How many lines of a phase's output show first-run.sh starting `tapectl <words>`.
ran_count() { grep -cE "^ +\\\$ .* $2( |\$)" "$1"; }
# The cartridge the catalog binds a volume to, by its barcode.
barcode_of() { sql "SELECT c.barcode FROM cartridges c JOIN cartridge_volumes cv ON cv.cartridge_id = c.id
                    JOIN volumes v ON v.id = cv.volume_id WHERE v.label = ?" "$1"; }

# The precondition of copy 2, from the catalog D left: the copy floor is 2
# ([defaults] min_copies and min_locations in force, read with config get:
# step 7's init writes both, and nothing in the private profile sets them;
# and each staged unit's resolved floor, from stage list), and every unit is
# still staged with its one copy, so the release E1 expects has something to
# release.
step_copy2_preconditions() {
    local key val nunits staged
    for key in defaults.min_copies defaults.min_locations; do
        val="$(sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" config get "$key" --json \
               | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("value"), d.get("source"))')" \
            || { echo "config get $key failed"; return 1; }
        echo "$key: $val"
        [ "${val%% *}" = 2 ] || { echo "$key is not 2: copy 2 would not be the copy that meets the floor (report, do not change it)"; return 1; }
    done
    # The floor each staged unit is held to, resolved (min_copies and its
    # copies_at), and its one copy so far.
    sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" stage list --status staged --json \
        | python3 -c 'import json, sys
rows = json.load(sys.stdin)
for r in rows:
    print("staged:", r.get("unit"), "copy_floor=%s copies=%s" % (r.get("copy_floor"), r.get("copies")))
sys.exit(0 if rows and all(r.get("copy_floor") == 2 and r.get("copies") == 1 for r in rows) else 1)' \
        || { echo "want every staged unit at copy floor 2 with 1 copy (above)"; return 1; }
    nunits="$(units_count)"; staged="$(sql "SELECT COUNT(*) FROM stage_sets WHERE status = 'staged'")"
    echo "units: $nunits; stage sets: $(stage_sets_by_status); catalog: $(catalog_rows)"
    { [ "$nunits" -ge 1 ] && [ "$staged" = "$nunits" ]; } \
        || { echo "want every unit staged once before copy 2 ($nunits units, $staged staged sets)"; return 1; }
    [ "$(sql "SELECT COUNT(*) FROM volumes")" = 1 ] || { echo "want exactly one volume ($LABEL) in the catalog"; return 1; }
}
check copy2_preconditions step_copy2_preconditions
must copy2_preconditions

# E0: this catalog's own sealed copy is still in the drive. The run names it
# with --erase-label $LABEL as well: consent to erase that very tape, which
# must still not reach it (ADR-0012 item 216: init offers no erase for it).
echo "fr414: phase E0 — first-run.sh --from 13 --label $LABEL2 with $LABEL still loaded; output $RUN/phase-E0.out"
step_phase_e0() {
    local rc f0_before rows_before w1_before out="$RUN/phase-E0.out" n
    f0_before="$(file0_hash)"; rows_before="$(catalog_rows)"; w1_before="$(writes_count)"
    [ -n "$f0_before" ] || { echo "could not read File 0 before the run"; return 1; }
    sampler_start E0
    first_run_label "$LABEL2" --from 13 --location "$PLACE2" --erase-label "$LABEL" </dev/null >"$out" 2>&1; rc=$?
    sampler_stop E0
    echo "first-run.sh exited $rc"
    [ "$rc" -eq 1 ] || { echo "want exit 1 (step 13's die)"; tail -30 "$out"; return 1; }
    grep -F "1 volume(s) already in the catalog." "$out" >/dev/null \
        || { echo "no '1 volume(s) already in the catalog.' note"; return 1; }
    grep -F "Write another tape now, or finish one an interrupted run left (same label)? [auto: yes]" "$out" >/dev/null \
        || { echo "--auto did not take the 'write another tape' confirm"; return 1; }
    # The init runs (it is what reads the tape) and refuses. The positive
    # control for every absence below: commands are shown in this output.
    n="$(ran_count "$out" "volume init $LABEL2")"
    [ "$n" = 1 ] || { echo "want exactly one 'volume init $LABEL2' started, saw $n"; return 1; }
    grep -F "It is this catalog's live copy: $LABEL, a copy of" "$out" >/dev/null \
        || { echo "init's refusal does not name $LABEL as this catalog's live copy:"; grep -iE 'refus|sealed' "$out" | head; return 1; }
    grep -F "Leave it as it is: eject it" "$out" >/dev/null || { echo "the refusal does not say to eject it"; return 1; }
    grep -F "is this catalog's own volume (above): eject it" "$out" >/dev/null \
        || { echo "step 13 did not die naming the eject"; tail -15 "$out"; return 1; }
    for n in "cartridge erase" "volume write" "volume resume" "volume verify" "stage create"; do
        [ "$(ran_count "$out" "$n")" = 0 ] || { echo "step 13 ran '$n' although init refused"; return 1; }
    done
    if grep -E "volume init .*--force" "$out" >/dev/null; then echo "step 13 forced an init"; return 1; fi
    if grep -F "ERASE the tape" "$out" >/dev/null; then echo "step 13 offered an erase"; return 1; fi
    [ "$(file0_hash)" = "$f0_before" ] || { echo "File 0 of the loaded cartridge changed"; return 1; }
    [ "$(file0_label)" = "$LABEL" ] || { echo "File 0 no longer names $LABEL"; return 1; }
    [ "$(sql "SELECT status FROM volumes WHERE label = ?" "$LABEL")" = sealed ] || { echo "$LABEL is no longer sealed"; return 1; }
    [ "$(writes_count)" = "$w1_before" ] || { echo "write rows of $LABEL changed"; return 1; }
    [ "$(catalog_rows)" = "$rows_before" ] || { echo "catalog rows changed: $rows_before -> $(catalog_rows)"; return 1; }
    [ "$(sql "SELECT COUNT(*) FROM volumes WHERE label = ?" "$LABEL2")" = 0 ] \
        || { echo "the refused init left a volume row for $LABEL2 (E1 would take the 'fresh' branch)"; return 1; }
    echo "refused with nothing erased or written; File 0 $f0_before unchanged; catalog $rows_before unchanged"
}
check own_sealed_copy_refused step_phase_e0
must own_sealed_copy_refused
check priority_phase_e0 assert_prio E0 "volume init"
check raise_notice_phase_e0 assert_raised_notice "$RUN/phase-E0.out"

# E1, the setup: the first copy back to its slot, the second cartridge in.
FIRST_TAG=""; FIRST_SLOT=""; SECOND_TAG=""; SECOND_SLOT=""
step_load_second_cartridge() {
    local status
    status="$(mtx -f "$CHG_SG" status)" || return 1
    FIRST_SLOT="$(echo "$status" | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    FIRST_TAG="$(echo "$status" | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"
    { [ -n "$FIRST_SLOT" ] && [ -n "$FIRST_TAG" ]; } || { echo "the changer does not name the loaded cartridge's slot"; return 1; }
    mtx -f "$CHG_SG" unload "$FIRST_SLOT" "$DTE" || return 1
    # The HIGHEST-numbered full slot of the drive's generation: the chain's
    # upgrade harness, on another drive, takes from the low end.
    SECOND_SLOT="$(mtx -f "$CHG_SG" status | grep -E "Storage Element [0-9]+:Full" | grep "VolumeTag=[EF][0-9]*$GEN" \
        | grep -v "VolumeTag=$FIRST_TAG" | tail -1 | sed -n 's/.*Storage Element \([0-9]*\):Full.*/\1/p')"
    [ -n "$SECOND_SLOT" ] || { echo "no second $GEN cartridge in the library"; return 1; }
    mtx -f "$CHG_SG" load "$SECOND_SLOT" "$DTE" || return 1
    SECOND_TAG="$(mtx -f "$CHG_SG" status | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"
    { [ -n "$SECOND_TAG" ] && [ "$SECOND_TAG" != "$FIRST_TAG" ]; } || { echo "the drive holds '$SECOND_TAG'"; return 1; }
    mt -f "$DEVICE" rewind || return 1
    echo "$LABEL's cartridge $FIRST_TAG back to slot $FIRST_SLOT; $SECOND_TAG loaded from slot $SECOND_SLOT"
}
check load_second_cartridge step_load_second_cartridge
must load_second_cartridge
# A sealed volume FOREIGN to the run's catalog, as home2's second cartridge
# holds (L6-0002, written by an old catalog): a throwaway home of the run's own
# binary, as this user, writes one tiny unit to it as $LABEL2 and seals it,
# after an `mt erase` so its init meets a blank tape. It takes the tape lock
# for itself only, in a subshell that ends before step 13 takes it.
step_foreign_sealed_volume() {
    local f="$RUN/foreign"
    mkdir -p "$f/home/staging" "$f/src/foreign-u1" || return 1
    echo "a unit of a catalog the fr414 run does not know" >"$f/src/foreign-u1/README"
    (
        flock -n 9 || { echo "the tape lock is held: another harness is using a drive"; exit 1; }
        ft() { echo "\$ tapectl $*"; "$TAPECTL" --home "$f/home" "$@"; }
        mt -f "$DEVICE" rewind && mt -f "$DEVICE" erase \
            && ft init --operator fr414-foreign \
            && ft drive add --name mhvtl --device-tape "$DEVICE" --generation "$DGEN" \
            && ft config set staging.directory "$f/home/staging" \
            && ft unit init "$f/src/foreign-u1" --tenant fr414-foreign --name foreign-u1 \
            && ft snapshot create foreign-u1 && ft stage create foreign-u1 \
            && ft volume write "$LABEL2" --init --device "$DEVICE" --yes
    ) 9>/tmp/tapectl-tape.lock >"$f/write.out" 2>&1 || { echo "the foreign home could not write $LABEL2:"; tail -30 "$f/write.out"; return 1; }
    [ "$("$TAPECTL" --home "$f/home" volume info "$LABEL2" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')" = sealed ] \
        || { echo "the foreign home's $LABEL2 is not sealed"; return 1; }
    [ "$(file0_label)" = "$LABEL2" ] || { echo "File 0 of $SECOND_TAG does not name $LABEL2"; return 1; }
    [ "$(sql "SELECT COUNT(*) FROM volumes WHERE label = ?" "$LABEL2")" = 0 ] || { echo "the run's catalog knows $LABEL2"; return 1; }
    echo "$SECOND_TAG holds the foreign home's sealed $LABEL2; the run's catalog has no such volume"
}
check foreign_sealed_volume_on_second_cartridge step_foreign_sealed_volume
must foreign_sealed_volume_on_second_cartridge

# E1, copy 2.
echo "fr414: phase E1 — copy 2, first-run.sh --from 13 --label $LABEL2 --location $PLACE2 --erase-label $LABEL2; output $RUN/phase-E1.out"
E1_SLICES_BEFORE=""
staged_slice_files() { sudo find "$RUN/staging" -type f -name '*.age' | wc -l; }
step_phase_e1() {
    local rc out="$RUN/phase-E1.out" state
    E1_SLICES_BEFORE="$(staged_slice_files)"
    touch "$RUN/phase-E1.start"; sleep 1
    sampler_start E1
    first_run_label "$LABEL2" --from 13 --location "$PLACE2" --erase-label "$LABEL2" </dev/null >"$out" 2>&1; rc=$?
    sampler_stop E1
    echo "first-run.sh exited $rc"
    [ "$rc" -eq 0 ] || { echo "copy 2 exited non-zero"; tail -40 "$out"; return 1; }
    grep -F "1 volume(s) already in the catalog." "$out" >/dev/null \
        || { echo "no '1 volume(s) already in the catalog.' note"; return 1; }
    grep -F "Write another tape now, or finish one an interrupted run left (same label)? [auto: yes]" "$out" >/dev/null \
        || { echo "--auto did not take the 'write another tape' confirm"; return 1; }
    state="$(grep "step 13: volume $LABEL2 is " "$FR_LOG" | tail -1)"
    case "$state" in *" is absent") echo "$state" ;; *) echo "step 13 classified $LABEL2 as: ${state:-nothing}"; return 1 ;; esac
}
check copy2_runs step_phase_e1
must copy2_runs
# Nothing snapshotted or staged again: snapshot create finds every unit
# unchanged, and step 13 skips each one already staged (#283).
step_e1_nothing_restaged() {
    local out="$RUN/phase-E1.out" n
    n="$(units_count)"
    [ "$(ran_count "$out" "stage create")" = 0 ] || { echo "step 13 ran stage create"; return 1; }
    [ "$(grep -cF -- "is already staged -- skipping it" "$out")" = "$n" ] \
        || { echo "want $n 'already staged -- skipping it' notes:"; grep -F "already staged" "$out"; return 1; }
    [ "$(ran_count "$out" "snapshot create")" = "$n" ] || { echo "want $n snapshot create runs"; return 1; }
    [ "$(grep -cE 'is unchanged since v[0-9]+; no snapshot created' "$out")" = "$n" ] \
        || { echo "want every snapshot create to find its unit unchanged:"; grep -iE 'snapshot' "$out" | head; return 1; }
    [ "$(sql "SELECT COUNT(*) FROM stage_sets")" = "$n" ] || { echo "a stage set was added: $(stage_sets_by_status)"; return 1; }
    echo "$n units: no snapshot minted, no stage set added ($(stage_sets_by_status))"
}
check copy2_nothing_restaged step_e1_nothing_restaged
# The foreign sealed volume: init refuses it, step 13 shows the dry run and
# erases it on the --erase-label consent, then init again with no --force on
# the verified erase (#469, #444).
step_e1_foreign_tape_erased() {
    local out="$RUN/phase-E1.out" proof
    grep -F "already carries a SEALED volume" "$out" >/dev/null || { echo "init did not refuse the sealed foreign volume"; return 1; }
    if grep -F "Leave it as it is: eject it" "$out" >/dev/null; then echo "the foreign volume was taken for this catalog's own"; return 1; fi
    grep -E "^ +\\\$ .* cartridge erase --device .* --dry-run$" "$out" >/dev/null || { echo "no cartridge erase --dry-run"; return 1; }
    grep -F "ERASE the tape holding $LABEL2 [auto: named by --erase-label]" "$out" >/dev/null \
        || { echo "the erase was not offered naming File 0's label $LABEL2:"; grep -F "ERASE" "$out"; return 1; }
    grep -E "^ +\\\$ .* cartridge erase --device .* --confirm-label $LABEL2$" "$out" >/dev/null \
        || { echo "no 'cartridge erase --confirm-label $LABEL2'"; return 1; }
    grep -F "the tape is erased, read back and recorded" "$out" >/dev/null || { echo "the erase did not complete"; return 1; }
    [ "$(ran_count "$out" "volume init $LABEL2")" = 2 ] || { echo "want init run twice (refused, then after the erase)"; return 1; }
    if grep -E "volume init .*--force" "$out" >/dev/null; then echo "step 13 forced volume init"; return 1; fi
    proof="$(sql "SELECT json_extract(e.details, '\$.blank_proof.kind') FROM events e
                  JOIN volumes v ON v.id = e.entity_id
                  WHERE e.entity_type = 'volume' AND e.action = 'created' AND v.label = ?" "$LABEL2")"
    [ "$proof" = verified_erase ] || { echo "volume $LABEL2 was created with blank proof '${proof:-none}', want verified_erase"; return 1; }
    echo "refused, erased on --erase-label $LABEL2, inited with no --force (blank proof $proof)"
}
check copy2_foreign_tape_erased step_e1_foreign_tape_erased
# What step 13 runs for the copy after the init: the write with its full
# readback and the move, as one command (quoted into the check's record).
step_e1_write_command() {
    local line
    line="$(grep -E "^ +\\\$ .* volume write $LABEL2 " "$RUN/phase-E1.out")"
    echo "$line"
    [ "$(printf '%s\n' "$line" | grep -c .)" = 1 ] || { echo "want one volume write"; return 1; }
    printf '%s\n' "$line" | grep -E -- "volume write $LABEL2 --device [^ ]+ --yes --full-confirm --move-to $PLACE2\$" >/dev/null \
        || { echo "the write is not 'volume write $LABEL2 --device D --yes --full-confirm --move-to $PLACE2'"; return 1; }
}
check copy2_write_command step_e1_write_command
check copy2_post_write_block assert_post_write_block 0 "$RUN/phase-E1.start" "$RUN/phase-E1.out" new "$LABEL2" "$PLACE2" write
# Item 219: a cartridge init registers takes the volume's label as its barcode.
step_e1_barcode() {
    local bc; bc="$(barcode_of "$LABEL2")"
    echo "$LABEL2 is bound to the cartridge with barcode '${bc:-none}' (the library's tag: $SECOND_TAG)"
    [ "$bc" = "$LABEL2" ]
}
check copy2_cartridge_barcode step_e1_barcode
# The copy floor met, the write released every unit's staging (item 217).
step_e1_staging_released() {
    local n left files
    n="$(units_count)"
    left="$(sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" stage list --status staged --json \
            | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')" || { echo "stage list failed"; return 1; }
    files="$(staged_slice_files)"
    echo "stage sets: $(stage_sets_by_status); staged per stage list: $left; slice files in staging: $E1_SLICES_BEFORE before, $files after"
    [ "$E1_SLICES_BEFORE" -gt 0 ] || { echo "no slice files in staging before copy 2: the release below proves nothing"; return 1; }
    [ "$left" = 0 ] || { echo "units still staged"; return 1; }
    [ "$(sql "SELECT COUNT(*) FROM stage_sets WHERE status = 'cleaned'")" = "$n" ] || { echo "want $n stage sets cleaned"; return 1; }
    [ "$files" = 0 ] || { echo "slice files left in $RUN/staging"; return 1; }
}
check copy2_staging_released step_e1_staging_released
# The end state: both volumes sealed, every unit 2 copies in 2 places
# (`report copies`).
step_e1_two_copies() {
    local v
    for v in "$LABEL" "$LABEL2"; do
        [ "$(sql "SELECT status FROM volumes WHERE label = ?" "$v")" = sealed ] || { echo "$v is not sealed"; return 1; }
    done
    # shellcheck disable=SC2024  # the redirect is ours on purpose: the service user cannot write $RUN
    sudo -u "$SVC_USER" -H "$TAPECTL" --home "$RUN/home" report copies --json >"$RUN/report-copies.json" \
        || { echo "report copies failed"; return 1; }
    python3 - "$RUN/report-copies.json" "$(units_count)" <<'PY'
import json, sys
rows, n = json.load(open(sys.argv[1])), int(sys.argv[2])
for r in rows:
    print(f"{r['unit']}: {r['copies']} copies, {r['locations']} locations, volumes {r.get('volumes')}")
bad = [r["unit"] for r in rows if r["copies"] != 2 or r["locations"] != 2]
if len(rows) != n or bad:
    print(f"want {n} units each with 2 copies in 2 places; not so: {bad or 'the unit count'}")
    sys.exit(1)
PY
}
check copy2_both_sealed_two_copies_two_places step_e1_two_copies
# Step 14 asks for another copy only while one is owed (`stage list --owing`):
# after copy 2 none is. Phase C's step 14, after copy 1 with every unit one copy
# short, is the positive control: it does ask.
step_e1_no_copy_owed() {
    grep -F "The next copy:" "$RUN/phase-C.out" >/dev/null \
        || { echo "phase C's step 14 did not ask for the next copy (the positive control)"; return 1; }
    if grep -F "The next copy:" "$RUN/phase-E1.out" >/dev/null; then
        echo "step 14 asked for another copy after copy 2, with nothing owed"; return 1
    fi
    grep -F "no tape is owed now" "$RUN/phase-E1.out" >/dev/null \
        || { echo "step 14 did not say no tape is owed"; return 1; }
    echo "step 14 after copy 1 asks for the next copy; after copy 2, says none is owed"
}
check copy2_step14_asks_for_no_third_copy step_e1_no_copy_owed
check priority_phase_e1 assert_prio E1 "cartridge erase" "volume init" "volume write"
check raise_notice_phase_e1 assert_raised_notice "$RUN/phase-E1.out"

# The library left tidy: the drive's cartridge back to its slot.
step_library_tidy() {
    local slot
    slot="$(mtx -f "$CHG_SG" status | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    [ -n "$slot" ] || { echo "the drive is already empty"; return 0; }
    mtx -f "$CHG_SG" unload "$slot" "$DTE" || return 1
    mtx -f "$CHG_SG" status | grep -E "Data Transfer Element $DTE:Empty" >/dev/null || { echo "DTE $DTE is not empty"; return 1; }
    echo "the drive's cartridge is back in slot $slot"
}
check library_tidy step_library_tidy

finish
