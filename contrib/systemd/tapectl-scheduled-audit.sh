#!/usr/bin/env bash
#
# tapectl scheduled advisory run (issue #70).
#
# Runs the two read-only, no-tape-required commands and reports the result.
# This is the borgmatic/resticprofile wrapper shape ratified in #13: a timer
# that changes *when* the advisory half runs, never *whether* it blocks.
# Writing stays manual forever — nothing here touches a tape or a cartridge.
#
# Exit-code contract (ADR-0004, advisory principle):
#
#   tapectl audit  0 = clean       -> success
#                  1 = warnings    -> success, logged
#                  2 = violations  -> failure
#                 70 = the audit's own error -> no verdict, failure (#408)
#                 75 = catalog busy -> no verdict, logged, no ping (issue #377)
#           anything else = no verdict -> failure (issue #408)
#
# 70 (sysexits' EX_SOFTWARE) is what `tapectl audit` exits when it stops on
# an error (ADR-0012, 2026-10-07 item 19): before that ruling an error exited
# 2, the code for violations. "Anything else" is a run that never reached a
# verdict some other way: tapectl panicked (101), or exited a code this script
# does not know. Both are a failure — a broken audit must not go quiet — but
# neither is reported as violations, which would send the operator to the
# policy when the problem is the run itself.
#
# Warnings are deliberately NOT a failure. `audit` warns for ordinary drift
# (an overdue verification, a unit one copy short of its target) and paging on
# that would make the advisory audit behave like a blocking one, which is
# exactly what ADR-0004 forbids. Only a violation is a failure.
#
# `tapectl report verify-status` always exits 0 — it is a report, not a check.
# It runs here for the journal record only. The verification-age *check* with
# real exit codes lives in `audit` (its sixth §2.20 check), so nothing is lost
# by the report having no exit code, and giving one to a report command to
# satisfy a wrapper would be the tail wagging the dog.
#
# Optional healthchecks.io-style pinging: set TAPECTL_HEALTHCHECK_URL. If it is
# unset, or curl is missing, or the ping fails, the run's own exit status is
# unchanged. Monitoring must never be able to break the thing it monitors
# (same fail-open rule as the dar capability probe, issue #97).

set -uo pipefail
# NOTE: deliberately no `set -e`. We need `audit`'s nonzero exit as data, and
# `-e` would abort the script before we could capture, report, or ping it.

TAPECTL=${TAPECTL_BIN:-tapectl}
HC=${TAPECTL_HEALTHCHECK_URL:-}

ping_hc() {
	# $1: path suffix ("/start", "/fail", "" for success)
	[ -n "$HC" ] || return 0
	command -v curl >/dev/null 2>&1 || return 0
	curl -fsS -m 10 --retry 3 -o /dev/null "${HC}${1}" || true
}

ping_hc /start

echo "== tapectl audit =="
"$TAPECTL" audit
rc=$?

echo
echo "== tapectl report verify-status =="
"$TAPECTL" report verify-status || true

case "$rc" in
0)
	echo "audit: clean"
	ping_hc ""
	;;
1)
	echo "audit: warnings only (advisory — not a failure)"
	ping_hc ""
	;;
75)
	# Issue #377: another tapectl command held the catalog's write lock past
	# tapectl's wait. Not a verdict, so neither a success ping nor /fail: the
	# audit simply did not run. The next timer run tries again.
	echo "audit: catalog busy — another tapectl command holds the catalog; no verdict this run, retry later" >&2
	;;
2)
	echo "audit: VIOLATIONS (exit $rc)" >&2
	ping_hc /fail
	;;
70)
	# ADR-0012, 2026-10-07 item 19: the audit stopped on an error (a database
	# or config error, say) and reached no verdict. Its error is above.
	echo "audit: no verdict (exit $rc) — the audit stopped on an error; its error is above" >&2
	ping_hc /fail
	;;
*)
	# Issue #408: no verdict. The audit's own output above says why.
	echo "audit: no verdict (exit $rc) — the audit did not complete; its error is above" >&2
	ping_hc /fail
	;;
esac

exit "$rc"
