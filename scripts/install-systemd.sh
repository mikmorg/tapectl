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
#   /usr/local/lib/tapectl/tapectl-scheduled-audit.sh    the two wrappers
#   /usr/local/lib/tapectl/tapectl-scheduled-backup.sh   the units run
#   <backup dir>                                 created, owned by the service
#                                                user, mode 0700
#   /usr/local/bin/tapectl-op                    `exec sudo -u <user> -H <tapectl> "$@"`
#
# The units under contrib/systemd/ are written for the default service user
# (`tapectl`, home /var/lib/tapectl); this script rewrites User=, HOME=,
# TAPECTL_BACKUP_DIR=, TAPECTL_BACKUP_KEEP= and ReadWritePaths= for the
# options given, so the INSTALLED copies are always consistent with each
# other. Edit them by re-running this script, not by hand.
#
# What it never does: create the service user (scripts/first-run.sh step 5),
# install the tapectl binary (step 3), touch a tape or a device node, or —
# under --uninstall — remove the service user, its home, the binary or the
# backups. docs/install.md "Uninstall" has those commands.
#
# scripts/first-run.sh step 14 calls this script; it can also be run alone.
# Root work goes through sudo, so --dry-run needs no privilege at all.
set -euo pipefail

SVC_USER="tapectl"
SVC_HOME=""            # default: the account's home from getent
TAPECTL_HOME_DIR=""    # default: $SVC_HOME/.tapectl (tapectl's own default)
BACKUP_DIR="/var/backups/tapectl"
KEEP=14
BIN="/usr/local/bin/tapectl"
WRAPPER=1
START=1
DRY=0
UNINSTALL=0

UNITDIR=/etc/systemd/system
LIBDIR=/usr/local/lib/tapectl
WRAPPER_PATH=/usr/local/bin/tapectl-op
UNITS=(tapectl-audit.service tapectl-audit.timer tapectl-backup.service tapectl-backup.timer)
TIMERS=(tapectl-audit.timer tapectl-backup.timer)
SCRIPTS=(tapectl-scheduled-audit.sh tapectl-scheduled-backup.sh)

usage() {
  sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
  cat <<EOF

Usage: scripts/install-systemd.sh [options]
  --user NAME         service user the units run as (default: tapectl; must exist)
  --home DIR          that account's home (default: from getent passwd); the units set HOME=DIR
  --tapectl-home DIR  a tapectl home that is NOT DIR/.tapectl (sets TAPECTL_HOME= in the units)
  --backup-dir DIR    where the catalog copies go (default: $BACKUP_DIR) — put it on a
                      SECOND DISK; the script warns when it shares a filesystem with the home
  --keep N            catalog copies to keep (default: $KEEP)
  --tapectl PATH      the binary the units run and the wrapper execs (default: $BIN)
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
    --backup-dir) BACKUP_DIR="$2"; shift 2 ;;
    --keep) KEEP="$2"; shift 2 ;;
    --tapectl) BIN="$2"; shift 2 ;;
    --no-wrapper) WRAPPER=0; shift ;;
    --no-start) START=0; shift ;;
    --dry-run) DRY=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

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

for f in "${UNITS[@]}" "${SCRIPTS[@]}"; do [ -f "$SRC/$f" ] || die "$SRC/$f is missing — run this from a tapectl checkout"; done
command -v systemctl >/dev/null 2>&1 || { [ "$DRY" = 1 ] && note "systemctl not found — this host is not running systemd; showing the plan anyway" || die "systemctl not found — this host is not running systemd"; }

# ================================================================ uninstall
if [ "$UNINSTALL" = 1 ]; then
  printf '%stapectl systemd uninstall%s\n' "$B" "$R"
  for t in "${TIMERS[@]}"; do
    if systemctl list-unit-files "$t" 2>/dev/null | grep -q "^$t"; then as_root systemctl disable --now "$t"; else note "$t is not installed"; fi
  done
  for u in tapectl-audit.service tapectl-backup.service; do
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
  note "left in place on purpose: the service user, its home, $BIN, and the backups in $BACKUP_DIR"
  note "docs/install.md 'Uninstall' has the commands for those"
  exit 0
fi

# ================================================================ install
printf '%stapectl systemd install%s — units from %s\n' "$B" "$R" "$SRC"
# At least 1: with 0 the wrapper would prune the copy it just wrote.
case "$KEEP" in '' | *[!0-9]* | 0) die "--keep must be an integer >= 1, got '$KEEP'" ;; esac

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
for p in "$SVC_HOME" "$THOME" "$BACKUP_DIR" "$BIN"; do case "$p" in *[[:space:]]*) die "path '$p' contains whitespace, which systemd's Environment= and ReadWritePaths= would split" ;; esac; done
if [ -x "$BIN" ]; then ok "binary: $BIN"; else note "$BIN is not there (or not executable) — the timers will fail until it is (first-run step 3 installs it)"; fi
printf '   user %s, HOME %s, tapectl home %s\n' "$SVC_USER" "$SVC_HOME" "$THOME"
printf '   backups: %s, keep %s\n' "$BACKUP_DIR" "$KEEP"

# --- the backup directory: outside the home, on a second disk
case "$BACKUP_DIR" in
  "$THOME"|"$THOME"/*) die "--backup-dir $BACKUP_DIR is inside the tapectl home — a backup on the disk that holds the original is not a backup" ;;
esac
if [ "$(fs_id "$BACKUP_DIR")" = "$(fs_id "$THOME")" ]; then
  note "WARNING: $BACKUP_DIR is on the same filesystem as $THOME. The ruling asks for a second"
  note "disk (ADR-0012, 2026-09-24 amendment, item 5). Re-run with --backup-dir on another disk when you have one."
fi
if [ -d "$BACKUP_DIR" ]; then ok "backup dir $BACKUP_DIR exists"; else as_root install -d -m 0700 -o "$SVC_USER" "$BACKUP_DIR"; fi
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
  return 0
}
for u in "${UNITS[@]}"; do render "$u"; done
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
  cat > "$TMP/tapectl-op" <<EOF
#!/bin/sh
# tapectl-op — run tapectl as the service user that owns the catalog and keys.
# Installed by scripts/install-systemd.sh (docs/install.md); re-run that
# script to change the user or the binary rather than editing this file.
exec sudo -u $SVC_USER -H $BIN "\$@"
EOF
  if same_as "$WRAPPER_PATH" "$TMP/tapectl-op"; then ok "$WRAPPER_PATH unchanged"; else as_root install -D -m 0755 "$TMP/tapectl-op" "$WRAPPER_PATH"; fi
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
else
  run systemctl list-timers --all 'tapectl-*'
  ok "installed. Run one now to see it work:  sudo systemctl start tapectl-backup.service && journalctl -u tapectl-backup.service -n 30"
fi
