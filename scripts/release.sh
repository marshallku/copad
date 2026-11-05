#!/usr/bin/env bash
# Cut a copad release: bump the workspace version, commit it, then tag +
# push the tag to trigger the Release workflow.
#
# Why this exists: the release version lives in TWO places that MUST agree —
#   1. the git tag `vX.Y.Z` — triggers `.github/workflows/release.yml`
#      (`on: push: tags: ["v*"]`) and names every release asset.
#   2. `[workspace.package] version` in Cargo.toml — what every binary
#      self-reports via `CARGO_PKG_VERSION`. comux's status-bar update hint,
#      `coctl update`, and copadd's daily check all compare THIS against the
#      latest GitHub release tag.
# release.yml sed-stamps the tag version at build time so release *binaries*
# always self-report correctly — but the COMMITTED Cargo.toml was bumped by
# hand, and got forgotten: v1.0.3 shipped while Cargo.toml still said 1.0.2,
# so every build-from-source (install-dev.sh / install-macos.sh / cargo build)
# saw `latest (1.0.3) > current (1.0.2)` and showed a spurious "⬆ update
# available" forever. This script makes the bump un-forgettable — version and
# tag are set together, in one atomic flow, so they can never drift again.
#
# Commits/tags with plain git ON PURPOSE (not ~/save.sh): this is a checked-in,
# public release tool that must run standalone (CI, a fresh clone, another
# contributor) without depending on a personal commit wrapper. A version-bump
# is a mechanical chore commit that doesn't need the review gates save.sh runs.
#
# Usage:
#   ./scripts/release.sh                  # pick patch/minor/major interactively
#   ./scripts/release.sh patch            # bump a level without picking (also: minor, major)
#   ./scripts/release.sh <version>        # e.g. ./scripts/release.sh 1.0.4
#   ./scripts/release.sh v1.0.4           # a leading `v` is tolerated + stripped
#   ./scripts/release.sh 1.0.4 --dry-run  # show every step, change nothing
#   ./scripts/release.sh --help
#
# With no argument it reads the current version out of Cargo.toml, offers the
# three bumps (plus a free-form "custom") and lets you choose — with `fzf` if
# it's installed, otherwise the bash `select` menu. Non-interactive callers
# (CI, a pipe) must still pass an explicit version or level.
#
# After it pushes the tag, watch the build at:
#   https://github.com/marshallku/copad/actions

set -euo pipefail

DEFAULT_BRANCH="master"

die() { echo "release: $*" >&2; exit 1; }

usage() {
    # `sed -E` for the optional-space strip: BSD sed's BRE has no `\?`, so the
    # portable-looking `s/^# \?//` left every `#` in place on macOS.
    sed -n '2,/^set -euo/p' "$0" | grep -E '^#( |$)' | sed -E 's/^# ?//'
}

VERSION=""
LEVEL=""
DRY_RUN=0
for arg in "$@"; do
    case "$arg" in
        -h|--help) usage; exit 0 ;;
        --dry-run) DRY_RUN=1 ;;
        -*) die "unknown flag: $arg (see --help)" ;;
        major|minor|patch)
            [ -z "$LEVEL" ] && [ -z "$VERSION" ] || die "version/level given twice: '${VERSION}${LEVEL}' and '$arg'"
            LEVEL="$arg"
            ;;
        *)
            [ -z "$VERSION" ] && [ -z "$LEVEL" ] || die "version/level given twice: '${VERSION}${LEVEL}' and '$arg'"
            VERSION="$arg"
            ;;
    esac
done

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository"
cd "$ROOT"

read_workspace_version() {
    awk -F'"' '/^\[workspace.package\]/{s=1;next} s&&/^version[[:space:]]*=/{print $2;exit}' "$1"
}

# Guards run before the version is resolved so a failure does not waste the bump prompt.
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "$DEFAULT_BRANCH" ] || die "on branch '$BRANCH', not '$DEFAULT_BRANCH' — release from $DEFAULT_BRANCH (or edit DEFAULT_BRANCH)"

[ -z "$(git status --porcelain)" ] || die "working tree is dirty — commit or stash first (a release must be a clean, reproducible snapshot)"

CURRENT="$(read_workspace_version Cargo.toml)"
[ -n "$CURRENT" ] || die "could not read [workspace.package] version from Cargo.toml"

# A `patch` bump off a PRERELEASE releases it (1.0.5-rc.1 -> 1.0.5). Build metadata carries
# no precedence, so 1.0.5+build still patches to 1.0.6.
bump() {
    local core rest pre major minor patch
    core="${CURRENT%%[-+]*}"
    rest="${CURRENT#"$core"}"
    case "$rest" in
        -*) pre="$rest" ;;   # -rc.1, or -rc.1+build
        *)  pre="" ;;        # empty, or +build only
    esac
    IFS=. read -r major minor patch <<< "$core"
    case "${major}${minor}${patch}" in
        *[!0-9]*|"") die "current version '${CURRENT}' is not X.Y.Z — pass an explicit version" ;;
    esac
    case "$1" in
        major) major=$((major + 1)); minor=0; patch=0 ;;
        minor) minor=$((minor + 1)); patch=0 ;;
        patch) [ -n "$pre" ] || patch=$((patch + 1)) ;;
    esac
    printf '%s.%s.%s\n' "$major" "$minor" "$patch"
}

