#!/usr/bin/env bash
# check-tapectl-op.sh — issue #432's proof for the tapectl-op wrapper, with NO
# tape, no mhvtl and no real tapectl: it renders the wrapper exactly as
# scripts/install-systemd.sh would (--render-dir), points it at a probe script
# that reports its own CPU and I/O priority, and runs it through the real sudo.
#
# What it proves, each with its own PASS/FAIL line:
#
#   raised     every tape command (volume write/resume/verify/..., restore ...,
#              cartridge erase, collection run, quick-archive, drive poll) runs
#              at the knobs' nice value and best-effort/realtime/idle I/O class,
#              wherever the global options stand (--home X, --home=X, -qy,
#              --json after the subcommand)
#   plain      every other command, and a LABEL that merely equals a tape
#              subcommand's name (`volume info write`, `volume init write`,
#              `--home write status`), runs at the caller's own priority. This
#              is the positive control for "raised": the same probe, the same
#              sudo, a different verdict
#   parity     a raised command runs as the same user with the same HOME,
#              USER, LOGNAME and supplementary groups as the old
#              `sudo -u USER -H` form (the groups open /dev/nst*), so tapectl
#              finds the same home and the same drive
#   fallback   with root's sudo unavailable (a shim that refuses `sudo -n nice`
#              and `sudo -v`, as home2's sudoers rule does without a password)
#              a tape command still RUNS, at normal priority, and says so on
#              stderr
#   off        --tape-nice '' renders a wrapper that never raises
#   names      with --tapectl BIN: every tape subcommand the wrapper names
#              exists in that binary, and the only global options that take a
#              value are --config and --home (a new one must be added to the
#              wrapper's skip list)
#
# Needs: sudo that runs without a password (the probe is run as the service
# user), the service user (default: tapectl), util-linux's runuser, ionice.
# It writes only under a temporary directory it removes.
#
# Usage: scripts/check-tapectl-op.sh [--user NAME] [--tape-nice N]
#            [--tape-ionice ARGS] [--tapectl BINARY] [--keep]
# Run it from a shell at NORMAL priority (it refuses otherwise: a caller
# already at the raised priority would make "plain" and "raised" the same).
# Exit: 0 all pass, 1 a check failed, 2 a precondition failed.
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SVC_USER=tapectl
NICE=-10
IONICE="-c2 -n0"
BIN=""
KEEP=0
die() { echo "check-tapectl-op: PRECONDITION FAILED: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in
    --user) SVC_USER="${2:-}"; shift 2 ;;
    --tape-nice) NICE="${2:-}"; shift 2 ;;
    --tape-ionice) IONICE="${2:-}"; shift 2 ;;
    --tapectl) BIN="${2:-}"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,/^set -uo pipefail/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1 (--help lists them)" ;;
  esac
done

[ -n "$NICE" ] || die "--tape-nice must be set: this script proves the raise"
command -v sudo >/dev/null || die "sudo is required"
sudo -n true 2>/dev/null || die "sudo needs a password here; the probe runs through sudo non-interactively"
id "$SVC_USER" >/dev/null 2>&1 || die "no '$SVC_USER' user (--user)"
command -v runuser >/dev/null || [ -x /usr/sbin/runuser ] || die "runuser (util-linux) is required"
command -v ionice >/dev/null || die "ionice is required"
REAL_SUDO="$(command -v sudo)"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/check-tapectl-op.XXXXXX")" || die "cannot make a temp dir"
chmod 0755 "$WORK"   # the service user must be able to run the probe from here
cleanup() { [ "$KEEP" = 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT

# The probe stands in for tapectl: one line with its own priority and identity.
# nice is field 19 of /proc/PID/stat, counted after the ")" that ends the
# command name (the name may contain spaces, so a plain split would drift).
cat >"$WORK/probe" <<'EOF'
#!/bin/sh
set -- $(sed 's/^.*) //' "/proc/$$/stat")
nice=${17}
# Values carry no spaces (spaces become _ or ,), so a line splits into fields.
printf 'nice=%s io=%s uid=%s gid=%s groups=%s home=%s user=%s logname=%s\n' \
  "$nice" "$(ionice -p $$ | tr -d '\n' | tr ' ' _)" "$(id -u)" "$(id -g)" "$(id -G | tr ' ' ,)" "$HOME" "$USER" "$LOGNAME"
EOF
chmod 0755 "$WORK/probe"
sudo -u "$SVC_USER" -H test -x "$WORK/probe" || die "$SVC_USER cannot execute $WORK/probe (is TMPDIR traversable?)"

render() { # render DIR [installer args...]
  local d="$1"; shift
  "$REPO/scripts/install-systemd.sh" --render-dir "$d" --user "$SVC_USER" --tapectl "$WORK/probe" "$@" >"$WORK/render.out" 2>&1 \
    || { cat "$WORK/render.out" >&2; die "install-systemd.sh --render-dir failed"; }
}
render "$WORK/on" --tape-nice "$NICE" --tape-ionice "$IONICE"
render "$WORK/off" --tape-nice ""

# Expected I/O field for the knobs: ionice prints '<class>: prio <n>', which
# the probe reports with its spaces as underscores.
want_io() {
  local c="" n="" w
  # shellcheck disable=SC2086
  set -- $IONICE
  while [ $# -gt 0 ]; do
    case "$1" in -c) c="$2"; shift 2 ;; -c?) c="${1#-c}"; shift ;; -n) n="$2"; shift 2 ;; -n?) n="${1#-n}"; shift ;; *) shift ;; esac
  done
  case "$c" in 1) w=realtime ;; 2) w=best-effort ;; 3) w=idle ;; *) w=none ;; esac
  if [ "$c" = 3 ]; then echo "idle"; else echo "$w:_prio_${n:-0}"; fi
}
WANT_IO="$(want_io)"

