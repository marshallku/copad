#!/bin/bash
# End-user install script: downloads copad + coctl from the
# latest GitHub Release tag and lays them out at ~/.local/bin (or
# /usr/local/bin with --system).
#
# For LOCAL DEVELOPMENT iteration on the working tree, use
# scripts/install-dev.sh instead — that one builds from source
# AND warns (loudly, in stderr) when ~/.local/bin/copad and
# /usr/local/bin/copad differ, so a stale system binary silently
# shadowing a working-tree fix at least becomes visible. (It
# can't auto-resolve the drift; remediation is one of the
# documented sudo rm or overwrite options the warning prints.)
#
# Plugin binaries (copad-plugin-*) are NOT in the release tarball
# yet; if you want plugins, install them separately via
# scripts/install-plugins.sh after running install-dev.sh, OR
# build from source.
set -euo pipefail

REPO="marshallku/copad"
INSTALL_DIR="${HOME}/.local/bin"
DESKTOP_DIR="${HOME}/.local/share/applications"
ICON_BASE="${HOME}/.local/share/icons/hicolor"
TARGET_VERSION=""
SYSTEM_INSTALL=false
ICON_SIZES=(16 22 24 32 48 64 128 256 512)

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Install copad from GitHub Releases."
    echo ""
    echo "Options:"
    echo "  --version VERSION    Install a specific version (e.g., v0.1.0)"
    echo "  --system             Install to /usr/local/bin (requires sudo)"
    echo "  -h, --help           Show this help message"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            TARGET_VERSION="$2"
            shift 2
            ;;
        --system)
            SYSTEM_INSTALL=true
            INSTALL_DIR="/usr/local/bin"
            DESKTOP_DIR="/usr/share/applications"
            ICON_BASE="/usr/share/icons/hicolor"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

ARCH="$(uname -m)"
if [[ "${ARCH}" != "x86_64" ]]; then
    echo "Error: unsupported architecture '${ARCH}'. Only x86_64 is supported."
    exit 1
fi

check_deps() {
    local missing=()
    pkg-config --exists gtk4 2>/dev/null || missing+=("gtk4")
    pkg-config --exists vte-2.91-gtk4 2>/dev/null || missing+=("vte4 (libvte-2.91-gtk4)")
    pkg-config --exists webkitgtk-6.0 2>/dev/null || missing+=("webkitgtk-6.0")
    pkg-config --exists gstreamer-1.0 2>/dev/null || missing+=("gst-plugins-good gst-plugins-bad")
    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "Warning: missing system dependencies: ${missing[*]}"
        echo "copad requires these libraries to run. Install them via your package manager."
    fi
}

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

echo "Installing copad ${VERSION}..."

ASSET_NAME="copad-${VERSION}-x86_64-linux.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET_NAME}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

echo "Downloading ${ASSET_NAME}..."
curl -fsSL -o "${TMPDIR}/copad.tar.gz" "${DOWNLOAD_URL}"

echo "Extracting..."
tar -xzf "${TMPDIR}/copad.tar.gz" -C "${TMPDIR}"

# Pre-0.2 release tarballs shipped a "copad.desktop"; v0.2+ ship
# "com.marshall.copad.desktop" so the basename matches the app_id
# and Wayland compositors associate windows with the launcher. Detect
# whichever the tarball carries so this installer is forward- and
# backward-compatible.
DESKTOP_SRC=""
DESKTOP_DEST_NAME=""
for candidate in "com.marshall.copad.desktop" "copad.desktop"; do
    if [[ -f "${TMPDIR}/${candidate}" ]]; then
        DESKTOP_SRC="${TMPDIR}/${candidate}"
        DESKTOP_DEST_NAME="$candidate"
        break
    fi
done

if ${SYSTEM_INSTALL}; then
    echo "Installing to ${INSTALL_DIR} (requires sudo)..."
    sudo install -Dm755 "${TMPDIR}/copad" "${INSTALL_DIR}/copad"
    sudo install -Dm755 "${TMPDIR}/coctl" "${INSTALL_DIR}/coctl"
    if [[ -n "$DESKTOP_SRC" ]]; then
        sudo install -Dm644 "$DESKTOP_SRC" "${DESKTOP_DIR}/${DESKTOP_DEST_NAME}"
        # Drop the pre-rename copy if both are about to coexist.
        if [[ "$DESKTOP_DEST_NAME" = "com.marshall.copad.desktop" ]]; then
            sudo rm -f "${DESKTOP_DIR}/copad.desktop"
        fi
    fi
    for size in "${ICON_SIZES[@]}"; do
        src="${TMPDIR}/icons/hicolor/${size}x${size}/apps/copad.png"
        [[ -f "$src" ]] || continue
        sudo install -Dm644 "$src" "${ICON_BASE}/${size}x${size}/apps/copad.png"
    done
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        sudo gtk-update-icon-cache -q -t "${ICON_BASE}" || true
    fi
else
    mkdir -p "${INSTALL_DIR}" "${DESKTOP_DIR}"
    install -m755 "${TMPDIR}/copad" "${INSTALL_DIR}/copad"
    install -m755 "${TMPDIR}/coctl" "${INSTALL_DIR}/coctl"
    if [[ -n "$DESKTOP_SRC" ]]; then
        install -Dm644 "$DESKTOP_SRC" "${DESKTOP_DIR}/${DESKTOP_DEST_NAME}"
        if [[ "$DESKTOP_DEST_NAME" = "com.marshall.copad.desktop" ]]; then
            rm -f "${DESKTOP_DIR}/copad.desktop"
        fi
    fi
    for size in "${ICON_SIZES[@]}"; do
        src="${TMPDIR}/icons/hicolor/${size}x${size}/apps/copad.png"
        [[ -f "$src" ]] || continue
        install -Dm644 "$src" "${ICON_BASE}/${size}x${size}/apps/copad.png"
    done
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t "${ICON_BASE}" || true
    fi
fi

check_deps

if ! echo "${PATH}" | tr ':' '\n' | grep -qx "${INSTALL_DIR}"; then
    echo ""
    echo "Warning: ${INSTALL_DIR} is not in your PATH."
    echo "Add it to your shell profile:"
    echo "  export PATH=\"${INSTALL_DIR}:\${PATH}\""
fi

echo ""
echo "copad ${VERSION} installed successfully!"
echo "  copad    -> ${INSTALL_DIR}/copad"
echo "  coctl -> ${INSTALL_DIR}/coctl"
