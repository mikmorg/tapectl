# shellcheck shell=bash
#
# volume_step13_state — where a volume label stands, for first-run.sh step 13
# to re-enter after an interrupted run (issue #414).
#
# Step 13 used to go straight from `volume init` to `volume write`. Re-entered
# after a write was interrupted (a 26-hour run makes that likely), init said
# "already exists", the write refused ("already has an unresolved write
# session ... volume resume") and the script died; after a manual `volume
# resume` the volume was sealed and the script said "choose a new label" and
# died. Either way the post-write block (verify --full, audit, the location
# move, the Heir Kit) never ran for that tape.
#
# Reads `tapectl volume info <label> --json` from the file named by $1 (the
# caller decides "absent" from the command's own refusal, so a busy catalog is
# never read as "no such volume") and prints one word:
#
#   fresh     initialized, no write session recorded: write it
#   resume    initialized with an `interrupted` session: `volume resume`
#             continues it from its frozen staging files, and `volume write`
#             refuses it
#   live      an `in_progress` session and no interrupted one: tapectl turns a
#             crashed session into `interrupted` whenever it opens the catalog
#             (as `volume info` just did), so this one has a live writer
#   planned   only a `planned` session: nothing reached the tape; `volume
#             abort` clears it, then the write runs again (`volume resume`
#             refuses a planned session and says so)
#   sealed    sealed and in service: only the post-write block is left (its
#             verify --full is run again on a re-entry: a full confirm also
#             records a full verification, so the JSON cannot tell "the
#             post-write verify ran" from "the write confirmed in full")
#   other     anything else (quarantined, retired, erased, an initialized
#             volume whose sessions all ended `aborted`, an unreadable file):
#             the caller stops and says why
#
# Sourced by scripts/first-run.sh. Kept in its own file with no `set -euo
# pipefail` prologue so a test can source it directly
# (tests/first_run_volume_state.rs).
volume_step13_state() {
    python3 - "${1:-}" <<'PY'
import json, sys
try:
    v = json.load(open(sys.argv[1]))
except Exception:
    print("other"); sys.exit(0)
status = v.get("status")
condition = v.get("condition")
writes = [w.get("status") for w in v.get("writes") or []]
if condition != "ok":
    print("other")
elif status == "initialized":
    if not writes:
        print("fresh")
    elif "interrupted" in writes:
        print("resume")
    elif "in_progress" in writes:
        print("live")
    elif "planned" in writes:
        print("planned")
    else:
        print("other")
elif status == "sealed":
    print("sealed")
else:
    print("other")
PY
}
