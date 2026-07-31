#!/bin/bash
# Standalone installer for `comux` — copad's terminal multiplexer — on its own,
# WITHOUT the copad GUI app, the copadd daemon, or the plugins.
#
# comux is a self-contained single binary (pure Rust, no GTK / no daemon): a
# tmux-style server/client multiplexer with splits, tabs, sessions, worktrees,
# a sidebar, agent-status, and session persistence. It is distributed as its
# own release asset so users who only want the multiplexer don't download the
# whole terminal app. For the full copad, use install.sh instead.
#
# Optional companion: the status-bar usage/limits readout shells out to `coctl
# usage --limits --json`. Without coctl on PATH (or installed next to comux)
# that gauge simply doesn't render — everything else works. Install coctl via
# install.sh, or set `usage = off` in ~/.config/copad/mux.toml.
set -euo pipefail

REPO="marshallku/copad"
TARGET_VERSION=""
SYSTEM_INSTALL=false
FORCE_MUSL=false

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Install just comux (copad's terminal multiplexer) from GitHub Releases."
    echo ""
    echo "Options:"
    echo "  --version VERSION    Install a specific version (e.g., v1.0.0)"
    echo "  --system             Install to /usr/local/bin (requires sudo)"
    echo "  --musl               Force the fully-static musl build (glibc-free)."
    echo "                       Auto-selected on musl-libc systems (Alpine); use"
    echo "                       this to force it on a glibc host too old to run"
    echo "                       the default build. (Linux x86_64 only.)"
    echo "  -h, --help           Show this help message"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) TARGET_VERSION="$2"; shift 2 ;;
        --system)  SYSTEM_INSTALL=true; shift ;;
        --musl)    FORCE_MUSL=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1"; usage; exit 1 ;;
    esac
done

# True when the running system's C library is musl (Alpine et al.), where the
# default glibc-linked asset would not load at all. `ldd --version` prints "musl"
# to stderr on musl systems; the loader-file check is a fallback for hosts where
# ldd is absent or gated.
is_musl_libc() {
    if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        return 0
    fi
    [[ -e /lib/ld-musl-x86_64.so.1 ]]
}

# Map the running platform to the release asset name (raw binary per target).
OS="$(uname -s)"
ARCH="$(uname -m)"
case "${OS}:${ARCH}" in
    Linux:x86_64)
        # Fully-static musl asset on musl-libc hosts (where the glibc build can't
        # run) or when the user forces it (glibc too old for the default build).
        if ${FORCE_MUSL} || is_musl_libc; then
            TARGET="x86_64-linux-musl"
        else
            TARGET="x86_64-linux"
        fi
        ;;
    Darwin:arm64)
        if ${FORCE_MUSL}; then
            echo "Error: --musl is Linux x86_64 only; macOS has no musl build."
            exit 1
        fi
        TARGET="aarch64-apple-darwin"
        ;;
    *)
        echo "Error: unsupported platform '${OS} ${ARCH}'. comux ships x86_64 Linux and"
        echo "arm64 macOS builds. On another platform, build from source: cargo build"
        echo "--release -p copad-mux."
        exit 1
        ;;
esac

if ${SYSTEM_INSTALL}; then
    BIN_DIR="/usr/local/bin"
    SUDO="sudo"
else
    BIN_DIR="${HOME}/.local/bin"
    SUDO=""
fi

# Resolve the release tag.
if [[ -n "${TARGET_VERSION}" ]]; then
    VERSION="${TARGET_VERSION}"
    API_URL="https://api.github.com/repos/${REPO}/releases/tags/${VERSION}"
else
    API_URL="https://api.github.com/repos/${REPO}/releases/latest"
fi

echo "Fetching release info..."
RELEASE_JSON="$(curl -fsSL "${API_URL}")"
VERSION="$(echo "${RELEASE_JSON}" | grep -m1 '"tag_name"' | cut -d'"' -f4)"
if [[ -z "${VERSION}" ]]; then
    echo "Error: could not determine release version."
    exit 1
fi

ASSET="comux-${VERSION}-${TARGET}"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

echo "Installing comux ${VERSION} (${TARGET})..."
echo "Downloading ${ASSET}..."
# Fall back with a clear message if the release predates the standalone asset.
if ! curl -fsSL -o "${TMPDIR}/comux" "${URL}"; then
    echo "Error: ${ASSET} not found in release ${VERSION}."
    if [[ "${TARGET}" == *-musl ]]; then
        echo "The static (musl) comux asset is only published by newer releases. If this"
        echo "release predates it, pass a newer --version, or build from source on the"
        echo "target machine: cargo build --release --target x86_64-unknown-linux-musl -p copad-mux."
    else
        echo "Releases before v1.0.1 do not ship a standalone comux binary — comux is"
        echo "bundled in the full tarball there. Use install.sh, or pass a newer"
        echo "--version."
    fi
    exit 1
fi

${SUDO} mkdir -p "${BIN_DIR}"
${SUDO} install -m755 "${TMPDIR}/comux" "${BIN_DIR}/comux"
# Drop the pre-rename copad-mux binary so an upgrade leaves only comux.
[[ -e "${BIN_DIR}/copad-mux" ]] && ${SUDO} rm -f "${BIN_DIR}/copad-mux"

if ! echo "${PATH}" | tr ':' '\n' | grep -qx "${BIN_DIR}"; then
    echo ""
    echo "Warning: ${BIN_DIR} is not in your PATH."
    echo "Add it to your shell profile:"
    echo "  export PATH=\"${BIN_DIR}:\${PATH}\""
fi

echo ""
echo "comux ${VERSION} installed -> ${BIN_DIR}/comux"
echo "Run 'comux' to start (spawns the server on first use); 'comux help' for commands."
