#!/bin/bash
# End-user install script: downloads copad and its supporting binaries
# (coctl, copadd, comux) plus the first-party plugins from the latest
# GitHub Release and lays them out. Cross-platform — one `curl | bash`
# entry point works on both:
#
#   Linux  (x86_64) : GTK app `copad` + `coctl` (+ `comux`) → ~/.local/bin
#                     (or /usr/local/bin with --system), plus the desktop
#                     entry and hicolor icons. Plugins are NOT in the Linux
#                     tarball yet — build from source for those.
#   macOS  (arm64)  : `Copad.app` → ~/Applications (or /Applications with
#                     --system), `coctl`/`copadd`/`comux` → ~/.local/bin
#                     (or /usr/local/bin), the first-party plugins →
#                     ~/Library/Application Support/copad/plugins, the
#                     live-cwd shell hooks, and — best effort — the copadd
#                     LaunchAgent so the daemon starts at login.
#
# For LOCAL DEVELOPMENT iteration on the working tree, use
# scripts/install-dev.sh (Linux) or scripts/install-macos.sh (macOS)
# instead — those build from source. This script only ever downloads
# prebuilt release artifacts.
set -euo pipefail

REPO="marshallku/copad"
TARGET_VERSION=""
SYSTEM_INSTALL=false

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Install copad from GitHub Releases (Linux x86_64 or macOS arm64)."
    echo ""
    echo "Options:"
    echo "  --version VERSION    Install a specific version (e.g., v1.0.0)"
    echo "  --system             Install system-wide (/usr/local/bin, /Applications; requires sudo)"
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

OS="$(uname -s)"
ARCH="$(uname -m)"

# ---- Shared: resolve the release tag + prepare a scratch dir --------------
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

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

# download_asset <asset-name> — fetch a release asset into $TMPDIR and
# extract it there. Leaves the extracted tree directly under $TMPDIR.
download_asset() {
    local asset="$1"
    local url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
    echo "Downloading ${asset}..."
    curl -fsSL -o "${TMPDIR}/${asset}" "${url}"
    echo "Extracting..."
    tar -xzf "${TMPDIR}/${asset}" -C "${TMPDIR}"
}

