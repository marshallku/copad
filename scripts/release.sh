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
    # Print the header block (lines starting with "# ") as help text.
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

# Everything runs from the repo root so the awk/git paths are unambiguous.
ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository"
cd "$ROOT"

read_workspace_version() {
    awk -F'"' '/^\[workspace.package\]/{s=1;next} s&&/^version[[:space:]]*=/{print $2;exit}' "$1"
}

# The branch + clean-tree guards run BEFORE the version is resolved: failing
# them after making the user pick a bump would waste the prompt.
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "$DEFAULT_BRANCH" ] || die "on branch '$BRANCH', not '$DEFAULT_BRANCH' — release from $DEFAULT_BRANCH (or edit DEFAULT_BRANCH)"

[ -z "$(git status --porcelain)" ] || die "working tree is dirty — commit or stash first (a release must be a clean, reproducible snapshot)"

CURRENT="$(read_workspace_version Cargo.toml)"
[ -n "$CURRENT" ] || die "could not read [workspace.package] version from Cargo.toml"

# --- Resolve the target version --------------------------------------------

# Next version for a bump level. A prerelease/build suffix is dropped, and a
# `patch` bump off a PRERELEASE releases it (1.0.5-rc.1 -> 1.0.5) rather than
# skipping to 1.0.6 — that is what semver ordering means. Build metadata does
# NOT count: it carries no precedence, so 1.0.5+build must still patch to
# 1.0.6, not back to a 1.0.5 that ranks equal to what's already released.
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

# Present the three bumps and let the user choose. Rows are
# "<level> <version> <what it means>"; only the second field is consumed, so
# the description is free to change.
pick_level() {
    # `choice`/`line` are initialized, not just declared: a bare `local x` is
    # UNSET, and under `set -u` bash 5 aborts on `$x` when a picker exits
    # without choosing (Ctrl-D out of `select`). bash 3.2 tolerates it, so this
    # only bites on a modern bash.
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
        # `select` writes its menu + PS3 to stderr and reads stdin, so the
        # picker never pollutes the captured stdout.
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

# Tolerate a leading `v` (the tag carries it; the Cargo.toml version does not).
VERSION="${VERSION#v}"

# Validate against the official SemVer grammar. Hand-rolling a loose `[0-9.]+`
# would let a bad version reach Cargo.toml and — since the cargo-update sync is
# non-fatal — get committed, tagged, and pushed only for CI's build to reject
# it. So match SemVer precisely: X.Y.Z with no leading zeros in the numeric
# core; an optional prerelease (`-…`) whose dot-separated identifiers are each a
# non-leading-zero number OR contain a non-digit (so `1.2.3-rc.1` / `-0` pass but
# `-01` / `-.` do not — the latter is what cargo itself rejects); an optional
# build section (`+…`) whose identifiers are any non-empty alnum/hyphen run
# (leading zeros ARE allowed in build metadata per spec). This is the shape
# release.yml derives from GITHUB_REF_NAME.
SEMVER_CORE='(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)'
PRE_ID='(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'   # numeric (no leading 0) or alphanumeric
SEMVER_PRE="(-${PRE_ID}(\\.${PRE_ID})*)?"
SEMVER_BUILD='(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?'
if ! printf '%s' "$VERSION" | grep -qE "^${SEMVER_CORE}${SEMVER_PRE}${SEMVER_BUILD}$"; then
    die "'$VERSION' is not a valid version (expected SemVer X.Y.Z, e.g. 1.0.4)"
fi
TAG="v${VERSION}"

run() {
    # Echo the command; execute it unless --dry-run.
    echo "  + $*"
    [ "$DRY_RUN" -eq 1 ] || "$@"
}

# --- Preflight guards (branch + clean tree already checked above) -----------

git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null 2>&1 \
    && die "tag ${TAG} already exists locally — bump to a new version"

# Fetch so the ahead/behind and remote-tag checks see the truth. Non-fatal
# offline (you might be cutting a release to push later), just a warning.
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

# --- Bump the workspace version --------------------------------------------

if [ "$CURRENT" = "$VERSION" ]; then
    echo "release: Cargo.toml already at ${VERSION}; skipping the version-bump commit"
    BUMPED=0
else
    # Rewrite ONLY the `version = "..."` line inside the [workspace.package]
    # section (anchored so a dependency's `version = ` is never touched).
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

    # Keep Cargo.lock's workspace-member versions in step so the release commit
    # is internally consistent (CI builds without --locked, but a matching lock
    # avoids a confusing post-checkout diff). Non-fatal if cargo is unavailable.
    if command -v cargo >/dev/null 2>&1; then
        run cargo update --workspace --quiet || echo "release: warning — 'cargo update --workspace' failed; Cargo.lock may lag (harmless, CI regenerates)"
    else
        echo "release: warning — cargo not found; skipping Cargo.lock sync"
    fi

    run git add Cargo.toml Cargo.lock
    run git commit -m "chore(release): v${VERSION}"
fi

# --- Tag + push -------------------------------------------------------------

run git tag -a "${TAG}" -m "Release ${TAG}"
# Push the branch and the tag in ONE atomic transaction: either both refs land
# or neither does. Two separate pushes could leave the version-bump commit on
# origin without its tag (no release fires), and the leftover LOCAL tag would
# then block a clean re-run. `--atomic` needs a modern remote (GitHub supports it).
run git push --atomic origin "${BRANCH}" "${TAG}"

if [ "$DRY_RUN" -eq 1 ]; then
    echo "release: dry run complete — nothing was changed."
else
    echo "release: pushed ${TAG}. The Release workflow will build + publish it:"
    echo "  https://github.com/marshallku/copad/actions"
fi
