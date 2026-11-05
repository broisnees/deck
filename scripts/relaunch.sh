#!/bin/sh
# Build, install and reopen Deck, from anywhere, Deck's own terminals included.
#
#     scripts/relaunch.sh
#
# It detaches first: run from a terminal inside Deck, that terminal goes
# away when Deck quits, and the rest would go with it. The background
# service keeps running throughout, so jobs, agents and service-kept
# terminal tabs carry on and reattach when Deck opens again.
set -eu

cd "$(dirname "$0")/.."
APP="${DECK_APP:-/Applications/Deck.app}"
LOG="${TMPDIR:-/tmp}/deck-relaunch.log"

if [ "${DECK_RELAUNCH_DETACHED-}" != 1 ]; then
    DECK_RELAUNCH_DETACHED=1 nohup "$0" "$@" >"$LOG" 2>&1 </dev/null &
    echo "Relaunching Deck in the background; log: $LOG"
    exit 0
fi

echo "== $(date) relaunch"
scripts/install.sh
# Give Launch Services a moment with the new bundle.
sleep 1
# -n: a new window app even if macOS counts the background service (which
# older builds ran from inside the bundle) as Deck already running.
open -n "$APP"
echo "== reopened"
