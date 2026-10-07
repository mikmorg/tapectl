#!/usr/bin/env bash
#
# tapectl scheduled drive health poll (issue #309; ADR-0012, 2026-10-07
# item 29).
#
# Runs `tapectl drive poll` — read-only, through the SCSI generic node only:
# the drive's log pages and, when a cartridge is loaded, its chip. Never a
# tape node, never a tape motion. Everything it reads is journalled verbatim
# in the catalog before it is judged.
#
# Exit-code contract:
#
#   tapectl drive poll  0 = reading recorded, the drive reported nothing  -> success
#                       1 = reading recorded, the drive raised a TapeAlert
#                           or reported an unrecovered error              -> failure
#                      75 = a tapectl command holds the drive (or the
#                           catalog); nothing was read                    -> no ping, logged
#             anything else = no reading (the sg node could not be read,
#                           the config is wrong, tapectl stopped)         -> failure
#
# A raised TapeAlert is a hardware fact, not a policy finding, so unlike the
# audit's warnings it is a failure here: this is the check that exists to go
# red. It is a SEPARATE check from the audit's (its own URL), so the audit's
# advisory semantics (ADR-0004) are untouched.
#
# Optional healthchecks.io-style pinging: set TAPECTL_DRIVE_HEALTHCHECK_URL.
# The poll's own output is sent as the ping's body, so the check's log shows
# which flags were raised. If the URL is unset, or curl is missing, or the
# ping fails, the run's own exit status is unchanged — monitoring must never
# be able to break the thing it monitors (issue #97's fail-open rule).

set -uo pipefail
# NOTE: deliberately no `set -e`: the poll's nonzero exit is data.

TAPECTL=${TAPECTL_BIN:-tapectl}
HC=${TAPECTL_DRIVE_HEALTHCHECK_URL:-}

ping_hc() {
	# $1: path suffix ("/start", "/fail", "" for success); $2: body (optional)
	[ -n "$HC" ] || return 0
	command -v curl >/dev/null 2>&1 || return 0
	printf '%s' "${2:-}" | head -c 10000 |
		curl -fsS -m 10 --retry 3 -o /dev/null --data-binary @- "${HC}${1}" || true
}

ping_hc /start

echo "== tapectl drive poll =="
out=$("$TAPECTL" drive poll 2>&1)
rc=$?
printf '%s\n' "$out"

case "$rc" in
0)
	echo "drive poll: recorded; the drive reported nothing"
	ping_hc "" "$out"
	;;
1)
	echo "drive poll: THE DRIVE REPORTED A PROBLEM — see above; the reading is in the catalog (tapectl report health)" >&2
	ping_hc /fail "$out"
	;;
75)
	# A tapectl command holds the drive (or the catalog): the poll read
	# nothing, on purpose — reading page 0x2E now could take the alerts that
	# command's own reading is owed. Neither success nor /fail; the next
	# timer run tries again.
	echo "drive poll: skipped — a tapectl command holds the drive or the catalog; nothing was read" >&2
	;;
*)
	echo "drive poll: no reading (exit $rc) — the poll did not complete; its error is above" >&2
	ping_hc /fail "$out"
	;;
esac

exit "$rc"