pick_level() {
    # Initialized, not just declared: a bare `local x` is unset, and under `set -u` bash 5
    # aborts on `$x` when the picker exits without choosing (Ctrl-D out of `select`).
    local rows prompt typed PS3 choice="" line=""
    rows="$(printf '%s\n' \
        "patch   $(bump patch)   backwards-compatible fixes" \
        "minor   $(bump minor)   backwards-compatible features" \
        "major   $(bump major)   breaking changes" \
        "custom  -       type an exact version")"
    prompt="release: current ${CURRENT} — bump which part?"

    if command -v fzf >/dev/null 2>&1; then
        choice="$(printf '%s\n' "$rows" | fzf --height=~8 --reverse --no-multi \
            --header="$prompt" --prompt='bump> ')" || true
    else
        echo "$prompt" >&2
        PS3="choice> "
        select line in $(printf '%s\n' "$rows" | awk '{print $1}'); do
            [ -n "$line" ] || { echo "release: pick a number (or Ctrl-C to abort)" >&2; continue; }
            choice="$(printf '%s\n' "$rows" | awk -v l="$line" '$1 == l')"
            break
        done < /dev/tty
    fi

    [ -n "$choice" ] || die "no version chosen — aborted"
    if [ "${choice%% *}" = "custom" ]; then
        read -r -p "release: version (current ${CURRENT}): " typed < /dev/tty || die "aborted"
        printf '%s\n' "$typed"
    else
        printf '%s\n' "$choice" | awk '{print $2}'
    fi
}

if [ -n "$LEVEL" ]; then
    VERSION="$(bump "$LEVEL")"
elif [ -z "$VERSION" ]; then
    [ -t 0 ] && [ -t 2 ] \
        || { usage; echo; die "a version or bump level is required when not interactive, e.g. ./scripts/release.sh patch"; }
    VERSION="$(pick_level)"
    [ -n "$VERSION" ] || die "no version chosen — aborted"
fi

VERSION="${VERSION#v}"

# Match the SemVer grammar exactly (this is also the shape release.yml derives from
# GITHUB_REF_NAME): a loose pattern would let a bad version reach Cargo.toml, get committed,
# tagged and pushed, and then fail in CI. Prerelease identifiers are non-leading-zero
# numbers or contain a non-digit (`-rc.1`, `-0` pass; `-01` fails, as cargo rejects it);
# build identifiers may have leading zeros.
SEMVER_CORE='(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)'
PRE_ID='(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
SEMVER_PRE="(-${PRE_ID}(\\.${PRE_ID})*)?"
SEMVER_BUILD='(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?'
if ! printf '%s' "$VERSION" | grep -qE "^${SEMVER_CORE}${SEMVER_PRE}${SEMVER_BUILD}$"; then
    die "'$VERSION' is not a valid version (expected SemVer X.Y.Z, e.g. 1.0.4)"
fi
TAG="v${VERSION}"

run() {
    echo "  + $*"
    [ "$DRY_RUN" -eq 1 ] || "$@"
}

git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null 2>&1 \
    && die "tag ${TAG} already exists locally — bump to a new version"

# Non-fatal offline: a release can be cut now and pushed later.
if ! git fetch --quiet --tags origin 2>/dev/null; then
    echo "release: warning — could not fetch origin (offline?); skipping remote up-to-date + remote-tag checks"
else
    git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null 2>&1 \
        && die "tag ${TAG} already exists on origin — bump to a new version"
    if git rev-parse -q --verify "origin/${DEFAULT_BRANCH}" >/dev/null 2>&1; then
        BEHIND="$(git rev-list --count "HEAD..origin/${DEFAULT_BRANCH}")"
        [ "$BEHIND" -eq 0 ] || die "local ${DEFAULT_BRANCH} is behind origin by ${BEHIND} commit(s) — pull first so the tag points at the pushed tip"
    fi
fi

echo "release: ${CURRENT} -> ${VERSION}  (tag ${TAG}, branch ${BRANCH})"
[ "$DRY_RUN" -eq 1 ] && echo "release: --dry-run — no files written, no commit, no tag, no push"

if [ "$CURRENT" = "$VERSION" ]; then
    echo "release: Cargo.toml already at ${VERSION}; skipping the version-bump commit"
    BUMPED=0
else
    # Anchored to [workspace.package] so a dependency's `version = ` is never touched.
    TMP="$(mktemp)"
    awk -v v="$VERSION" '
        /^\[/ { in_wp = ($0 == "[workspace.package]") }
        in_wp && /^version[[:space:]]*=/ { sub(/=.*/, "= \"" v "\""); in_wp = 0 }
        { print }
    ' Cargo.toml > "$TMP"

    NEW="$(read_workspace_version "$TMP")"
    [ "$NEW" = "$VERSION" ] || { rm -f "$TMP"; die "version rewrite failed (got '${NEW}') — Cargo.toml unchanged"; }

    if [ "$DRY_RUN" -eq 1 ]; then
        rm -f "$TMP"
    else
        mv "$TMP" Cargo.toml
    fi
    BUMPED=1

    # Keep Cargo.lock's workspace versions in step with the release commit. Non-fatal.
    if command -v cargo >/dev/null 2>&1; then
        run cargo update --workspace --quiet || echo "release: warning — 'cargo update --workspace' failed; Cargo.lock may lag (harmless, CI regenerates)"
    else
        echo "release: warning — cargo not found; skipping Cargo.lock sync"
    fi

    run git add Cargo.toml Cargo.lock
    run git commit -m "chore(release): v${VERSION}"
fi

run git tag -a "${TAG}" -m "Release ${TAG}"
# One atomic push: separate pushes could leave the bump commit on origin without its tag,
# and the leftover local tag would block a re-run. Needs a remote with `--atomic`.
run git push --atomic origin "${BRANCH}" "${TAG}"

if [ "$DRY_RUN" -eq 1 ]; then
    echo "release: dry run complete — nothing was changed."
else
    echo "release: pushed ${TAG}. The Release workflow will build + publish it:"
    echo "  https://github.com/marshallku/copad/actions"
fi
