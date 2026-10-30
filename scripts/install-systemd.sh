#!/usr/bin/env bash
# install-systemd.sh — install (or remove) tapectl's systemd timers and the
# operator wrapper. Idempotent: run it again after changing an option and
# only what differs is rewritten; run it with nothing changed and it reports
# every file unchanged.
#
# What it installs (ADR-0012, 2026-09-24 amendment, items 5 and 6):
#
#   /etc/systemd/system/tapectl-audit.service    weekly advisory audit
#   /etc/systemd/system/tapectl-audit.timer        (Mon 09:00, read-only)
#   /etc/systemd/system/tapectl-backup.service   daily catalog backup
#   /etc/systemd/system/tapectl-backup.timer       (03:00, `db backup`, keep N)
#   /etc/systemd/system/tapectl-drive-poll.service  daily drive health poll
#   /etc/systemd/system/tapectl-drive-poll.timer      (06:30, /dev/sg* only;
#                                                     2026-10-07 item 29)
#   /usr/local/lib/tapectl/tapectl-scheduled-audit.sh       the three wrappers
#   /usr/local/lib/tapectl/tapectl-scheduled-backup.sh      the units run
#   /usr/local/lib/tapectl/tapectl-scheduled-drive-poll.sh
#   <backup dir>                                 created, owned by the service
#                                                user, mode 0700
#   /usr/local/bin/tapectl-op                    runs <tapectl> as <user> through sudo; a
#                                                tape command (volume write, ...) runs at
#                                                raised CPU/IO priority (see below)
#   /etc/tapectl/home                            the system home pointer: the
#                                                tapectl home's path, root 0644, so
#                                                an [ops] group member's plain
#                                                `tapectl status` finds it (#437)
#
# The units under contrib/systemd/ are written for the default service user
# (`tapectl`, home /var/lib/tapectl); this script rewrites User=, HOME=,
# TAPECTL_BACKUP_DIR=, TAPECTL_BACKUP_KEEP= and ReadWritePaths= for the
# options given, so the INSTALLED copies are always consistent with each
# other. Edit them by re-running this script, not by hand.
#
# Tape-command priority (issue #432, ADR-0012 2026-10-15 item 58): the drive
# wants an even feed, and a host that is busy elsewhere starves it. Priority can
# only be RAISED by root, so tapectl-op runs a tape command as
#   sudo nice -n <N> ionice <ARGS> runuser -u <user> -- <tapectl> ...
# (raised as root, then dropped to the service user: no limits.d grant, no new
# sudoers line), and tapectl-drive-poll.service gets Nice= and
# IOSchedulingClass=/IOSchedulingPriority= from the same two knobs. The tape
# commands are the ones that stream the drive: volume write, resume, verify,
# compact-write, compact, read-slices, compact-read; restore unit, file, volume,
# raw-volume; cartridge erase; collection run; quick-archive; drive poll.
# Everything else runs exactly as before. --tape-nice '' turns the raise off:
# tapectl-op then always runs `sudo -u <user> -H <tapectl>`, as it used to, and
# the drive-poll unit loses its Nice= and IOScheduling lines.
#
# What it never does: create the service user (scripts/first-run.sh step 5),
# install the tapectl binary (step 3), touch a tape or a device node, or —
# under --uninstall — remove the service user, its home, the binary, the
# backups or the home pointer. docs/install.md "Uninstall" has those commands.
#
# scripts/first-run.sh step 14 calls this script; it can also be run alone.
# Root work goes through sudo, so --dry-run needs no privilege at all.
set -euo pipefail

SVC_USER="tapectl"
SVC_HOME=""            # default: the account's home from getent
TAPECTL_HOME_DIR=""    # default: $SVC_HOME/.tapectl (tapectl's own default)
BACKUP_DIR="/var/backups/tapectl"
BACKUP_DIR_GIVEN=0   # 1 once --backup-dir is on the command line (issue #502)
KEEP=14
BIN="/usr/local/bin/tapectl"
# Tape-command priority (issue #432). The environment may set either knob (an
# empty TAPE_NICE disables the raise); a flag overrides the environment.
[ "${TAPE_NICE+set}" = set ] || TAPE_NICE=-10
[ "${TAPE_IONICE+set}" = set ] || TAPE_IONICE="-c2 -n0"
WRAPPER=1
START=1
DRY=0
UNINSTALL=0
RENDER_DIR=""