# ===========================================================================
# Linux
# ===========================================================================
install_linux() {
    if [[ "${ARCH}" != "x86_64" ]]; then
        echo "Error: unsupported Linux architecture '${ARCH}'. Only x86_64 is supported."
        exit 1
    fi

    local INSTALL_DIR DESKTOP_DIR ICON_BASE
    if ${SYSTEM_INSTALL}; then
        INSTALL_DIR="/usr/local/bin"
        DESKTOP_DIR="/usr/share/applications"
        ICON_BASE="/usr/share/icons/hicolor"
    else
        INSTALL_DIR="${HOME}/.local/bin"
        DESKTOP_DIR="${HOME}/.local/share/applications"
        ICON_BASE="${HOME}/.local/share/icons/hicolor"
    fi
    local ICON_SIZES=(16 22 24 32 48 64 128 256 512)

    echo "Installing copad ${VERSION} (Linux x86_64)..."
    download_asset "copad-${VERSION}-x86_64-linux.tar.gz"

    # Pre-0.2 release tarballs shipped a "copad.desktop"; v0.2+ ship
    # "com.marshall.copad.desktop" so the basename matches the app_id
    # and Wayland compositors associate windows with the launcher. Detect
    # whichever the tarball carries so this installer is forward- and
    # backward-compatible.
    local DESKTOP_SRC="" DESKTOP_DEST_NAME=""
    for candidate in "com.marshall.copad.desktop" "copad.desktop"; do
        if [[ -f "${TMPDIR}/${candidate}" ]]; then
            DESKTOP_SRC="${TMPDIR}/${candidate}"
            DESKTOP_DEST_NAME="$candidate"
            break
        fi
    done

    local SUDO=""
    if ${SYSTEM_INSTALL}; then
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        SUDO="sudo"
    else
        mkdir -p "${INSTALL_DIR}" "${DESKTOP_DIR}"
    fi

    ${SUDO} install -Dm755 "${TMPDIR}/copad" "${INSTALL_DIR}/copad"
    ${SUDO} install -Dm755 "${TMPDIR}/coctl" "${INSTALL_DIR}/coctl"
    # comux (the terminal multiplexer) ships in v0.2+ tarballs; guard so
    # older releases without it still install cleanly.
    [[ -f "${TMPDIR}/comux" ]] && ${SUDO} install -Dm755 "${TMPDIR}/comux" "${INSTALL_DIR}/comux"
    # Drop the pre-rename copad-mux binary so an upgrade leaves only comux.
    [[ -e "${INSTALL_DIR}/copad-mux" ]] && ${SUDO} rm -f "${INSTALL_DIR}/copad-mux"

    if [[ -n "$DESKTOP_SRC" ]]; then
        ${SUDO} install -Dm644 "$DESKTOP_SRC" "${DESKTOP_DIR}/${DESKTOP_DEST_NAME}"
        # Drop the pre-rename copy if both are about to coexist.
        if [[ "$DESKTOP_DEST_NAME" = "com.marshall.copad.desktop" ]]; then
            ${SUDO} rm -f "${DESKTOP_DIR}/copad.desktop"
        fi
    fi
    for size in "${ICON_SIZES[@]}"; do
        local src="${TMPDIR}/icons/hicolor/${size}x${size}/apps/copad.png"
        [[ -f "$src" ]] || continue
        ${SUDO} install -Dm644 "$src" "${ICON_BASE}/${size}x${size}/apps/copad.png"
    done
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        ${SUDO} gtk-update-icon-cache -q -t "${ICON_BASE}" || true
    fi

    # Warn (don't fail) on missing GTK/VTE runtime libraries — copad needs
    # them at runtime but they are the user's package manager's job.
    local missing=()
    pkg-config --exists gtk4 2>/dev/null || missing+=("gtk4")
    pkg-config --exists vte-2.91-gtk4 2>/dev/null || missing+=("vte4 (libvte-2.91-gtk4)")
    pkg-config --exists webkitgtk-6.0 2>/dev/null || missing+=("webkitgtk-6.0")
    pkg-config --exists gstreamer-1.0 2>/dev/null || missing+=("gst-plugins-good gst-plugins-bad")
    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "Warning: missing system dependencies: ${missing[*]}"
        echo "copad requires these libraries to run. Install them via your package manager."
    fi

    warn_path "${INSTALL_DIR}"

    echo ""
    echo "copad ${VERSION} installed successfully!"
    echo "  copad -> ${INSTALL_DIR}/copad"
    echo "  coctl -> ${INSTALL_DIR}/coctl"
    # Report from the tarball contents (what THIS run installed), not the
    # destination — an old release over an existing mux shouldn't claim it.
    [[ -f "${TMPDIR}/comux" ]] && echo "  comux -> ${INSTALL_DIR}/comux"
}

