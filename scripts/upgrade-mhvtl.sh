#!/usr/bin/env bash
# upgrade-mhvtl.sh — issue #505's acceptance (ADR-0012 item 192), on mhvtl:
# `scripts/first-run.sh --upgrade` takes a host installed by the previous
# release to the candidate, and every step does what --help says it does.
#
# What it does, in order (every first-run.sh run is the real script, --auto,
# driven by a private profile against an mhvtl drive; nothing is stubbed but the
# host's systemd units, which a stand-in replaces):
#
#   setup  preconditions (fail closed on a drive that is not mhvtl); two
#          throwaway CLONES of this repository (never worktrees: a tag made in a
#          worktree lands in the real repository): the previous release tag, and
#          HEAD as the candidate. When the candidate's version is not newer than
#          the previous tag's, its clone stamps it X.Y.(Z+1) with a stub release
#          note, and it is tagged there, annotated, never pushed. A DEBUG build of
#          each under the shared build lock (--skip-build --tapectl installs it;
#          no release build). A stand-in tapectl-drive-poll.timer in
#          /run/systemd/system (its service runs /bin/true), and a stand-in
#          installed tapectl-backup.service under TAPECTL_UNIT_DIR that names a
#          "previous" backup directory. An escrow key pair from age-keygen, which
#          the previous script's step 7 adopts.
#   P      the previous tag's own first-run.sh, --from 5 --to 11, installs it into
#          a throwaway home, with its binary "installed" at $RUN/install/tapectl
#          (the TAPECTL_INSTALL_BIN seam). The old binary then writes one sealed
#          volume, UPG-1, on cartridge P, which binds P's chip serial.
#   U1     the candidate's first-run.sh --upgrade, --barcode <P's serial>: steps
#          1-8 run (the catalog backed up with the OLD binary and checked, the old
#          binary kept, the new one installed, the migrations, step 14's plan, the
#          notes, the look), and step 9 refuses P by name: no rehearsal ran, P
#          still holds UPG-1, and the stand-in timer step 12 stopped is active again.
#   U2     cartridge T loaded and --upgrade run again, --barcode <T> --to 11: the
#          upgrade resumes (steps 2-4 checked as done, not redone), rehearses on T
#          (the timer seen stopped during it and active after), regenerates the
#          Heir Kit and checks its copy of tapectl against the installed one.
#   U3     --upgrade --from 12 with leftovers planted in a stand-in HOME, the work
#          directory, the previous backup directory and the install directory:
#          each is listed and still there (--auto never removes), what the step
#          keeps is not listed. Then the rollback step 13 printed, run as printed:
#          the old binary is installed again and reads the restored catalog.
#   R      refusals: an upgrade to the version already installed with no record
#          of one, a checkout whose HEAD is not a release tag, an upgrade step out
#          of range.
#
# Not covered, on purpose: tidy's removal (it needs a typed word, never given
# under --auto), step 11's start of a real tapectl-backup.service, and a real
# release build (the build rule: debug only). With no migration between the two
# versions the schema check proves N -> N, and the run says so.
#
# It never takes /tmp/tapectl-tape.lock itself (step 12's lifecycle suite takes
# it), only checks it is free before it starts.
#
# Usage:
#   TAPECTL_MHVTL=1 scripts/upgrade-mhvtl.sh --device /dev/tape/by-id/scsi-XYZZY_A1-nst
#   options: --from-tag vX.Y.Z  (default: the newest release tag before HEAD)
#            --unit-mib N        (MiB of random data per unit, 2 units; default 8)
#            --workspace DIR     (default /scratch/tapectl-upg)
#            --target-dir DIR    (CARGO_TARGET_DIR for both builds; default
#                                 /scratch/tapectl-target-upgrade)
#            --keep              (keep the run directory even when green)
#
# Needs: mhvtl loaded with at least two cartridges of the drive's generation,
# sudo without a password, the `tapectl` service user, dar, age, age-keygen,
# lsscsi, mtx, mt, python3, cargo, git, systemd.
# Exit: 0 green, 1 a check failed (the run directory is kept), 2 a
# precondition failed (nothing was written).
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DEVICE="${TAPECTL_GATE_TAPE:-}"
FROM_TAG=""
UNIT_MIB=8
WORKSPACE=/scratch/tapectl-upg
TARGET=/scratch/tapectl-target-upgrade
KEEP=0
SVC_USER=tapectl
LABEL=UPG-1
STANDIN_MARK="# upgrade-mhvtl.sh stand-in"

die() { echo "UPGRADE PRECONDITION FAILED: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
    case "$1" in
        --device) DEVICE="${2:-}"; shift 2 ;;
        --from-tag) FROM_TAG="${2:-}"; shift 2 ;;
        --unit-mib) UNIT_MIB="${2:-}"; shift 2 ;;
        --workspace) WORKSPACE="${2:-}"; shift 2 ;;
        --target-dir) TARGET="${2:-}"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,/^set -uo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (--help lists them)" ;;
    esac