# Where the units go. TAPECTL_UNIT_DIR aims it elsewhere, as TAPECTL_HOME_POINTER
# aims the pointer, so a dry run or a test reads a scratch "installed" unit.
UNITDIR="${TAPECTL_UNIT_DIR:-/etc/systemd/system}"
LIBDIR=/usr/local/lib/tapectl
WRAPPER_PATH=/usr/local/bin/tapectl-op
WRAPPER_TEMPLATE=tapectl-op.in   # under contrib/systemd/
# The system home pointer (issue #437). The same variable the tapectl binary
# reads to find it, so a dry run or a test can aim both at a scratch path.
POINTER_PATH="${TAPECTL_HOME_POINTER:-/etc/tapectl/home}"
UNITS=(tapectl-audit.service tapectl-audit.timer tapectl-backup.service tapectl-backup.timer tapectl-drive-poll.service tapectl-drive-poll.timer)
TIMERS=(tapectl-audit.timer tapectl-backup.timer tapectl-drive-poll.timer)
SCRIPTS=(tapectl-scheduled-audit.sh tapectl-scheduled-backup.sh tapectl-scheduled-drive-poll.sh)

usage() {
  sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
  cat <<EOF

Usage: scripts/install-systemd.sh [options]
  --user NAME         service user the units run as (default: tapectl; must exist)
  --home DIR          that account's home (default: from getent passwd); the units set HOME=DIR
  --tapectl-home DIR  a tapectl home that is NOT DIR/.tapectl (sets TAPECTL_HOME= in the units)
  --backup-dir DIR    where the catalog copies go (default: $BACKUP_DIR) — put it on a
                      SECOND DISK; the script warns when it shares a filesystem or a disk with
                      the home. Over an installed backup unit that names another directory, a
                      run without it refuses: name the directory to keep, or the one to move to
  --keep N            catalog copies to keep (default: $KEEP)
  --tapectl PATH      the binary the units run and the wrapper execs (default: $BIN)
  --tape-nice N       nice value (-20..19) for tape commands, raised as root (default: -10; env
                      TAPE_NICE). Empty turns the raise off: tapectl-op is then the plain sudo -u
                      wrapper and the drive-poll unit has no Nice= or IOScheduling lines
  --tape-ionice ARGS  ionice arguments for tape commands, e.g. '-c2 -n0' (default: $TAPE_IONICE;
                      env TAPE_IONICE). Empty = no I/O priority change. Classes 1-3 only
  --render-dir DIR    render the six units and tapectl-op into DIR and stop: nothing is installed,
                      no sudo, no systemd, no service user needed (how the wrapper is tested)
  --no-wrapper        do not install $WRAPPER_PATH
  --no-start          install and enable the timers, but do not start them now
  --dry-run           print what would be done; change nothing
  --uninstall         remove the units, the wrappers and $WRAPPER_PATH (never the user,
                      the home, the binary or the backups)
  -h, --help
EOF
}
while [ $# -gt 0 ]; do
  case "$1" in
    --user) SVC_USER="$2"; shift 2 ;;
    --home) SVC_HOME="$2"; shift 2 ;;
    --tapectl-home) TAPECTL_HOME_DIR="$2"; shift 2 ;;
    --backup-dir) BACKUP_DIR="$2"; BACKUP_DIR_GIVEN=1; shift 2 ;;
    --keep) KEEP="$2"; shift 2 ;;
    --tapectl) BIN="$2"; shift 2 ;;
    --tape-nice) TAPE_NICE="$2"; shift 2 ;;
    --tape-ionice) TAPE_IONICE="$2"; shift 2 ;;
    --render-dir) RENDER_DIR="$2"; shift 2 ;;
    --no-wrapper) WRAPPER=0; shift ;;
    --no-start) START=0; shift ;;
    --dry-run) DRY=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[ -z "$RENDER_DIR" ] || DRY=1   # --render-dir installs nothing: every host-changing command only prints

