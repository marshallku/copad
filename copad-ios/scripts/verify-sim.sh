#!/usr/bin/env bash
# verify-sim.sh — build + launch the copad iOS shell in a Simulator and screenshot
# it. Deterministic from a clean state (picks the newest iOS runtime + an iPhone,
# boots it, uninstalls any prior copy, resets notification privacy). Requires
# Xcode + xcodegen. macOS only.
#
#   ./scripts/verify-sim.sh [server-url]
#
# server-url (optional, default http://127.0.0.1:7575): passed to the app via a
# `-ServerURL` launch arg that overrides persisted state every launch. If it's a
# 127.0.0.1 URL, the script preflights `/healthz` so a dead web-bridge is
# distinguishable from a shell failure.

set -euo pipefail
cd "$(dirname "$0")/.."

BUNDLE_ID="com.marshall.copad.ios"
SCHEME="copad-ios"
DERIVED="build"
SERVER_URL="${1:-http://127.0.0.1:7575}"
SHOT="${TMPDIR:-/tmp}/copad-ios-sim.png"

log() { printf '[verify-sim] %s\n' "$*" >&2; }

command -v xcodegen >/dev/null || { log "xcodegen not installed (brew install xcodegen)"; exit 1; }

# Preflight the local web-bridge so a server-down state isn't misread as a shell bug.
case "$SERVER_URL" in
  http://127.0.0.1*|http://localhost*)
    if ! curl -fsS -o /dev/null "${SERVER_URL%/}/healthz"; then
      log "web-bridge not responding at ${SERVER_URL%/}/healthz — start it first (see docs/mobile-access.md)."
      log "continuing anyway; the app will show its failure/retry UI."
    fi
    ;;
esac

log "generating project"
xcodegen generate >/dev/null

# Pick the newest available iOS runtime, then an iPhone device on it. Deterministic
# ordering (sort by runtime version desc) — not JSON enumeration order.
log "selecting simulator"
UDID="$(xcrun simctl list devices available --json | python3 -c '
import json,sys,re
d=json.load(sys.stdin)["devices"]
def ver(rt):
    m=re.search(r"iOS-(\d+)-(\d+)",rt); return (int(m.group(1)),int(m.group(2))) if m else (0,0)
best=None
for rt in sorted((k for k in d if "iOS" in k), key=ver, reverse=True):
    for dev in d[rt]:
        if "iPhone" in dev["name"]:
            best=dev["udid"]; break
    if best: break
print(best or "")
')"
[ -n "$UDID" ] || { log "no available iPhone simulator found"; exit 1; }
log "simulator udid=$UDID"

xcrun simctl boot "$UDID" 2>/dev/null || true
xcrun simctl bootstatus "$UDID" -b >/dev/null

log "building for simulator (no signing)"
xcodebuild -project copad-ios.xcodeproj -scheme "$SCHEME" \
  -destination "id=$UDID" -sdk iphonesimulator -configuration Debug \
  -derivedDataPath "$DERIVED" build >/dev/null

APP="$DERIVED/Build/Products/Debug-iphonesimulator/$SCHEME.app"
[ -d "$APP" ] || { log "built app not found at $APP"; exit 1; }

# Clean state: remove any prior install + reset notification permission so the
# permission prompt (if driven) is reproducible.
xcrun simctl uninstall "$UDID" "$BUNDLE_ID" 2>/dev/null || true
xcrun simctl privacy "$UDID" reset all "$BUNDLE_ID" 2>/dev/null || true

log "installing + launching (ServerURL=$SERVER_URL)"
xcrun simctl install "$UDID" "$APP"
xcrun simctl launch "$UDID" "$BUNDLE_ID" -ServerURL "$SERVER_URL" >/dev/null

sleep 3
xcrun simctl io "$UDID" screenshot "$SHOT" >/dev/null
log "screenshot: $SHOT"
echo "$SHOT"