# ===========================================================================
# macOS
# ===========================================================================
install_macos() {
    if [[ "${ARCH}" != "arm64" ]]; then
        echo "Error: unsupported macOS architecture '${ARCH}'. Release builds are"
        echo "Apple Silicon (arm64) only. On an Intel Mac, build from source with"
        echo "scripts/install-macos.sh."
        exit 1
    fi

    local APP_DEST BIN_DIR SUDO_APP
    if ${SYSTEM_INSTALL}; then
        APP_DEST="/Applications"
        BIN_DIR="/usr/local/bin"
        SUDO_APP="sudo"
    else
        APP_DEST="${HOME}/Applications"
        BIN_DIR="${HOME}/.local/bin"
        SUDO_APP=""
    fi
    local PLUGIN_DEST="${HOME}/Library/Application Support/copad/plugins"
    local SHELL_HOOK_DEST="${HOME}/.config/copad/shell-hooks"

    echo "Installing copad ${VERSION} (macOS arm64)..."
    download_asset "copad-${VERSION}-aarch64-apple-darwin.tar.gz"

    if [[ ! -d "${TMPDIR}/Copad.app" ]]; then
        echo "Error: release tarball did not contain Copad.app." >&2
        exit 1
    fi

    # Stop any running instance so we can replace the bundle's executable —
    # macOS holds an exclusive lock on a running .app's exec.
    pkill -x Copad 2>/dev/null || true
    sleep 0.3

    # 1. Install Copad.app. `ditto` preserves bundle structure/xattrs and is
    #    the Apple-blessed way to copy an .app. Replace atomically-ish: the
    #    running instance was just killed above.
    echo "==> installing Copad.app to ${APP_DEST}"
    ${SUDO_APP} mkdir -p "${APP_DEST}"
    ${SUDO_APP} rm -rf "${APP_DEST}/Copad.app"
    ${SUDO_APP} ditto "${TMPDIR}/Copad.app" "${APP_DEST}/Copad.app"
    # The bundle is ad-hoc signed (not notarized). Strip the quarantine
    # xattr the download carried so Gatekeeper lets it launch without the
    # right-click → Open dance. Only affects this locally-installed copy.
    ${SUDO_APP} xattr -dr com.apple.quarantine "${APP_DEST}/Copad.app" 2>/dev/null || true

    # 2. CLI binaries → BIN_DIR.
    echo "==> installing coctl / copadd / comux to ${BIN_DIR}"
    ${SUDO_APP} mkdir -p "${BIN_DIR}"
    for b in coctl copadd comux; do
        [[ -f "${TMPDIR}/${b}" ]] && ${SUDO_APP} install -m755 "${TMPDIR}/${b}" "${BIN_DIR}/${b}"
    done
    # Drop the pre-rename copad-mux binary so an upgrade leaves only comux.
    [[ -e "${BIN_DIR}/copad-mux" ]] && ${SUDO_APP} rm -f "${BIN_DIR}/copad-mux"

    # Pin the copadd path this install chose so the LaunchAgent wrapper launches
    # THIS binary, not a stale copy a prior --user/--system install left behind.
    if [[ -f "${TMPDIR}/copadd" ]]; then
        mkdir -p "${HOME}/.config/copad"
        printf '%s\n' "${BIN_DIR}/copadd" > "${HOME}/.config/copad/copadd.path"
    fi

    # 3. Plugins → ~/Library/Application Support/copad/plugins/<name>/.
    #    PluginSupervisor reads this at startup; each dir holds the binary
    #    co-located with its plugin.toml so it wins over $PATH lookup.
    if [[ -d "${TMPDIR}/plugins" ]]; then
        echo "==> installing plugins to ${PLUGIN_DEST}"
        mkdir -p "${PLUGIN_DEST}"
        # Copy each plugin dir individually so an existing install is
        # refreshed in place rather than nested under a stray subdir.
        for pdir in "${TMPDIR}/plugins/"*/; do
            [[ -d "$pdir" ]] || continue
            local name; name="$(basename "$pdir")"
            rm -rf "${PLUGIN_DEST}/${name}"
            cp -R "$pdir" "${PLUGIN_DEST}/${name}"
        done
    fi

    # 4. Live-cwd shell hooks (opt-in; user sources one from their rc file).
    if [[ -d "${TMPDIR}/shell-hooks" ]]; then
        mkdir -p "${SHELL_HOOK_DEST}"
        cp -f "${TMPDIR}/shell-hooks/"* "${SHELL_HOOK_DEST}/" 2>/dev/null || true
    fi

    # 5. copadd LaunchAgent (best effort). The Swift app auto-spawns copadd
    #    on launch, so this is only needed for the daemon to run at login
    #    without the app open (background agent-turn notifications). Requires
    #    the copadd-launch.sh wrapper the tarball ships alongside the plist.
    #    Run in a subshell so a mid-setup failure under `set -e` (an
    #    unwritable dir, a bad plist write) is contained there and NEVER
    #    aborts the core install that already succeeded above.
    ( install_macos_launchagent ) || \
        echo "note: copadd LaunchAgent setup did not complete (non-fatal) — the app still auto-starts copadd when open."

    warn_path "${BIN_DIR}"

    echo ""
    echo "copad ${VERSION} installed successfully!"
    echo "  Copad.app -> ${APP_DEST}/Copad.app   (launch: open -a Copad)"
    for b in coctl copadd comux; do
        [[ -f "${TMPDIR}/${b}" ]] && echo "  ${b} -> ${BIN_DIR}/${b}"
    done
    [[ -d "${TMPDIR}/plugins" ]] && echo "  plugins -> ${PLUGIN_DEST}/"
}

