#!/usr/bin/env bash
#
# tapectl scheduled catalog backup (ADR-0012, 2026-09-24 amendment, item 5:
# "a timer-driven `db backup` to a second disk after every session").
#
# Copies the live catalog (`tapectl.db`) to a timestamped file in
# $TAPECTL_BACKUP_DIR with `tapectl db backup`, keeps the newest
# $TAPECTL_BACKUP_KEEP copies, and reports. Same wrapper shape as
# tapectl-scheduled-audit.sh: a timer decides *when*; nothing here touches a
# tape, a cartridge or a device node (the unit sets PrivateDevices=true).
#
# What this backs up, and what it does not:
#
#   - The CATALOG ONLY. `db backup` is SQLite's online backup API, so the copy
#     is consistent even while another tapectl command holds the database.
#   - NOT the private keys. `db backup --include-keys` exists but is opt-in
#     (issue #40): a key copy turns the backup destination into a key-escrow
#     point, and a catalog without keys is useless to a thief. The keys' backup
#     is the heir kit (`key escrow-kit`) and a copy of the home you make on
#     purpose — docs/install.md, "Moving to a new host".
#     TAPECTL_BACKUP_INCLUDE_KEYS=1 turns it on if you have decided that the
#     destination is as secret as the home itself.
#   - NOT config.toml. It is small and hand-editable; keep it in the same
#     place you keep the keys.
#
# The copy is checked for the SQLite header before it counts (a zero-byte or
# non-database file is a failure, not a backup), and `db fsck` runs against
# the LIVE database afterwards so a corrupt catalog is reported the same day
# it is backed up — a backup of a corrupt database is still worth having,
# but the failure must be visible in `systemctl status`.
#
# Retention prunes ONLY files this script named (tapectl-<UTC stamp>.db),
# newest first by name, which sorts by time because the stamp is ISO-8601
# basic. Anything else in the directory is left alone.
#
# Exit status: nonzero if the backup failed its header check, or fsck
# reported problems. Pruning failures are logged, never fatal.
#
# Optional healthchecks.io-style pinging: TAPECTL_HEALTHCHECK_URL, fail-open,
# same rule as the audit wrapper (monitoring must never break the thing it
# monitors).

set -uo pipefail
# NOTE: deliberately no `set -e`. Exit codes are data here, captured and
# reported; `-e` would abort before the report.

TAPECTL=${TAPECTL_BIN:-tapectl}
DIR=${TAPECTL_BACKUP_DIR:-/var/backups/tapectl}
KEEP=${TAPECTL_BACKUP_KEEP:-14}
INCLUDE_KEYS=${TAPECTL_BACKUP_INCLUDE_KEYS:-0}
HC=${TAPECTL_HEALTHCHECK_URL:-}

ping_hc() {
	# $1: path suffix ("/start", "/fail", "" for success)
	[ -n "$HC" ] || return 0
	command -v curl >/dev/null 2>&1 || return 0
	curl -fsS -m 10 --retry 3 -o /dev/null "${HC}${1}" || true
}

fail() {
	echo "backup: FAILED — $*" >&2
	ping_hc /fail
	exit 1
}

# At least 1: with 0 the prune below would delete the copy just written.
case "$KEEP" in
'' | *[!0-9]* | 0) fail "TAPECTL_BACKUP_KEEP must be an integer >= 1, got '$KEEP'" ;;
esac

ping_hc /start

# The unit's ProtectSystem=strict makes the directory read-only unless it is
# in ReadWritePaths, and the installer creates it before the first run; the
# mkdir is for a hand-run outside systemd.
mkdir -p "$DIR" || fail "cannot create $DIR"
[ -w "$DIR" ] || fail "$DIR is not writable by $(id -un)"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$DIR/tapectl-$STAMP.db"

echo "== tapectl db backup --to $OUT =="
if [ "$INCLUDE_KEYS" = 1 ]; then
	"$TAPECTL" db backup --to "$OUT" --include-keys
else
	"$TAPECTL" db backup --to "$OUT"
fi
rc=$?
[ "$rc" -eq 0 ] || fail "tapectl db backup exited $rc"

# A backup that is not a SQLite database is not a backup. The header is the
# 16 bytes "SQLite format 3\0"; checking it needs no sqlite3 binary.
if [ ! -s "$OUT" ] || [ "$(head -c 15 "$OUT" 2>/dev/null)" != "SQLite format 3" ]; then
	rm -f "$OUT"
	fail "$OUT is not a SQLite database (removed)"
fi
chmod 0600 "$OUT" || true
echo "backup: $OUT ($(stat -c %s "$OUT") bytes)"

echo
echo "== tapectl db fsck (live database) =="
"$TAPECTL" db fsck
fsck_rc=$?

echo
echo "== retention: keep newest $KEEP in $DIR =="
# Newest first by name (the stamp sorts chronologically); everything after
# the first $KEEP is pruned. `-maxdepth 1` and the exact glob mean nothing
# this script did not write is ever a candidate.
n=0
pruned=0
while IFS= read -r f; do
	n=$((n + 1))
	[ "$n" -le "$KEEP" ] && continue
	if rm -f -- "$f"; then
		pruned=$((pruned + 1))
		# A --include-keys sibling (`<name>.keys/`) goes with its database.
		[ -d "${f%.db}.keys" ] && rm -rf -- "${f%.db}.keys"
	else
		echo "retention: could not remove $f" >&2
	fi
done < <(find "$DIR" -maxdepth 1 -type f -name 'tapectl-[0-9]*T[0-9]*Z.db' -print | sort -r)
echo "retention: $((n - pruned)) kept, $pruned pruned"

if [ "$fsck_rc" -ne 0 ]; then
	echo "backup: written, but db fsck reported problems (exit $fsck_rc) — the LIVE catalog needs attention" >&2
	ping_hc /fail
	exit "$fsck_rc"
fi
echo "backup: ok"
ping_hc ""
exit 0