# The caller's own priority, measured the way the plain cases are: the old
# wrapper form, the same probe. Also the baseline of the 'parity' check.
BASE="$(sudo -u "$SVC_USER" -H "$WORK/probe")" || die "the legacy 'sudo -u $SVC_USER -H probe' failed"
field() { printf '%s\n' "$2" | tr ' ' '\n' | sed -n "s/^$1=//p" | head -1; }
BASE_NICE="$(field nice "$BASE")"
BASE_IO="$(field io "$BASE")"
if [ "$BASE_NICE" = "$NICE" ] || [ "$BASE_IO" = "$WANT_IO" ]; then
  die "this shell already runs at nice $BASE_NICE / '$BASE_IO', which is what a raised command gets: run it from a shell at normal priority"
fi
echo "check-tapectl-op: knobs nice $NICE, ionice '$IONICE' (expect '$WANT_IO'); the caller's own priority: nice $BASE_NICE, '$BASE_IO'"

FAILED=0; N=0
pass() { N=$((N + 1)); printf '  [PASS] %s\n' "$1"; }
fail() { N=$((N + 1)); FAILED=$((FAILED + 1)); printf '  [FAIL] %s\n' "$1"; shift; [ $# -eq 0 ] || printf '         %s\n' "$@"; }

# expect KIND WRAPPER_DIR ARGS...: run the wrapper, judge its one line.
expect() {
  local kind="$1" dir="$2" out err line n io label; shift 2
  label="$kind: tapectl-op $*"
  out="$("$dir/tapectl-op" "$@" 2>"$WORK/err")"; err="$(cat "$WORK/err")"
  line="$out"; n="$(field nice "$line")"; io="$(field io "$line")"
  case "$kind" in
    raised)
      if [ "$n" = "$NICE" ] && [ "$io" = "$WANT_IO" ]; then pass "$label"; else fail "$label" "got nice=$n io='$io' (want $NICE / '$WANT_IO')" "stdout: $line" "stderr: $err"; fi ;;
    plain)
      if [ "$n" = "$BASE_NICE" ] && [ "$io" = "$BASE_IO" ]; then pass "$label"; else fail "$label" "got nice=$n io='$io' (want $BASE_NICE / '$BASE_IO')" "stdout: $line" "stderr: $err"; fi ;;
  esac
}

echo "raised: every tape command, with global options anywhere before the subcommand"
for args in \
  "volume write L1 --device /dev/x --yes" \
  "volume resume L1" "volume verify L1 --full --json" \
  "volume compact-write L2" "volume compact L1 --to L2" \
  "volume read-slices --from L1" "volume compact-read L1" \
  "restore unit u" "restore file u f" "restore volume L1" "restore raw-volume L1 /out" \
  "cartridge erase --device /dev/x" "collection run keepsake" "drive poll" \
  "quick-archive vol /some/path" \
  "--home /x --quiet volume write L1" "--home=/x volume write L1" "-qy volume write L1" \
  "--json --home /x -v volume verify L1" "--config /c/config.toml volume resume L1" \
  "volume --json write L1" "volume -q verify L1" "--dry-run collection run c" ; do
  # shellcheck disable=SC2086
  expect raised "$WORK/on" $args
done

echo "plain: everything else, and labels that equal a tape subcommand's name"
for args in \
  "status" "unit list" "report summary" "audit" "catalog stats" "tenant list" \
  "volume info write" "volume info resume" "volume list" "volume init write" "volume plan" \
  "volume abort verify" "volume retire write" "volume move run" "volume identify" \
  "collection sync" "collection status" "collection plan" "cartridge list" "cartridge info erase" \
  "restore --help" "stage create write" "snapshot create volume" "tenant add run" \
  "catalog search volume write" "location add poll" "key list" "host check" "db backup" \
  "--home write status" "--home volume --quiet status" "--config volume volume list" \
  "-q volume info write" "help volume write" "volume --help" "--version" "write" "run" ; do
  # shellcheck disable=SC2086
  expect plain "$WORK/on" $args
