#!/usr/bin/env bash
# Browser-side regression tests for the mobile bridge.
#
# They are here rather than in `cargo test` because what they cover lives in `static/app.js`:
# IME submission, focus ownership, the board's status partition, the polling lifecycle and the
# keybar's byte encoding. Each one pulls the REAL function out of app.js and runs it — none of
# them re-implement the logic they check, so a behaviour change fails the test rather than
# quietly diverging from it.
#
# Two need a live bridge (they are skipped when it is not answering):
#   board-check    wants a /api/board payload   -> pass it as $1, or set BOARD_JSON
#   sidebar-check  attaches a real WebSocket    -> needs COPAD_WEB_BRIDGE_TOKEN
#
# Usage:  plugins/web-bridge/tests/run.sh [board.json]
set -u
cd "$(dirname "$0")"
board_json="${1:-${BOARD_JSON:-}}"
token="${COPAD_WEB_BRIDGE_TOKEN:-}"
fail=0

for t in ime-check focus-check poll-check keybar-check nav-check preserve-check; do
    printf '  %-16s ' "$t"
    if timeout 60 node "$t.mjs" >/dev/null 2>&1; then echo PASS; else echo FAIL; fail=1; fi
done

printf '  %-16s ' board-check
if [ -n "$board_json" ] && [ -f "$board_json" ]; then
    if timeout 60 node board-check.mjs "$board_json" >/dev/null 2>&1; then echo PASS; else echo FAIL; fail=1; fi
else
    echo "SKIP (no board.json; curl /api/board into a file and pass it)"
fi

printf '  %-16s ' sidebar-check
if [ -n "$token" ]; then
    if timeout 60 node sidebar-check.mjs "$token" 40 >/dev/null 2>&1; then echo PASS; else echo FAIL; fail=1; fi
else
    echo "SKIP (no COPAD_WEB_BRIDGE_TOKEN)"
fi

exit $fail