# ---------------------------------------------------------------- plumbing
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO/contrib/systemd"
if [ -t 1 ]; then B=$'\e[1m'; R=$'\e[0m'; Y=$'\e[33m'; G=$'\e[32m'; RD=$'\e[31m'; else B=""; R=""; Y=""; G=""; RD=""; fi
SUDO=""; [ "$(id -u)" = 0 ] || SUDO="sudo"
note() { printf '   %s%s%s\n' "$Y" "$*" "$R"; }
ok()   { printf '   %s✓ %s%s\n' "$G" "$*" "$R"; }
die()  { printf '   %s✗ %s%s\n' "$RD" "$*" "$R" >&2; exit 1; }
# run: every command that changes the host goes through here. Under --dry-run
# it is printed with a "would:" prefix and not executed.
run() {
  if [ "$DRY" = 1 ]; then printf '   %swould:%s %s\n' "$B" "$R" "$*"; return 0; fi
  printf '   %s$ %s%s\n' "$B" "$*" "$R"; "$@"
}
# as_root: `run`, with sudo when needed.
as_root() { if [ -n "$SUDO" ]; then run "$SUDO" "$@"; else run "$@"; fi; }
# same_as PATH FILE: true when the installed PATH already has FILE's bytes.
same_as() { [ -f "$1" ] && cmp -s "$1" "$2"; }
# fs_id PATH: the device id of PATH's nearest existing ancestor.
fs_id() { local d="$1"; while [ ! -e "$d" ] && [ "$d" != / ]; do d="$(dirname "$d")"; done; stat -c %d "$d" 2>/dev/null || echo "?"; }
# disks_of PATH: the whole disks under the filesystem holding PATH's nearest
# existing ancestor, one per line, through LVM, dm-crypt and md (issue #502:
# two LVs on one disk are two filesystems, and fs_id cannot see they share it).
# Prints nothing when the source is not a block device (tmpfs, NFS, overlay)
# or the tools are missing: the check then says nothing rather than guess.
disks_of() {
  local d="$1" src
  while [ ! -e "$d" ] && [ "$d" != / ]; do d="$(dirname "$d")"; done
  src="$(findmnt -n -o SOURCE -T "$d" 2>/dev/null)" || return 0
  src="${src%%\[*}"   # a btrfs subvolume reads /dev/sdX[/subvol]
  case "$src" in /dev/*) ;; *) return 0 ;; esac
  lsblk -s -n -r -o NAME,TYPE "$src" 2>/dev/null | awk '$2 == "disk" { print $1 }' | sort -u
}

for f in "${UNITS[@]}" "${SCRIPTS[@]}" "$WRAPPER_TEMPLATE"; do [ -f "$SRC/$f" ] || die "$SRC/$f is missing — run this from a tapectl checkout"; done
command -v systemctl >/dev/null 2>&1 || { [ "$DRY" = 1 ] && note "systemctl not found — this host is not running systemd; showing the plan anyway" || die "systemctl not found — this host is not running systemd"; }

# ================================================================ uninstall
if [ "$UNINSTALL" = 1 ]; then
  printf '%stapectl systemd uninstall%s\n' "$B" "$R"
  for t in "${TIMERS[@]}"; do
    if systemctl list-unit-files "$t" 2>/dev/null | grep -q "^$t"; then as_root systemctl disable --now "$t"; else note "$t is not installed"; fi
  done
  for u in tapectl-audit.service tapectl-backup.service tapectl-drive-poll.service; do
    if systemctl is-active --quiet "$u" 2>/dev/null; then as_root systemctl stop "$u"; fi
  done
  for f in "${UNITS[@]}"; do
    [ -e "$UNITDIR/$f" ] && as_root rm -f "$UNITDIR/$f"
    [ -d "$UNITDIR/$f.d" ] && as_root rm -rf "$UNITDIR/$f.d"
  done
  for s in "${SCRIPTS[@]}"; do [ -e "$LIBDIR/$s" ] && as_root rm -f "$LIBDIR/$s"; done
  [ -d "$LIBDIR" ] && [ -z "$(ls -A "$LIBDIR" 2>/dev/null)" ] && as_root rmdir "$LIBDIR"
  [ -e "$WRAPPER_PATH" ] && as_root rm -f "$WRAPPER_PATH"
  as_root systemctl daemon-reload
  as_root systemctl reset-failed 'tapectl-*' || true
  ok "units, wrappers and $WRAPPER_PATH removed"
  note "left in place on purpose: the service user, its home, $BIN, the backups in $BACKUP_DIR, and the home pointer $POINTER_PATH"
  note "docs/install.md 'Uninstall' has the commands for those (remove the pointer when you delete the home it names)"
  exit 0
fi

# ================================================================ install
printf '%stapectl systemd install%s — units from %s\n' "$B" "$R" "$SRC"
# At least 1: with 0 the wrapper would prune the copy it just wrote.
case "$KEEP" in '' | *[!0-9]* | 0) die "--keep must be an integer >= 1, got '$KEEP'" ;; esac

# --- the tape-command priority knobs (issue #432). Both are baked into the
# rendered wrapper and units, so they are checked here, once, in the shapes
# nice(1), ionice(1) and systemd accept; the wrapper then never has to.
case "$TAPE_NICE" in
  '' | [0-9] | 1[0-9] | -[0-9] | -1[0-9] | -20) ;;
  *) die "--tape-nice must be an integer from -20 to 19 (or empty to turn the raise off), got '$TAPE_NICE'" ;;
esac
IO_CLASS=""; IO_PRIO=""   # ionice's numbers; systemd's names are derived below
if [ -n "$TAPE_NICE" ] && [ -n "$TAPE_IONICE" ]; then
  set -f; set -- $TAPE_IONICE; set +f   # word-split on purpose: it is a list of ionice arguments
  while [ $# -gt 0 ]; do
    case "$1" in
      -c) [ $# -ge 2 ] || die "--tape-ionice: -c needs a class"; IO_CLASS="$2"; shift 2 ;;
      -c[0-9]) IO_CLASS="${1#-c}"; shift ;;
      -n) [ $# -ge 2 ] || die "--tape-ionice: -n needs a level"; IO_PRIO="$2"; shift 2 ;;
      -n[0-9]) IO_PRIO="${1#-n}"; shift ;;
      *) die "--tape-ionice takes only -c CLASS and -n LEVEL (e.g. '-c2 -n0'), got '$1' in '$TAPE_IONICE'" ;;
    esac
  done
  case "$IO_CLASS" in
    1 | 2 | 3) ;;
    *) die "--tape-ionice needs -c 1 (realtime), 2 (best-effort) or 3 (idle), got '${IO_CLASS:-none}' in '$TAPE_IONICE'" ;;
  esac
  case "$IO_PRIO" in
    '') [ "$IO_CLASS" = 3 ] || die "--tape-ionice class $IO_CLASS needs -n LEVEL (0-7)" ;;
    [0-7]) [ "$IO_CLASS" != 3 ] || die "--tape-ionice: class 3 (idle) takes no -n level" ;;
    *) die "--tape-ionice -n must be 0-7, got '$IO_PRIO'" ;;
  esac
  case "$IO_CLASS" in 1) IO_CLASS_NAME=realtime ;; 2) IO_CLASS_NAME=best-effort ;; 3) IO_CLASS_NAME=idle ;; esac
fi
# What the wrapper bakes in: the validated, normalised form ('-c2 -n0'), or nothing.
IONICE_ARGS=""; [ -z "$IO_CLASS" ] || IONICE_ARGS="-c$IO_CLASS${IO_PRIO:+ -n$IO_PRIO}"
case "$SVC_USER$BIN" in *\'* | *\"* | *\\* | *\$* | *\`* | *\|* | *\&*) die "the service user and the binary path may not contain quotes, backslashes, \$, backticks, | or & (they are baked into $WRAPPER_PATH)" ;; esac

# The service user is step 5's job, not this script's: creating an account
# is a decision about the host, and the units are useless without the home
# and keys that account owns.
if id "$SVC_USER" >/dev/null 2>&1; then
  ok "service user $SVC_USER exists"
  [ -n "$SVC_HOME" ] || SVC_HOME="$(getent passwd "$SVC_USER" | cut -d: -f6)"
else
  [ "$DRY" = 1 ] || die "user $SVC_USER does not exist — scripts/first-run.sh step 5 creates it (or pass --user)"
  note "user $SVC_USER does not exist (dry run continues; the real run refuses)"
  [ -n "$SVC_HOME" ] || SVC_HOME="/var/lib/$SVC_USER"
fi
[ -n "$SVC_HOME" ] || die "no home directory for $SVC_USER — pass --home"
THOME="${TAPECTL_HOME_DIR:-$SVC_HOME/.tapectl}"
# The pointer holds this path, and tapectl refuses a relative one.
case "$THOME" in /*) ;; *) die "the tapectl home '$THOME' is not an absolute path — pass absolute --home / --tapectl-home" ;; esac
for p in "$SVC_HOME" "$THOME" "$BACKUP_DIR" "$BIN"; do case "$p" in *[[:space:]]*) die "path '$p' contains whitespace, which systemd's Environment= and ReadWritePaths= would split" ;; esac; done
# These are substituted into the units with sed (`s|...|$p|`), where | & and \
# are special: refuse them by name, as the user and binary are refused above (#470).
for p in "$SVC_HOME" "$THOME" "$BACKUP_DIR" "$LIBDIR" "${TAPECTL_HOME_DIR:-}"; do
  case "$p" in *\|* | *\&* | *\\*) die "path '$p' contains | & or \\, which the unit rendering cannot carry" ;; esac
done
if [ -x "$BIN" ]; then ok "binary: $BIN"; else note "$BIN is not there (or not executable) — the timers will fail until it is (first-run step 3 installs it)"; fi
printf '   user %s, HOME %s, tapectl home %s\n' "$SVC_USER" "$SVC_HOME" "$THOME"
printf '   home pointer: %s -> %s\n' "$POINTER_PATH" "$THOME"
printf '   backups: %s, keep %s\n' "$BACKUP_DIR" "$KEEP"
if [ -n "$TAPE_NICE" ]; then
  printf '   tape commands: nice %s%s (tapectl-op, tapectl-drive-poll.service)\n' "$TAPE_NICE" "${IONICE_ARGS:+, ionice $IONICE_ARGS}"
else
  printf '   tape commands: priority NOT raised (--tape-nice is empty)\n'
fi

# --- the backup directory: outside the home, on a second disk
case "$BACKUP_DIR" in
  "$THOME"|"$THOME"/*) die "--backup-dir $BACKUP_DIR is inside the tapectl home — a backup on the disk that holds the original is not a backup" ;;
esac
# Issue #502: a re-run without --backup-dir rendered the default over the
# directory the installed unit named, and the daily copies moved without a
# word. Moving them is a decision: without --backup-dir, refuse; with it, say so.
INSTALLED_BACKUP_DIR=""
INSTALLED_UNIT="$UNITDIR/tapectl-backup.service"
if [ -z "$RENDER_DIR" ] && [ -e "$INSTALLED_UNIT" ]; then
  [ -r "$INSTALLED_UNIT" ] || die "cannot read $INSTALLED_UNIT, so cannot tell which backup directory it names"
  INSTALLED_BACKUP_DIR="$(sed -n '/^Environment=TAPECTL_BACKUP_DIR=/{s///p;q;}' "$INSTALLED_UNIT")"
fi
if [ -n "$INSTALLED_BACKUP_DIR" ] && [ "$INSTALLED_BACKUP_DIR" != "$BACKUP_DIR" ]; then
  if [ "$BACKUP_DIR_GIVEN" = 0 ]; then
    why="the installed $UNITDIR/tapectl-backup.service backs the catalog up to $INSTALLED_BACKUP_DIR, and this run, given no --backup-dir, would move it to the default $BACKUP_DIR. Pass --backup-dir $INSTALLED_BACKUP_DIR to keep it, or --backup-dir $BACKUP_DIR to move it (scripts/first-run.sh: the host's --profile names it)"
    [ "$DRY" = 1 ] || die "$why"
    note "the real run refuses: $why"
  else
    note "moving the daily catalog backup from $INSTALLED_BACKUP_DIR to $BACKUP_DIR. The copies already in"
    note "$INSTALLED_BACKUP_DIR are no longer pruned; remove them by hand once $BACKUP_DIR holds copies."
  fi
fi
if [ -z "$RENDER_DIR" ]; then
  if [ "$(fs_id "$BACKUP_DIR")" = "$(fs_id "$THOME")" ]; then
    note "WARNING: $BACKUP_DIR is on the same filesystem as $THOME. The ruling asks for a second"
    note "disk (ADR-0012, 2026-09-24 amendment, item 5). Re-run with --backup-dir on another disk when you have one."
  else
    SHARED_DISKS="$(comm -12 <(disks_of "$BACKUP_DIR") <(disks_of "$THOME") | tr '\n' ' ')"
    if [ -n "$SHARED_DISKS" ]; then
      note "WARNING: $BACKUP_DIR and $THOME are different filesystems on the same disk (${SHARED_DISKS% })."
      note "The ruling asks for a second disk (ADR-0012, 2026-09-24 amendment, item 5). Re-run with --backup-dir on another disk when you have one."
    fi
  fi
fi
if [ -n "$RENDER_DIR" ]; then :   # rendering only: no directory is touched, not even in the plan
elif [ -d "$BACKUP_DIR" ]; then ok "backup dir $BACKUP_DIR exists"; else as_root install -d -m 0700 -o "$SVC_USER" "$BACKUP_DIR"; fi
[ "$DRY" = 1 ] || { as_root chown "$SVC_USER" "$BACKUP_DIR"; as_root chmod 0700 "$BACKUP_DIR"; }

# --- render the units for this host
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
render() { # render <unit>: contrib copy → $TMP/<unit>, substituted
  local u="$1" rw
  case "$u" in
    tapectl-backup.service) rw="$THOME $BACKUP_DIR" ;;
    *) rw="$THOME" ;;
  esac
  sed -e "s|^User=.*|User=$SVC_USER|" \
      -e "s|^Environment=HOME=.*|Environment=HOME=$SVC_HOME|" \
      -e "s|^Environment=TAPECTL_BACKUP_DIR=.*|Environment=TAPECTL_BACKUP_DIR=$BACKUP_DIR|" \
      -e "s|^Environment=TAPECTL_BACKUP_KEEP=.*|Environment=TAPECTL_BACKUP_KEEP=$KEEP|" \
      -e "s|^ReadWritePaths=.*|ReadWritePaths=$rw|" \
      -e "s|^ExecStart=.*/\([^/]*\)$|ExecStart=$LIBDIR/\1|" \
      "$SRC/$u" > "$TMP/$u"
  # The wrappers default to `tapectl` on PATH; pin the binary this host runs.
  case "$u" in *.service)
    sed -i "/^Environment=HOME=/a Environment=TAPECTL_BIN=$BIN" "$TMP/$u"
    [ -n "$TAPECTL_HOME_DIR" ] && sed -i "/^Environment=HOME=/a Environment=TAPECTL_HOME=$TAPECTL_HOME_DIR" "$TMP/$u"
    ;;
  esac
  # Issue #432: the one unit that runs a tape command (`drive poll`) takes the
  # same priority tapectl-op gives one. systemd raises it as root before it
  # drops to User=, so nothing needs a grant. Raise off (empty --tape-nice) =
  # no lines; no ionice arguments = no I/O lines.
  case "$u" in tapectl-drive-poll.service)
    if [ -z "$TAPE_NICE" ]; then
      sed -i -e '/^Nice=/d' -e '/^IOSchedulingClass=/d' -e '/^IOSchedulingPriority=/d' "$TMP/$u"
    else
      sed -i "s|^Nice=.*|Nice=$TAPE_NICE|" "$TMP/$u"
      if [ -z "$IO_CLASS" ]; then
        sed -i -e '/^IOSchedulingClass=/d' -e '/^IOSchedulingPriority=/d' "$TMP/$u"
      else
        sed -i "s|^IOSchedulingClass=.*|IOSchedulingClass=$IO_CLASS_NAME|" "$TMP/$u"
        if [ -n "$IO_PRIO" ]; then sed -i "s|^IOSchedulingPriority=.*|IOSchedulingPriority=$IO_PRIO|" "$TMP/$u"; else sed -i '/^IOSchedulingPriority=/d' "$TMP/$u"; fi
      fi
    fi
    ;;
  esac
  return 0
}
for u in "${UNITS[@]}"; do render "$u"; done
# tapectl-op from its template: the four values validated above, nothing else.
sed -e "s|@SVC_USER@|$SVC_USER|" -e "s|@TAPECTL@|$BIN|" \
    -e "s|@TAPE_NICE@|$TAPE_NICE|" -e "s|@TAPE_IONICE@|$IONICE_ARGS|" \
    "$SRC/$WRAPPER_TEMPLATE" > "$TMP/tapectl-op"