done

# ---------- preconditions: fail closed before anything opens a device ----------
[ "${TAPECTL_MHVTL:-}" = 1 ] || die "TAPECTL_MHVTL=1 not set — this scenario writes tapes and runs only against mhvtl"
[ -n "$DEVICE" ] || die "--device not given — name the mhvtl drive (e.g. /dev/tape/by-id/scsi-XYZZY_A1-nst); there is no default"
case "$UNIT_MIB" in ''|*[!0-9]*) die "--unit-mib must be a whole number" ;; esac
[ -e "$DEVICE" ] || die "$DEVICE does not exist"
DEV_REAL="$(readlink -f "$DEVICE")"
IS_MHVTL=0
for f in /dev/tape/by-id/*XYZZY*-nst; do
    [ -e "$f" ] && [ "$(readlink -f "$f")" = "$DEV_REAL" ] && IS_MHVTL=1
done
[ "$IS_MHVTL" = 1 ] || die "$DEVICE ($DEV_REAL) is not an mhvtl drive (no /dev/tape/by-id/*XYZZY*-nst names it) — refusing"
grep -q '^mhvtl ' /proc/modules || die "mhvtl module not loaded — systemctl start mhvtl.target"
for bin in lsscsi mtx mt dar age age-keygen sha256sum python3 cargo git flock sudo systemctl; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
id "$SVC_USER" >/dev/null 2>&1 || die "no '$SVC_USER' service user — run scripts/first-run.sh --from 5 --to 5 once, or create it"
sudo -n true 2>/dev/null || die "sudo needs a password here; the upgrade's installs and the catalog reads need non-interactive sudo"
[ -z "$(git -C "$REPO" status --porcelain)" ] || die "$REPO has uncommitted changes: the candidate is HEAD as committed (the clone takes HEAD), so commit first"
for u in tapectl-drive-poll.timer tapectl-drive-poll.service tapectl-backup.service; do
    [ ! -e "/etc/systemd/system/$u" ] || die "/etc/systemd/system/$u exists: this host has a tapectl install, and the stand-in timer would shadow it"
done
( flock -n 9 || exit 1 ) 9>/tmp/tapectl-tape.lock || die "another process holds /tmp/tapectl-tape.lock — a gate, a rehearsal or a fill is using a drive; wait for it"

CAND_SHA="$(git -C "$REPO" rev-parse HEAD)"
if [ -z "$FROM_TAG" ]; then
    if git -C "$REPO" describe --exact-match HEAD >/dev/null 2>&1; then
        FROM_TAG="$(git -C "$REPO" describe --abbrev=0 --match 'v[0-9]*' HEAD^ 2>/dev/null)"
    else
        FROM_TAG="$(git -C "$REPO" describe --abbrev=0 --match 'v[0-9]*' HEAD 2>/dev/null)"
    fi
fi
[ "$(git -C "$REPO" cat-file -t "$FROM_TAG" 2>/dev/null)" = tag ] || die "the previous release '$FROM_TAG' is not an annotated tag here (--from-tag)"
PREV_VER="${FROM_TAG#v}"

DISCOVERY="$("$REPO/scripts/mhvtl-device.sh" --tape "$DEVICE" --ensure-media)" || die "device discovery failed (above)"
eval "$DISCOVERY"
DGEN="LTO-${GEN#L}"
P_TAG="$LOADED_TAG"
[ -n "$P_TAG" ] || die "no cartridge loaded after discovery"
[ "$(mtx -f "$CHG_SG" status | grep -E 'Storage Element [0-9]+:Full' | grep -c "VolumeTag=[EF][0-9]*$GEN")" -ge 1 ] \
    || die "the library has no second $GEN cartridge in a slot: the rehearsal needs one the catalog has never seen"
echo "upgrade: drive=$DEVICE ($DRIVE_MODEL, $DGEN) cartridge P=$P_TAG; from $FROM_TAG to HEAD ${CAND_SHA:0:12}"

# ---------- workspace, clones, builds ----------
RUN="$WORKSPACE/run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUN"/{install,builds/prev,builds/cand,state,src/keepsake,units,work} || die "cannot create $RUN"
chmod 0755 "$WORKSPACE" "$RUN" "$RUN/install" "$RUN/src" "$RUN/src/keepsake"
echo "upgrade: workspace $RUN"
PREV="$RUN/prev"; CAND="$RUN/cand"
git clone --quiet --no-checkout "$REPO" "$PREV" && git -C "$PREV" checkout --quiet "$FROM_TAG" \
    || die "cannot clone $FROM_TAG into $PREV"
git clone --quiet --no-checkout "$REPO" "$CAND" && git -C "$CAND" checkout --quiet "$CAND_SHA" \
    || die "cannot clone HEAD into $CAND"
gitc() { git -C "$CAND" -c user.name=upgrade-mhvtl -c user.email=upgrade-mhvtl@localhost "$@"; }
CAND_VER="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$CAND/Cargo.toml" | head -n 1)"
newer() { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$1" ]; }
NEW_VER="$CAND_VER"
STAMPED=0
if ! newer "$CAND_VER" "$PREV_VER"; then
    NEW_VER="$(echo "$PREV_VER" | awk -F. '{ printf "%d.%d.%d", $1, $2, $3 + 1 }')"
    python3 - "$CAND" "$CAND_VER" "$NEW_VER" <<'PY' || die "cannot stamp the candidate's version"
import re, sys
root, old, new = sys.argv[1:]
p = f"{root}/Cargo.toml"
s = open(p).read()
s2 = s.replace(f'version = "{old}"', f'version = "{new}"', 1)
assert s2 != s, "Cargo.toml has no version line"
open(p, "w").write(s2)
p = f"{root}/Cargo.lock"
s = open(p).read()
s2 = s.replace(f'name = "tapectl"\nversion = "{old}"', f'name = "tapectl"\nversion = "{new}"', 1)
assert s2 != s, "Cargo.lock has no tapectl package"
open(p, "w").write(s2)
PY
    STAMPED=1
fi
if [ ! -f "$CAND/docs/releases/$NEW_VER.md" ]; then
    cat >"$CAND/docs/releases/$NEW_VER.md" <<EOF
# tapectl $NEW_VER — a stand-in made by scripts/upgrade-mhvtl.sh

This clone's candidate, never a release.

## Upgrading

Nothing beyond \`first-run.sh --upgrade\` (the harness's stub).
EOF
fi
if [ -n "$(git -C "$CAND" status --porcelain)" ]; then
    { gitc add -A && gitc commit --quiet -m "upgrade-mhvtl.sh: the candidate as $NEW_VER, in this clone only"; } >/dev/null \
        || die "cannot commit the stamp in $CAND"
fi
if [ "$(git -C "$CAND" describe --exact-match HEAD 2>/dev/null)" != "v$NEW_VER" ]; then
    gitc tag -a "v$NEW_VER" -m "upgrade-mhvtl.sh: the candidate, in this clone only" || die "cannot tag v$NEW_VER in $CAND"
fi
[ -z "$(git -C "$CAND" status --porcelain)" ] || die "the candidate clone is not clean after the stamp"
echo "upgrade: candidate v$NEW_VER$( [ "$STAMPED" = 1 ] && printf ' (stamped from %s in its clone)' "$CAND_VER")"

build() { # build <clone> <out dir>
    local rc
    ( cd "$1" && CARGO_TARGET_DIR="$TARGET" flock -w 1200 -E 99 /scratch/tapectl-build.lock cargo build --locked --quiet )
    rc=$?
    [ "$rc" -eq 99 ] && die "timed out waiting for /scratch/tapectl-build.lock (run this script bare, not inside a flock)"
    [ "$rc" -eq 0 ] || die "cargo build failed in $1"
    install -m 0755 "$TARGET/debug/tapectl" "$2/tapectl" || die "cannot copy the binary built in $1"
}
echo "upgrade: building $FROM_TAG and v$NEW_VER (debug, $TARGET)"
build "$PREV" "$RUN/builds/prev"
build "$CAND" "$RUN/builds/cand"
PREV_LINE="$("$RUN/builds/prev/tapectl" --version)"
CAND_LINE="$("$RUN/builds/cand/tapectl" --version)"
case "$PREV_LINE" in "tapectl $PREV_VER ($FROM_TAG, "*) ;; *) die "the previous build says '$PREV_LINE'" ;; esac
case "$CAND_LINE" in "tapectl $NEW_VER (v$NEW_VER, "*) ;; *) die "the candidate build says '$CAND_LINE'" ;; esac
MIG_TOP="$(find "$CAND/src/db/migrations" -maxdepth 1 -name '[0-9]*.sql' -printf '%f\n' | sed 's/_.*//' | sort -n | tail -n 1)"
MIG_TOP=$((10#$MIG_TOP))

# ---------- the stand-ins and the profile ----------
export XDG_STATE_HOME="$RUN/state"
export TAPECTL_HOME_POINTER="$RUN/home-pointer"
export TAPECTL_INSTALL_BIN="$RUN/install/tapectl"
export TAPECTL_UNIT_DIR="$RUN/units"
install -m 0755 "$RUN/builds/prev/tapectl" "$TAPECTL_INSTALL_BIN" || die "cannot install the previous binary"
sudo -u "$SVC_USER" -H test -x "$TAPECTL_INSTALL_BIN" || die "$SVC_USER cannot execute $TAPECTL_INSTALL_BIN (an ancestor of $RUN is not traversable?)"
# The backup unit an earlier install left: it backed up to old-backups, so the
# preflight records that directory and step 12 offers what is left in it.
mkdir -p "$RUN/old-backups" "$RUN/backups"
cat >"$TAPECTL_UNIT_DIR/tapectl-backup.service" <<EOF
$STANDIN_MARK: an earlier install's backup unit, read by the preflight only.
[Service]
Environment=TAPECTL_BACKUP_DIR=$RUN/old-backups
EOF
sudo chown "$SVC_USER" "$RUN/old-backups" "$RUN/backups" && sudo chmod 0700 "$RUN/old-backups" "$RUN/backups" \
    || die "cannot hand the backup directories to $SVC_USER"
# The stand-in poll timer: transient (/run), a service that does nothing, a
# calendar that never comes. Removed on exit, whatever the result.
standin_remove() {
    sudo systemctl stop tapectl-drive-poll.timer tapectl-drive-poll.service >/dev/null 2>&1
    for u in tapectl-drive-poll.timer tapectl-drive-poll.service; do
        if sudo grep -qF "$STANDIN_MARK" "/run/systemd/system/$u" 2>/dev/null; then sudo rm -f "/run/systemd/system/$u"; fi
    done
    sudo systemctl daemon-reload
}
trap standin_remove EXIT
standin_remove
for u in tapectl-drive-poll.timer tapectl-drive-poll.service; do
    [ ! -e "/run/systemd/system/$u" ] || die "/run/systemd/system/$u exists and is not this script's stand-in"
done
sudo tee /run/systemd/system/tapectl-drive-poll.service >/dev/null <<EOF
$STANDIN_MARK
[Unit]
Description=upgrade-mhvtl.sh stand-in for the drive poll (does nothing)
[Service]
Type=oneshot
ExecStart=/bin/true
EOF
sudo tee /run/systemd/system/tapectl-drive-poll.timer >/dev/null <<EOF
$STANDIN_MARK
[Unit]
Description=upgrade-mhvtl.sh stand-in for the drive-poll timer
[Timer]
OnCalendar=2099-01-01 00:00:00
EOF
sudo systemctl daemon-reload && sudo systemctl start tapectl-drive-poll.timer || die "cannot start the stand-in timer"
systemctl is-active --quiet tapectl-drive-poll.timer || die "the stand-in timer is not active"

ESCROW_KEY="$RUN/escrow.key"
age-keygen -o "$ESCROW_KEY" 2>/dev/null || die "age-keygen failed"
ESCROW_PUB="$(age-keygen -y "$ESCROW_KEY")"
for u in a b; do
    mkdir -p "$RUN/src/keepsake/$u"
    echo "unit $u of the #505 upgrade scenario" >"$RUN/src/keepsake/$u/README"
    head -c "$((UNIT_MIB * 1024 * 1024))" /dev/urandom >"$RUN/src/keepsake/$u/payload.bin"
done
PROFILE="$RUN/profile"
cat >"$PROFILE" <<EOF
# Generated by scripts/upgrade-mhvtl.sh: the dev VM's mhvtl recipe, every path
# moved into this run's workspace. Read by both releases' first-run.sh.
. "$REPO/contrib/hosts/vm-desk1-mhvtl.profile"
HOME_DIR="$RUN/home"
TAPECTL="$TAPECTL_INSTALL_BIN"
DEVICE="$DEVICE"
DGEN="$DGEN"
OPERATOR="$USER"
ESCROW_PUBLIC_KEY="$ESCROW_PUB"
WORK_DIR="$RUN/work"
STAGING_DIR="$RUN/staging"
DAR_BIN="$(command -v dar)"
BACKUP_DIR="$RUN/backups"
KIT_OUT="$RUN/kit"
LABEL="$LABEL"
LOCATIONS=(
  "upg-rack|the upgrade scenario's shelf"
)
COLLECTIONS=(
  "keepsake|$RUN/src/keepsake|family|1"
)
EOF
FR_LOG="$RUN/state/tapectl/first-run.log"
DB="$RUN/home/tapectl.db"
tci() { sudo -u "$SVC_USER" -H "$TAPECTL_INSTALL_BIN" --home "$RUN/home" "$@" </dev/null; }
db_version() { # db_version FILE: PRAGMA user_version, read-only, as the service user
    sudo -u "$SVC_USER" -H python3 - "$1" <<'PY'
import sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True, timeout=10)
print(c.execute("PRAGMA user_version").fetchone()[0], [r[0] for r in c.execute("PRAGMA integrity_check")] == ["ok"])
PY
}
file0_label() { # the label File 0 carries, read with mt and dd alone
    mt -f "$DEVICE" rewind >/dev/null 2>&1 || return 1
    dd if="$DEVICE" bs=512k count=1 2>/dev/null | tr -d '\0' | sed -n 's/^label = "\(.*\)"$/\1/p' | head -1
}
upgrade() { bash "$CAND/scripts/first-run.sh" --profile "$PROFILE" --auto --upgrade --skip-build --tapectl "$RUN/builds/cand/tapectl" "$@"; }
has() { grep -F -- "$2" "$1" >/dev/null || { echo "$1 does not say: $2"; return 1; }; }
# listed FILE PATH: step 12 lists PATH as a candidate (its "  <size>  <path>"
# line), which its explanation's mention of a path is not.
listed() { python3 -c 'import re, sys
sys.exit(0 if any(re.fullmatch(r"\s+\S+  " + re.escape(sys.argv[2]), l) for l in open(sys.argv[1]).read().splitlines()) else 1)' "$1" "$2"; }
hasnt() { ! grep -F -- "$2" "$1" >/dev/null || { echo "$1 says what it must not: $2"; grep -F -- "$2" "$1" | head -3; return 1; }; }

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
    echo "upgrade: ${#CHECKS[@]} checks, $failed failed — $FROM_TAG -> v$NEW_VER, drive $DEVICE"
    if [ "$failed" -eq 0 ] && [ "${1:-}" != stop ]; then
        echo "upgrade: GREEN — issue #505's acceptance holds"
        if [ "$KEEP" = 0 ]; then sudo rm -rf "$RUN"; echo "upgrade: workspace removed (--keep keeps it)"; fi
        exit 0
    fi
    echo "upgrade: RED — the workspace is kept for reading: $RUN (first-run log: $FR_LOG)"
    exit 1
}
must() { [ "${RESULT[$1]}" = PASS ] || finish stop; }

# ---------- phase P: the previous release installed, and a production tape ----------
echo "upgrade: phase P — $FROM_TAG's first-run.sh --from 5 --to 11; output $RUN/P.out"
step_install_previous() {
    bash "$PREV/scripts/first-run.sh" --profile "$PROFILE" --auto --from 5 --to 11 </dev/null >"$RUN/P.out" 2>&1 \
        || { tail -40 "$RUN/P.out"; return 1; }
    [ "$(tci --version)" = "$PREV_LINE" ] || { echo "the installed binary is not $FROM_TAG"; return 1; }
    tci unit list
}
check install_previous step_install_previous
must install_previous
step_production_tape() {
    local u
    for u in keepsake/a keepsake/b; do
        tci snapshot create "$u" && tci stage create "$u" || return 1
    done
    mt -f "$DEVICE" rewind && mt -f "$DEVICE" erase || { echo "cannot blank cartridge $P_TAG"; return 1; }
    tci volume init "$LABEL" --device "$DEVICE" --force --yes || return 1
    tci volume write "$LABEL" --device "$DEVICE" --yes --keep-staging || return 1
    [ "$(file0_label)" = "$LABEL" ] || { echo "File 0 of $P_TAG does not name $LABEL"; return 1; }
}
check production_tape step_production_tape
must production_tape
P_SERIAL=""
step_p_serial() {
    local b
    b="$(tci cartridge list --json | python3 -c 'import json,sys; r=json.load(sys.stdin); assert len(r) == 1, r; print(r[0]["barcode"])')" || return 1
    P_SERIAL="$(tci cartridge info "$b" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["serial_number"] or "")')" || return 1
    [ -n "$P_SERIAL" ] || { echo "volume init bound cartridge $b with no chip serial"; return 1; }
    echo "cartridge $b, chip serial $P_SERIAL, holds $LABEL"
}
check production_cartridge_known step_p_serial
must production_cartridge_known
S_BEFORE="$(db_version "$DB" | cut -d' ' -f1)"

# ---------- phase U1: --upgrade, refused at the rehearsal ----------
echo "upgrade: phase U1 — --upgrade --barcode $P_SERIAL (the production cartridge); output $RUN/U1.out"
step_u1() {
    local rc
    upgrade --barcode "$P_SERIAL" </dev/null >"$RUN/U1.out" 2>&1; rc=$?
    [ "$rc" -eq 1 ] || { echo "the upgrade exited $rc, not 1 (the refusal)"; tail -30 "$RUN/U1.out"; return 1; }
    has "$RUN/U1.out" "$P_SERIAL is a cartridge this catalog knows" && has "$RUN/U1.out" "Nothing was erased" \
        && has "$RUN/U1.out" "first-run.sh's step 12 stopped (exit 1"
}
check u1_rehearsal_refuses_the_production_cartridge step_u1
must u1_rehearsal_refuses_the_production_cartridge
U_STATE="$RUN/state/tapectl/upgrade-$NEW_VER"
BACKUP="$RUN/backups/tapectl-pre-$NEW_VER.db"
KEPT="$RUN/install/tapectl-$PREV_VER"
step_u1_backup() {
    sudo -u "$SVC_USER" -H test -s "$BACKUP" || { echo "no backup at $BACKUP"; return 1; }
    [ "$(db_version "$BACKUP")" = "$S_BEFORE True" ] || { echo "the backup reads '$(db_version "$BACKUP")', not schema $S_BEFORE intact"; return 1; }
    grep -qx "backup=$BACKUP" "$U_STATE" || { echo "$U_STATE records no backup=$BACKUP"; return 1; }
    # Taken by the OLD binary: first-run.sh's `tc` runs the installed one, and its
    # backup comes before the one install that replaced it.
    python3 - "$FR_LOG" "$TAPECTL_INSTALL_BIN" "$BACKUP" <<'PY'
import sys
log, binp, backup = sys.argv[1:]
lines = open(log).read().splitlines()
b = next((i for i, l in enumerate(lines) if l.endswith(f"$ tc db backup --to {backup}")), None)
i = next((i for i, l in enumerate(lines) if f"install -m 0755" in l and l.rstrip().endswith(binp)), None)
print("backup line", b, "install line", i)
sys.exit(0 if b is not None and i is not None and b < i else 1)
PY
}
check u1_catalog_backed_up_by_the_old_binary step_u1_backup
step_u1_binaries() {
    [ "$("$KEPT" --version)" = "$PREV_LINE" ] || { echo "$KEPT is not $FROM_TAG"; return 1; }
    [ "$("$TAPECTL_INSTALL_BIN" --version)" = "$CAND_LINE" ] || { echo "the installed binary is not v$NEW_VER: $("$TAPECTL_INSTALL_BIN" --version)"; return 1; }
}
check u1_old_binary_kept_new_installed step_u1_binaries
step_u1_schema() {
    [ "$(db_version "$DB")" = "$MIG_TOP True" ] || { echo "the catalog reads '$(db_version "$DB")', not schema $MIG_TOP"; return 1; }
    has "$RUN/U1.out" "the catalog's schema: $S_BEFORE → $MIG_TOP"
    [ "$S_BEFORE" != "$MIG_TOP" ] || { has "$RUN/U1.out" "no migration applied" && echo "no migration between $FROM_TAG and v$NEW_VER: N -> N only"; }
}
check u1_migrations_applied step_u1_schema
step_u1_steps() {
    has "$RUN/U1.out" "== Step 14 —" && has "$RUN/U1.out" "install-systemd.sh --dry-run" \
        && has "$RUN/U1.out" "docs/releases/$NEW_VER.md, \"Upgrading\":" \
        && has "$RUN/U1.out" "--auto does not stop here" \
        && has "$RUN/U1.out" '$ tc status' && has "$RUN/U1.out" '$ tc audit' \
        && has "$RUN/U1.out" '$ tc report failed-stages'
}
check u1_steps_6_to_8_ran step_u1_steps
step_u1_tape_and_timer() {
    [ ! -d "$RUN/work/tapectl-lifecycle" ] || [ -z "$(ls -A "$RUN/work/tapectl-lifecycle")" ] \
        || { echo "a rehearsal ran: $(ls "$RUN/work/tapectl-lifecycle")"; return 1; }
    [ "$(file0_label)" = "$LABEL" ] || { echo "File 0 of $P_TAG no longer names $LABEL"; return 1; }
    [ "$(tci volume info "$LABEL" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')" = sealed ] \
        || { echo "$LABEL is not sealed in the catalog"; return 1; }
    has "$RUN/U1.out" "sudo systemctl stop tapectl-drive-poll.timer" && has "$RUN/U1.out" "tapectl-drive-poll.timer started again"
    systemctl is-active --quiet tapectl-drive-poll.timer || { echo "the stand-in timer is not active after the refused run"; return 1; }
}
check u1_production_cartridge_untouched_timer_restarted step_u1_tape_and_timer

# ---------- phase U2: cartridge T, the upgrade resumed ----------
step_load_t() {
    local status origin slot
    status="$(mtx -f "$CHG_SG" status)" || return 1
    origin="$(echo "$status" | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    [ -n "$origin" ] || { echo "the changer does not name P's slot"; return 1; }
    mtx -f "$CHG_SG" unload "$origin" "$DTE" || return 1
    slot="$(mtx -f "$CHG_SG" status | grep -E "Storage Element [0-9]+:Full" | grep "VolumeTag=[EF][0-9]*$GEN" \
        | grep -v "VolumeTag=$P_TAG" | head -1 | sed -n 's/.*Storage Element \([0-9]*\):Full.*/\1/p')"
    [ -n "$slot" ] || { echo "no second $GEN cartridge"; return 1; }
    mtx -f "$CHG_SG" load "$slot" "$DTE" || return 1
    T_TAG="$(mtx -f "$CHG_SG" status | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"
    [ -n "$T_TAG" ] && [ "$T_TAG" != "$P_TAG" ] || { echo "the drive holds '$T_TAG'"; return 1; }
    echo "cartridge T=$T_TAG loaded from slot $slot"
}
T_TAG=""
check load_test_cartridge step_load_t
must load_test_cartridge
echo "upgrade: phase U2 — --upgrade --barcode $T_TAG --to 11 (resumed); output $RUN/U2.out"
step_u2() {
    local rc sampler
    rm -f "$RUN/timer.stop"
    ( while [ ! -e "$RUN/timer.stop" ]; do systemctl is-active tapectl-drive-poll.timer; sleep 0.5; done ) >"$RUN/timer-U2.txt" 2>&1 &
    sampler=$!
    upgrade --barcode "$T_TAG" --to 11 </dev/null >"$RUN/U2.out" 2>&1; rc=$?
    touch "$RUN/timer.stop"; wait "$sampler"
    [ "$rc" -eq 0 ] || { echo "the resumed upgrade exited $rc"; tail -40 "$RUN/U2.out"; return 1; }
    has "$RUN/U2.out" "resuming the upgrade from $PREV_VER to $NEW_VER" \
        && has "$RUN/U2.out" "taken already: $BACKUP" && has "$RUN/U2.out" "kept already: $KEPT" \
        && has "$RUN/U2.out" "installed already: $CAND_LINE" \
        && has "$RUN/U2.out" "stopping before upgrade step 12"
    [ "$(grep -cF -- '$ tc db backup --to' "$FR_LOG")" -eq 1 ] \
        || { echo "the catalog was not backed up exactly once across both runs"; return 1; }
}
check u2_upgrade_resumes step_u2
must u2_upgrade_resumes
step_u2_rehearsal() {
    local marker
    marker="$RUN/state/tapectl/rehearsal-ok-$(sha256sum "$TAPECTL_INSTALL_BIN" | cut -c1-16)"
    [ -f "$marker" ] || { echo "no rehearsal marker for the installed binary ($marker)"; return 1; }
    has "$RUN/U2.out" "$T_TAG is not a cartridge this catalog knows" && has "$RUN/U2.out" "rehearsal green"
    grep -qx inactive "$RUN/timer-U2.txt" || { echo "the stand-in timer was never seen stopped during the rehearsal"; sort "$RUN/timer-U2.txt" | uniq -c; return 1; }
    [ "$(tail -n 1 "$RUN/timer-U2.txt")" = active ] && systemctl is-active --quiet tapectl-drive-poll.timer \
        || { echo "the stand-in timer is not active after the run"; return 1; }
    sort "$RUN/timer-U2.txt" | uniq -c
}
check u2_rehearsal_green_timer_stopped_and_restarted step_u2_rehearsal
step_u2_kit() {
    local a b
    a="$(sha256sum "$RUN/kit/tapectl-$NEW_VER" | cut -d' ' -f1)" || return 1
    b="$(sha256sum "$TAPECTL_INSTALL_BIN" | cut -d' ' -f1)"
    [ "$a" = "$b" ] || { echo "the kit's tapectl-$NEW_VER is not the installed binary"; return 1; }
    grep -F "$a" "$RUN/kit/COVER.txt" >/dev/null || { echo "COVER.txt does not name $a"; return 1; }
    has "$RUN/U2.out" "the kit's tapectl-$NEW_VER is the installed binary" \
        && has "$RUN/U2.out" "no backup unit is started"
}
check u2_kit_regenerated_with_the_new_binary step_u2_kit

# ---------- phase U3: the tidy, and the rollback ----------
FAKEHOME="$RUN/fakehome"
step_u3() {
    local rc p
    mkdir -p "$FAKEHOME/tapectl-1.7.0-0123abcd" "$FAKEHOME/tapectl-99.0.0-89abcdef" "$FAKEHOME/old-kit" \
        "$RUN/work/tapectl-lifecycle/run-20200101-000000" || return 1
    echo x >"$FAKEHOME/tapectl-1.7.0-0123abcd/tapectl"
    echo x >"$FAKEHOME/old-kit/COVER.txt"
    cp "$RUN/builds/prev/tapectl" "$RUN/install/tapectl-1.6.0" || return 1
    sudo -u "$SVC_USER" -H touch "$RUN/old-backups/tapectl-20261001T000000Z.db" || return 1
    HOME="$FAKEHOME" upgrade --from 12 </dev/null >"$RUN/U3.out" 2>&1; rc=$?
    [ "$rc" -eq 0 ] || { echo "the tidy run exited $rc"; tail -30 "$RUN/U3.out"; return 1; }
    for p in "$FAKEHOME/tapectl-1.7.0-0123abcd" "$RUN/work/tapectl-lifecycle/run-20200101-000000" \
             "$FAKEHOME/old-kit" "$RUN/old-backups/tapectl-20261001T000000Z.db" "$RUN/install/tapectl-1.6.0"; do
        listed "$RUN/U3.out" "$p" || { echo "not listed: $p"; return 1; }
        sudo test -e "$p" || { echo "$p was removed under --auto"; return 1; }
    done
    has "$RUN/U3.out" "--auto lists only: nothing removed" || return 1
    for p in "$FAKEHOME/tapectl-99.0.0-89abcdef" "$KEPT" "$TAPECTL_INSTALL_BIN" "$RUN/kit" "$RUN/backups" \
             "$RUN/home" "$FAKEHOME" "$CAND" \
             "$(find "$RUN/work/tapectl-lifecycle" -mindepth 1 -maxdepth 1 -name 'run-*' | sort | tail -n 1)"; do
        ! listed "$RUN/U3.out" "$p" || { echo "listed, and must not be: $p"; return 1; }
    done
    grep -q '^finished=' "$U_STATE" || { echo "$U_STATE records no finish"; return 1; }
}
check u3_tidy_lists_and_keeps step_u3
# The rollback step 13 prints: the import while the NEW binary is installed (an
# older binary cannot open a catalog a newer one migrated), then the old binary
# back, then fsck with it. Run here as printed, --yes for the import's consent.
step_rollback() {
    local imp cpl
    imp="$(grep -n -F "db import $BACKUP" "$RUN/U3.out" | head -1 | cut -d: -f1)"
    cpl="$(grep -n -F "sudo cp -p $KEPT $TAPECTL_INSTALL_BIN" "$RUN/U3.out" | head -1 | cut -d: -f1)"
    [ -n "$imp" ] && [ -n "$cpl" ] && [ "$imp" -lt "$cpl" ] \
        || { echo "step 13 does not print the import (line ${imp:-none}) before the binary swap (line ${cpl:-none})"; return 1; }
    tci --yes db import "$BACKUP" || return 1
    sudo cp -p "$KEPT" "$TAPECTL_INSTALL_BIN" || return 1
    [ "$("$TAPECTL_INSTALL_BIN" --version)" = "$PREV_LINE" ] || { echo "the installed binary is not $FROM_TAG after the swap"; return 1; }
    tci db fsck || return 1
    [ "$(db_version "$DB")" = "$S_BEFORE True" ] || { echo "the catalog reads '$(db_version "$DB")' after the rollback"; return 1; }
    [ "$(tci volume info "$LABEL" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')" = sealed ] \
        || { echo "$LABEL is not sealed in the restored catalog"; return 1; }
}
check rollback_as_printed_restores_the_old_install step_rollback

# ---------- R: refusals ----------
step_r_nothing_to_upgrade() {
    local rc
    # After the rollback the old binary is installed again; put the new one back
    # so the installed version equals the candidate's.
    sudo install -m 0755 "$RUN/builds/cand/tapectl" "$TAPECTL_INSTALL_BIN" || return 1
    XDG_STATE_HOME="$RUN/state-r1" upgrade --to 1 </dev/null >"$RUN/R1.out" 2>&1; rc=$?
    [ "$rc" -eq 1 ] || { echo "exit $rc, not 1"; cat "$RUN/R1.out"; return 1; }
    has "$RUN/R1.out" "the installed tapectl is already $NEW_VER" && has "$RUN/R1.out" "there is nothing to upgrade"
}
check r_same_version_refused step_r_nothing_to_upgrade
step_r_untagged() {
    local rc
    git clone --quiet "$CAND" "$RUN/untagged" && git -C "$RUN/untagged" -c user.name=u -c user.email=u@localhost commit --quiet --allow-empty -m "not a release" \
        || return 1
    bash "$RUN/untagged/scripts/first-run.sh" --profile "$PROFILE" --auto --upgrade --to 1 </dev/null >"$RUN/R2.out" 2>&1; rc=$?
    [ "$rc" -eq 1 ] || { echo "exit $rc, not 1"; cat "$RUN/R2.out"; return 1; }
    has "$RUN/R2.out" "not exactly an annotated release tag"
}
check r_untagged_checkout_refused step_r_untagged
step_r_step_range() {
    local rc
    upgrade --from 14 </dev/null >"$RUN/R3.out" 2>&1; rc=$?
    [ "$rc" -eq 2 ] || { echo "exit $rc, not 2"; cat "$RUN/R3.out"; return 1; }
    has "$RUN/R3.out" "takes an upgrade step from 1 to 13"
}
check r_step_out_of_range_refused step_r_step_range

finish
