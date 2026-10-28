#!/usr/bin/env bash
# Ships the copad iOS app to TestFlight (fastlane lanes in copad-ios/fastlane/Fastfile;
# decisions #176). The same script runs on this Mac and in .github/workflows/testflight.yml.
#
#   copad-ios/scripts/testflight.sh                   # build + upload from this Mac
#   copad-ios/scripts/testflight.sh --version 0.2.0   # … with that marketing version
#   copad-ios/scripts/testflight.sh --build-only      # signed ipa in build/fastlane, no upload
#   copad-ios/scripts/testflight.sh --check           # only the checks before uploading
#   copad-ios/scripts/testflight.sh --allow-dirty     # upload uncommitted changes too
#   copad-ios/scripts/testflight.sh --ci [--version X.Y.Z] [--check | --build-only]
#                                                     # upload from GitHub Actions instead: runs
#                                                     # testflight.yml on this (pushed) commit;
#                                                     # --check there only imports the certificate
#                                                     # and key, --build-only signs without uploading
#   copad-ios/scripts/testflight.sh status            # recent builds and their processing state
#
# The build number is TestFlight's latest + 1; the version is project.yml's MARKETING_VERSION
# unless --version says otherwise. A local upload refuses when copad-ios/ differs from the last
# commit, so every build is a commit someone can find again. Needs fastlane, xcodegen, the
# "Apple Distribution" identity in the keychain and the App Store Connect key
# (~/.appstoreconnect/private_keys/AuthKey_<id>.p8, or APP_STORE_CONNECT_KEY_PATH).
# Logs land in copad-ios/build/fastlane/testflight.log.
set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
    echo "this script is macOS-only" >&2
    exit 2
fi

IOS="$(cd "$(dirname "$0")/.." && pwd)"
MODE=upload
CI_RUN=false
ALLOW_DIRTY=false
VERSION=""
while [[ $# -gt 0 ]]; do
    case "$1" in
    status) MODE=status ;;
    --build-only) MODE=build ;;
    --check) MODE=check ;;
    --ci) CI_RUN=true ;;
    --allow-dirty) ALLOW_DIRTY=true ;;
    --version)
        VERSION="${2:-}"
        shift
        ;;
    -h | --help)
        sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "unknown argument: $1 (status | --build-only | --check | --ci | --allow-dirty | --version X.Y.Z)" >&2
        exit 2
        ;;
    esac
    shift
done
VERSION="${VERSION#v}"
if [[ -n "$VERSION" && ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "--version must be X.Y.Z" >&2
    exit 2
fi

if [[ "$MODE" == status ]]; then
    cd "$IOS/fastlane" && exec fastlane ios status
fi

if $CI_RUN; then
    command -v gh >/dev/null || { echo "gh is not installed" >&2; exit 2; }
    sha="$(git -C "$IOS" rev-parse HEAD)"
    git -C "$IOS" fetch -q origin
    if ! git -C "$IOS" branch -r --contains "$sha" | grep -q .; then
        echo "HEAD ($sha) is not pushed; CI builds what is on GitHub" >&2
        exit 1
    fi
    args=(-f "ref=$sha" -f "mode=$MODE")
    [[ -n "$VERSION" ]] && args+=(-f "version=$VERSION")
    gh workflow run testflight.yml "${args[@]}"
    echo "==> started testflight.yml ($MODE) for $(git -C "$IOS" rev-parse --short HEAD); watch it with: gh run watch"
    exit 0
fi

for tool in fastlane xcodegen; do
    command -v "$tool" >/dev/null || { echo "$tool is not installed" >&2; exit 2; }
done
KEY_ID="${APP_STORE_CONNECT_KEY_ID:-$(sed -n 's/^KEY_ID *= *ENV.fetch("APP_STORE_CONNECT_KEY_ID", "\(.*\)")/\1/p' "$IOS/fastlane/Fastfile")}"
KEY="${APP_STORE_CONNECT_KEY_PATH:-$HOME/.appstoreconnect/private_keys/AuthKey_$KEY_ID.p8}"
[[ -f "$KEY" ]] || { echo "App Store Connect key missing: $KEY" >&2; exit 2; }
security find-identity -v -p codesigning | grep -q "Apple Distribution" \
    || { echo "no \"Apple Distribution\" identity in the keychain" >&2; exit 2; }

if ! $ALLOW_DIRTY && [[ "$MODE" != build ]]; then
    dirty="$(git -C "$IOS" status --porcelain -- . ':!build' ':!fastlane/report.xml' ':!fastlane/README.md')"
    if [[ -n "$dirty" ]]; then
        echo "copad-ios/ differs from the last commit:" >&2
        echo "$dirty" >&2
        echo "commit first, or pass --allow-dirty" >&2
        exit 1
    fi
fi

if [[ "$MODE" == check ]]; then
    echo "==> ready to upload $(git -C "$IOS" rev-parse --short HEAD)"
    exit 0
fi

mkdir -p "$IOS/build/fastlane"
log="$IOS/build/fastlane/testflight.log"
lane=(ios beta)
[[ "$MODE" == build ]] && lane+=(upload:false)
[[ -n "$VERSION" ]] && lane+=("version:$VERSION")
echo "==> fastlane ${lane[*]} (log: $log)"
if ! (cd "$IOS/fastlane" && fastlane "${lane[@]}") >"$log" 2>&1; then
    tail -30 "$log" >&2
    echo "fastlane failed; full log: $log" >&2
    exit 1
fi

if [[ "$MODE" == build ]]; then
    echo "==> built $IOS/build/fastlane/copad.ipa"
    exit 0
fi
echo "==> uploaded $(git -C "$IOS" rev-parse --short HEAD)$($ALLOW_DIRTY && echo ' (+ uncommitted changes)'); processing takes a few minutes:"
(cd "$IOS/fastlane" && fastlane ios status 2>&1 | grep -E "=>" | sed "s/^\[[^]]*\]: //" | head -3) || true