if [ -n "$RENDER_DIR" ]; then
  mkdir -p "$RENDER_DIR" || die "cannot create $RENDER_DIR"
  for u in "${UNITS[@]}"; do cp "$TMP/$u" "$RENDER_DIR/$u"; done
  install -m 0755 "$TMP/tapectl-op" "$RENDER_DIR/tapectl-op"
  ok "rendered ${#UNITS[@]} units and tapectl-op into $RENDER_DIR — nothing was installed"
  exit 0
fi
if [ "$DRY" = 1 ]; then
  printf '   rendered settings (lines that differ from contrib/systemd/):\n'
  # diff exits 1 when the files differ, which is the expected case here.
  for u in "${UNITS[@]}"; do diff "$SRC/$u" "$TMP/$u" | sed -n 's/^> /     '"$u"': /p' || true; done
fi

# --- install: scripts, units, wrapper — each only when its bytes differ
for s in "${SCRIPTS[@]}"; do
  if same_as "$LIBDIR/$s" "$SRC/$s"; then ok "$LIBDIR/$s unchanged"; else as_root install -D -m 0755 "$SRC/$s" "$LIBDIR/$s"; fi
done
CHANGED=0
for u in "${UNITS[@]}"; do
  if same_as "$UNITDIR/$u" "$TMP/$u"; then ok "$UNITDIR/$u unchanged"; else as_root install -D -m 0644 "$TMP/$u" "$UNITDIR/$u"; CHANGED=1; fi