done
expect plain "$WORK/on"   # no arguments at all

echo "off: --tape-nice '' never raises"
expect plain "$WORK/off" volume write L1
expect plain "$WORK/off" drive poll

echo "parity: a raised command runs as the same user, groups and home as the old sudo -u -H form"
RAISED="$("$WORK/on/tapectl-op" volume write L1 2>/dev/null)"
parity_ok=1
for f in uid gid groups home user logname; do
  if [ "$(field "$f" "$RAISED")" != "$(field "$f" "$BASE")" ]; then parity_ok=0; fail "parity: $f differs" "raised: $RAISED" "legacy: $BASE"; fi
done
[ "$parity_ok" = 1 ] && pass "parity: uid, gid, groups ($(field groups "$RAISED")), HOME ($(field home "$RAISED")), USER, LOGNAME match the legacy form"
[ "$(field uid "$RAISED")" = "$(id -u "$SVC_USER")" ] && pass "parity: it runs as $SVC_USER, not root" || fail "parity: it does not run as $SVC_USER" "$RAISED"

echo "fallback: root's sudo unavailable -> the command still runs, at normal priority, and says so"
mkdir -p "$WORK/shim"
cat >"$WORK/shim/sudo" <<EOF
#!/bin/sh
# Stands in for a sudoers rule that lets the operator run commands AS the
# service user but not as root: refuses the wrapper's probes and a password prompt.
case "\$1" in -n | -v) exit 1 ;; esac
exec $REAL_SUDO "\$@"
EOF
chmod 0755 "$WORK/shim/sudo"
out="$(PATH="$WORK/shim:$PATH" "$WORK/on/tapectl-op" volume write L1 2>"$WORK/err")"; err="$(cat "$WORK/err")"
n="$(field nice "$out")"
if [ "$n" = "$BASE_NICE" ] && [ "$(field uid "$out")" = "$(id -u "$SVC_USER")" ]; then pass "fallback: ran as $SVC_USER at the caller's priority (nice $n)"; else fail "fallback: did not run plain as $SVC_USER" "stdout: $out" "stderr: $err"; fi
case "$err" in *"NORMAL priority"*) pass "fallback: said so on stderr" ;; *) fail "fallback: no notice on stderr" "stderr: $err" ;; esac
out="$(PATH="$WORK/shim:$PATH" "$WORK/on/tapectl-op" status 2>"$WORK/err")"; err="$(cat "$WORK/err")"
if [ -z "$err" ] && [ "$(field nice "$out")" = "$BASE_NICE" ]; then pass "fallback: a non-tape command says nothing"; else fail "fallback: a non-tape command was noisy or raised" "stdout: $out" "stderr: $err"; fi

if [ -n "$BIN" ]; then
  echo "names: the wrapper's words against $BIN"
  [ -x "$BIN" ] || die "--tapectl $BIN is not executable"
  top="$("$BIN" --help 2>&1 | sed -n '/^Commands:/,/^$/p' | awk 'NR>1 && NF {print $1}')"
  valued="$("$BIN" --help 2>&1 | sed -n '/^Options:/,$p' | sed -n 's/^ *\(-[a-zA-Z], \)\{0,1\}--\([a-z-]*\) <.*/\2/p' | sort | tr '\n' ' ')"
  [ "$valued" = "config home " ] && pass "names: --config and --home are the only global options that take a value" \
    || fail "names: global options that take a value are '$valued', the wrapper skips only 'config home'"
  printf '%s\n' "$top" | grep -x quick-archive >/dev/null && pass "names: quick-archive is a top-level command" || fail "names: no top-level quick-archive"
  for pair in "volume write" "volume resume" "volume verify" "volume compact-write" "volume compact" "volume read-slices" "volume compact-read" \
              "restore unit" "restore file" "restore volume" "restore raw-volume" "cartridge erase" "collection run" "drive poll"; do
    set -- $pair
    if "$BIN" "$1" --help 2>&1 | sed -n '/^Commands:/,/^$/p' | awk 'NR>1 && NF {print $1}' | grep -x "$2" >/dev/null; then pass "names: $1 $2 exists"; else fail "names: '$1 $2' is not a subcommand of $BIN"; fi
  done
fi

echo
echo "check-tapectl-op: $N checks, $FAILED failed"
[ "$FAILED" -eq 0 ] && { echo "check-tapectl-op: GREEN"; exit 0; }
echo "check-tapectl-op: RED"; KEEP=1; echo "workspace kept: $WORK"; exit 1