# LaunchAgent setup for copadd on macOS. Best effort: any failure prints a
# note and returns 0 so the overall install still succeeds. Mirrors the
# proven logic in scripts/install-macos.sh (HOME_PLACEHOLDER rewrite +
# secrets.env minting), adapted to read the plist/wrapper from the tarball.
install_macos_launchagent() {
    local plist_src="${TMPDIR}/com.marshall.copad.daemon.plist"
    local wrap_src="${TMPDIR}/copadd-launch.sh"

    if ! command -v launchctl >/dev/null 2>&1; then
        echo "note: launchctl not found — skipping login daemon; the app auto-starts copadd when open."
        return 0
    fi
    if [[ ! -f "$plist_src" || ! -f "$wrap_src" ]]; then
        echo "note: this release does not ship the daemon LaunchAgent; the app auto-starts copadd when open."
        return 0
    fi

    local wrap_dst="${HOME}/.config/copad/copadd-launch.sh"
    local secrets_env="${HOME}/.config/copad/secrets.env"
    local plist_dst="${HOME}/Library/LaunchAgents/com.marshall.copad.daemon.plist"

    echo "==> installing copadd LaunchAgent (login daemon)"
    mkdir -p "${HOME}/.config/copad" "${HOME}/Library/LaunchAgents"
    cp -f "$wrap_src" "$wrap_dst"
    chmod 755 "$wrap_dst"

    # Mint the web-bridge bearer token once (600), preserved across
    # reinstalls so a paired phone keeps working after an upgrade.
    if [[ ! -f "$secrets_env" ]]; then
        local wb_token
        wb_token="$(openssl rand -hex 24 2>/dev/null || true)"
        if [[ -n "$wb_token" ]]; then
            ( umask 077; printf 'COPAD_WEB_BRIDGE_TOKEN=%s\n' "$wb_token" > "$secrets_env" )
            chmod 600 "$secrets_env"
        fi
    else
        chmod 600 "$secrets_env" 2>/dev/null || true
    fi

    # XML-escape $HOME then substitute HOME_PLACEHOLDER (launchd does not
    # expand ~). Disable bash 5.3+ patsub_replacement first so `&` in the
    # replacement text is treated literally, not as the matched substring.
    shopt -u patsub_replacement 2>/dev/null || true
    local home_esc="$HOME"
    home_esc=${home_esc//&/&amp;}
    home_esc=${home_esc//</&lt;}
    home_esc=${home_esc//>/&gt;}
    local plist_text; plist_text="$(cat "$plist_src")"
    plist_text=${plist_text//HOME_PLACEHOLDER/$home_esc}
    printf '%s\n' "$plist_text" > "$plist_dst"
    chmod 644 "$plist_dst"

    launchctl bootout "gui/$UID/com.marshall.copad.daemon" 2>/dev/null || true
    if launchctl bootstrap "gui/$UID" "$plist_dst" 2>/dev/null; then
        echo "    copadd will start at login (logs: ~/Library/Logs/copad-daemon.{out,err}.log)"
    else
        echo "note: launchctl bootstrap failed — the app still auto-starts copadd when open."
    fi
}

# warn_path <dir> — remind the user to add <dir> to PATH if it is not there.
warn_path() {
    local dir="$1"
    if ! echo "${PATH}" | tr ':' '\n' | grep -qx "${dir}"; then
        echo ""
        echo "Warning: ${dir} is not in your PATH."
        echo "Add it to your shell profile:"
        echo "  export PATH=\"${dir}:\${PATH}\""
    fi
}

case "${OS}" in
    Linux)  install_linux ;;
    Darwin) install_macos ;;
    *)
        echo "Error: unsupported OS '${OS}'. copad ships Linux and macOS builds."
        exit 1
        ;;
esac