done
if [ "$WRAPPER" = 1 ] && [ "$SVC_USER" = "$(id -un)" ]; then
  note "the service user is you — no $WRAPPER_PATH (it would sudo to yourself)"; WRAPPER=0
fi
if [ "$WRAPPER" = 1 ]; then
  if same_as "$WRAPPER_PATH" "$TMP/tapectl-op"; then ok "$WRAPPER_PATH unchanged"; else as_root install -D -m 0755 "$TMP/tapectl-op" "$WRAPPER_PATH"; fi
fi

# --- the system home pointer (issue #437, ADR-0012 2026-10-15 item 62):
# one absolute path, root-owned and world-readable, so an account without a
# ~/.tapectl of its own (an [ops] group member) finds the service user's home
# with a plain `tapectl status`. tapectl never reads it as root and never when
# the account has its own ~/.tapectl; it is rewritten here only when it differs.
printf '%s\n' "$THOME" > "$TMP/home-pointer"
if same_as "$POINTER_PATH" "$TMP/home-pointer"; then
  ok "$POINTER_PATH unchanged ($THOME)"
else
  if [ -f "$POINTER_PATH" ]; then note "$POINTER_PATH named $(head -n 1 "$POINTER_PATH" 2>/dev/null || echo '?') — replacing it with $THOME"; fi
  as_root install -d -m 0755 "$(dirname "$POINTER_PATH")"
  as_root install -m 0644 "$TMP/home-pointer" "$POINTER_PATH"
fi

# --- activate
as_root systemctl daemon-reload
if [ "$START" = 1 ]; then as_root systemctl enable --now "${TIMERS[@]}"; else as_root systemctl enable "${TIMERS[@]}"; fi
if [ "$CHANGED" = 1 ] && [ "$START" = 1 ]; then
  # A timer whose unit file changed keeps its old schedule until restarted.
  as_root systemctl restart "${TIMERS[@]}"
fi
if [ "$DRY" = 1 ]; then
  note "dry run: nothing was changed. The timers would be:"
  printf '     tapectl-audit.timer   %s\n' "$(sed -n 's/^OnCalendar=//p' "$TMP/tapectl-audit.timer")"
  printf '     tapectl-backup.timer  %s\n' "$(sed -n 's/^OnCalendar=//p' "$TMP/tapectl-backup.timer")"
  printf '     tapectl-drive-poll.timer  %s\n' "$(sed -n 's/^OnCalendar=//p' "$TMP/tapectl-drive-poll.timer")"
else
  run systemctl list-timers --all 'tapectl-*'
  ok "installed. Run one now to see it work:  sudo systemctl start tapectl-backup.service && journalctl -u tapectl-backup.service -n 30"
fi
