#!/usr/bin/env bash
# scripts/install-macos.sh — Build + install copad-macos as a real .app
# and install coctl via `cargo install --path copad-cli`.
#
# Companion to scripts/install-dev.sh (which is Linux-only — it does
# `cargo build --workspace`, and the workspace contains copad-linux which
# does not build on macOS without GTK4).
#
# Why this script exists:
#   - The macOS GUI app builds via SwiftPM in copad-macos/, not cargo.
#     Up to now, copad-macos/run.sh was the only path, and it builds an
#     ephemeral debug bundle under .build/debug/ and `open -n`s it. There
#     was no way to install copad as a real /Applications app.
#   - `cargo install copad-cli` (crates.io) fails — the package is not
#     published. `cargo install --path .` from the repo root also fails
#     because the root manifest is a workspace, not a package. The
#     correct invocation is `cargo install --path copad-cli`, which this
#     script wraps so the user does not need to memorize it.
#
# Usage:
#   ./scripts/install-macos.sh              # ~/Applications + ~/.cargo/bin (no sudo)
#   ./scripts/install-macos.sh --system     # /Applications + ~/.cargo/bin (sudo for /Applications)
#   ./scripts/install-macos.sh --no-build   # skip swift build (use existing .build/release/Copad)
#   ./scripts/install-macos.sh --no-coctl # skip cargo install of coctl
#   ./scripts/install-macos.sh --no-copadd # skip cargo install of copadd (daemon)
#   ./scripts/install-macos.sh --no-mux   # skip cargo install of comux (multiplexer)
#   ./scripts/install-macos.sh --no-plugins # skip building/installing plugin binaries
#   ./scripts/install-macos.sh --launch     # open the installed app afterwards
#
# Notes:
#   - coctl + copadd always go to ~/.cargo/bin (cargo install's default).
#     If you want them in /usr/local/bin, run `sudo install -m755 \\
#     ~/.cargo/bin/{coctl,copadd} /usr/local/bin/` after this script.
#   - This script kills any running Copad instance so the binary can be
#     replaced. macOS holds an exclusive lock on a running .app's exec.
#   - First launch may show Gatekeeper warning if the .app is unsigned;
#     right-click → Open once, or `xattr -d com.apple.quarantine` (only
#     applies to downloaded apps; locally-built bundles do not carry the
#     quarantine xattr).

set -euo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
    echo "this script is macOS-only; on Linux use scripts/install-dev.sh" >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="Copad.app"

# `cargo install` binaries carry a linker-signed ad-hoc signature that the macOS 26 Code
# Signing Monitor can reject at exec time (SIGKILL "Code Signature Invalid", seen on comux)
# even though `codesign -v` passes. A fresh ad-hoc signature avoids it.
resign_cli() {
    local bin="$1"
    [ -e "$bin" ] || return 0
    echo "==> codesign --force --sign - $bin"
    codesign --force --sign - "$bin"
}
DO_BUILD=true
SYSTEM_INSTALL=false
DO_COCTL=true
DO_COPADD=true
DO_MUX=true
DO_PLUGINS=true
DO_DAEMON=true
DO_LAUNCH=false

MACOS_PLUGINS=(echo git llm calendar kb docs todo pomodoro bookmark slack discord jira claude pilot web-bridge)

while [[ $# -gt 0 ]]; do
    case "$1" in
        --system)      SYSTEM_INSTALL=true ; shift ;;
        --no-build)    DO_BUILD=false ; shift ;;
        --no-coctl)  DO_COCTL=false ; shift ;;
        --no-copadd)  DO_COPADD=false ; shift ;;
        --no-mux)  DO_MUX=false ; shift ;;
        --no-plugins)  DO_PLUGINS=false ; shift ;;
        --no-daemon)   DO_DAEMON=false ; shift ;;
        --with-daemon) DO_DAEMON=true ; shift ;;
        --launch)      DO_LAUNCH=true ; shift ;;
        -h|--help)
            sed -n '2,/^set -euo/p' "$0" | grep -E '^# ' | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "unknown flag: $1" >&2
            exit 2
            ;;
    esac
done

if $SYSTEM_INSTALL; then
    APP_DEST="/Applications"
    SUDO_APP="sudo"
else
    APP_DEST="$HOME/Applications"
    SUDO_APP=""
fi

# SwiftPM cannot run cargo as a prebuild step, so build the Rust staticlibs first;
# Package.swift links them via -L../target/release.
if $DO_BUILD; then
    echo "==> cargo build --release -p copad-ffi -p copad-term (Rust staticlibs for Swift FFI)"
    (cd "$REPO_ROOT" && cargo build --release -p copad-ffi -p copad-term)

    echo "==> swift build -c release (copad-macos)"
    (cd "$REPO_ROOT/copad-macos" && swift build -c release)
fi

BUILT_BIN="$REPO_ROOT/copad-macos/.build/release/Copad"
if [[ ! -x "$BUILT_BIN" ]]; then
    echo "error: $BUILT_BIN not found — drop --no-build, or run swift build -c release in copad-macos/" >&2
    exit 1
fi

# Stop any running instance so the bundle's executable can be replaced.
pkill -x Copad 2>/dev/null || true
sleep 0.3

STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGING_DIR"' EXIT
STAGING="$STAGING_DIR/$APP_NAME"
CONTENTS="$STAGING/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
mkdir -p "$MACOS" "$RESOURCES"
cp "$BUILT_BIN" "$MACOS/Copad"

ICNS_SRC="$REPO_ROOT/copad-macos/Resources/AppIcon.icns"
if [[ -f "$ICNS_SRC" ]]; then
    cp "$ICNS_SRC" "$RESOURCES/AppIcon.icns"
else
    echo "warn: $ICNS_SRC missing — bundle will fall back to the generic app icon" >&2
fi

# Info.plist is kept in sync with copad-macos/run.sh by hand.
cat > "$CONTENTS/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>Copad</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.marshall.copad</string>
    <key>CFBundleName</key>
    <string>copad</string>
    <key>CFBundleDisplayName</key>
    <string>copad</string>
    <key>CFBundleVersion</key>
    <string>1.0.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
EOF

# Sign the staging bundle with the stable self-signed cert: swift's ad-hoc signature has a
# fresh cdhash per build, so TCC would re-prompt for every permission after each install.
"$REPO_ROOT/scripts/codesign-dev.sh" "$STAGING"

echo "==> installing $APP_NAME to $APP_DEST"
mkdir -p "$APP_DEST" 2>/dev/null || $SUDO_APP mkdir -p "$APP_DEST"
$SUDO_APP rm -rf "$APP_DEST/$APP_NAME"
$SUDO_APP mv "$STAGING" "$APP_DEST/$APP_NAME"

if $DO_COCTL; then
    echo "==> cargo install --path copad-cli (coctl → ~/.cargo/bin)"
    cargo install --path "$REPO_ROOT/copad-cli"
    resign_cli "$HOME/.cargo/bin/coctl"
fi
if $DO_COPADD; then
    echo "==> cargo install --path copad-daemon (copadd → ~/.cargo/bin)"
    cargo install --path "$REPO_ROOT/copad-daemon"
    resign_cli "$HOME/.cargo/bin/copadd"
    # Pin the path for the LaunchAgent wrapper (copadd-launch.sh) so it does not launch a
    # stale ~/.local/bin copy left by install.sh.
    mkdir -p "$HOME/.config/copad"
    printf '%s\n' "$HOME/.cargo/bin/copadd" > "$HOME/.config/copad/copadd.path"
fi
if $DO_MUX; then
    echo "==> cargo install --path copad-mux (comux → ~/.cargo/bin)"
    cargo install --path "$REPO_ROOT/copad-mux"
    resign_cli "$HOME/.cargo/bin/comux"
    # Drop the pre-rename copad-mux binary so an upgrade leaves only comux.
    [ -e "$HOME/.cargo/bin/copad-mux" ] && rm -f "$HOME/.cargo/bin/copad-mux"
fi

# Manifest `services.exec` resolves against the plugin dir first, so the binary goes next
# to plugin.toml and needs no PATH.
PLUGIN_DEST="$HOME/Library/Application Support/copad/plugins"
if $DO_PLUGINS; then
    mkdir -p "$PLUGIN_DEST"
    for name in "${MACOS_PLUGINS[@]}"; do
        crate="copad-plugin-$name"
        src_manifest="$REPO_ROOT/plugins/$name/plugin.toml"
        if [[ ! -f "$src_manifest" ]]; then
            echo "skip plugin $name: $src_manifest missing"
            continue
        fi
        echo "==> cargo build --release -p $crate"
        (cd "$REPO_ROOT" && cargo build --release -p "$crate")

        bin_src="$REPO_ROOT/target/release/$crate"
        if [[ ! -x "$bin_src" ]]; then
            echo "warn  plugin $name: binary $bin_src not built — skipping" >&2
            continue
        fi

        plugin_dir="$PLUGIN_DEST/$name"
        mkdir -p "$plugin_dir"
        find "$REPO_ROOT/plugins/$name" -maxdepth 1 -type f ! -name 'Cargo.toml' \
            -exec cp -f {} "$plugin_dir/" \;
        # Copy, not symlink, so a `git clean` of target/ doesn't silently break the install.
        cp -f "$bin_src" "$plugin_dir/$crate"
        chmod 755 "$plugin_dir/$crate"
        echo "ok    plugin $name → $plugin_dir/"
    done
fi

# launchd does not expand `~`, so the HOME_PLACEHOLDER tokens in the plist template are
# rewritten; `bootout` first keeps re-runs idempotent.
if $DO_DAEMON; then
    if command -v launchctl >/dev/null 2>&1 && $DO_COPADD; then
        PLIST_SRC="$REPO_ROOT/dist/launchd/com.marshall.copad.daemon.plist"
        PLIST_DST="$HOME/Library/LaunchAgents/com.marshall.copad.daemon.plist"
        if [[ ! -f "$PLIST_SRC" ]]; then
            echo "warn: $PLIST_SRC missing — skipping LaunchAgent install" >&2
        else
            echo "==> installing LaunchAgent plist into $PLIST_DST"
            mkdir -p "$(dirname "$PLIST_DST")"

            # The wrapper sources secrets.env (mode 600) before exec'ing copadd, keeping
            # plugin tokens out of the 644 plist. The token is minted once so a paired phone
            # survives upgrades.
            WRAP_SRC="$REPO_ROOT/dist/launchd/copadd-launch.sh"
            WRAP_DST="$HOME/.config/copad/copadd-launch.sh"
            SECRETS_ENV="$HOME/.config/copad/secrets.env"
            mkdir -p "$HOME/.config/copad"
            if [[ -f "$WRAP_SRC" ]]; then
                cp -f "$WRAP_SRC" "$WRAP_DST"
                chmod 755 "$WRAP_DST"
            else
                echo "warn: $WRAP_SRC missing — copadd will run without secrets sourcing" >&2
            fi
            if [[ ! -f "$SECRETS_ENV" ]]; then
                # Validate before writing: a failed openssl must not leave an empty token file
                # that later reinstalls would preserve forever.
                WB_TOKEN=$(openssl rand -hex 24 2>/dev/null || true)
                if [[ -z "$WB_TOKEN" ]]; then
                    echo "warn: openssl rand failed — no web-bridge token minted; add COPAD_WEB_BRIDGE_TOKEN=<≥32 chars> to $SECRETS_ENV manually" >&2
                else
                    ( umask 077; printf 'COPAD_WEB_BRIDGE_TOKEN=%s\n' "$WB_TOKEN" > "$SECRETS_ENV" )
                    chmod 600 "$SECRETS_ENV"
                    echo "    minted web-bridge token → $SECRETS_ENV (600). View with: cat $SECRETS_ENV"
                fi
            else
                # Re-assert 600: a pre-existing looser file would keep the token exposed.
                chmod 600 "$SECRETS_ENV"
                echo "    preserving existing $SECRETS_ENV (web-bridge token kept; perms reasserted 600)"
            fi
            # Bash 5.3+ enables patsub_replacement, which expands `&` in the replacement and
            # breaks both the XML escapes and the HOME_PLACEHOLDER substitution; disable it
            # first. Escape `&` before `<` and `>` so the injected `&amp;` is not re-escaped.
            shopt -u patsub_replacement 2>/dev/null || true
            HOME_ESC=$HOME
            HOME_ESC=${HOME_ESC//&/&amp;}
            HOME_ESC=${HOME_ESC//</&lt;}
            HOME_ESC=${HOME_ESC//>/&gt;}
            PLIST_TEXT=$(cat "$PLIST_SRC")
            PLIST_TEXT=${PLIST_TEXT//HOME_PLACEHOLDER/$HOME_ESC}
            printf '%s\n' "$PLIST_TEXT" > "$PLIST_DST"
            chmod 644 "$PLIST_DST"
            launchctl bootout "gui/$UID/com.marshall.copad.daemon" 2>/dev/null || true
            launchctl bootstrap "gui/$UID" "$PLIST_DST"
            echo "    launchctl list | grep copad       # to inspect"
            echo "    tail ~/Library/Logs/copad-daemon.{out,err}.log  # logs"
        fi
    else
        if ! command -v launchctl >/dev/null 2>&1; then
            echo "warn: launchctl not on PATH; skipping LaunchAgent install."
        elif ! $DO_COPADD; then
            echo "note: skipping LaunchAgent install because --no-copadd was passed."
        fi
    fi
fi

# The alacritty backend cannot capture OSC 7 and proc_pidinfo is EPERM under hardened
# runtime, so an in-shell hook calls `coctl call panel.report_cwd` on every chpwd.
SHELL_HOOK_SRC="$REPO_ROOT/copad-macos/shell-hooks"
SHELL_HOOK_DEST="$HOME/.config/copad/shell-hooks"
if [[ -d "$SHELL_HOOK_SRC" ]]; then
    mkdir -p "$SHELL_HOOK_DEST"
    for f in "$SHELL_HOOK_SRC"/copad-cwd.*; do
        [[ -f "$f" ]] || continue
        cp -f "$f" "$SHELL_HOOK_DEST/$(basename "$f")"
    done
fi

# Context bridge (zsh): same script Linux uses; a no-op outside copad PTY children.
CONTEXT_HOOK_SRC="$REPO_ROOT/examples/shell/copad-context.zsh"
if [[ -f "$CONTEXT_HOOK_SRC" ]]; then
    mkdir -p "$SHELL_HOOK_DEST"
    cp -f "$CONTEXT_HOOK_SRC" "$SHELL_HOOK_DEST/copad-context.zsh"
fi

if $DO_LAUNCH; then
    open "$APP_DEST/$APP_NAME"
fi

cat <<EOF

Installed:
  $APP_DEST/$APP_NAME
EOF
if $DO_COCTL; then
    echo "  $HOME/.cargo/bin/coctl"
fi
if $DO_COPADD; then
    echo "  $HOME/.cargo/bin/copadd"
fi
if $DO_MUX; then
    echo "  $HOME/.cargo/bin/comux"
fi
if $DO_PLUGINS; then
    echo "  $PLUGIN_DEST/{$(IFS=,; echo "${MACOS_PLUGINS[*]}")}"
fi
cat <<'EOF'

Next:
  - Launch the GUI: `open -a Copad` (or Spotlight / Launchpad).
  - CLI helpers on the app binary itself:
      Copad.app/Contents/MacOS/Copad --version
      Copad.app/Contents/MacOS/Copad --config-path
      Copad.app/Contents/MacOS/Copad --init-config   # writes ~/.config/copad/config.toml if missing
    (Many users alias `copad` to that binary so `copad --config-path` works.)
  - Verify a plugin is alive: `coctl call echo.ping --params '{"hi":"there"}'`
  - Tail recent daemon events:    `coctl recent`
  - Live-cwd tracking (so session restore lands at your current dir,
    not the spawn-time one). macOS hardened runtime blocks the
    proc_pidinfo path Linux/VTE gets for free; the workaround is a
    shell hook installed under ~/.config/copad/shell-hooks/. Add ONE
    of these lines to your shell rc file:
        zsh   ~/.zshrc       source ~/.config/copad/shell-hooks/copad-cwd.zsh
        bash  ~/.bashrc      source ~/.config/copad/shell-hooks/copad-cwd.bash
        fish  ~/.config/fish/config.fish
                             source ~/.config/copad/shell-hooks/copad-cwd.fish
    No-op when the shell isn't running inside a copad PTY, so it's
    safe to source unconditionally.
  - Optional: Phase 22.1 context bridge (publishes pane.context_changed
    every prompt redraw with host / cwd / git_remote / branch / tmux —
    eventual input for the dossier panel). zsh-only in v1:
        zsh   ~/.zshrc       source ~/.config/copad/shell-hooks/copad-context.zsh
    Same no-op-outside-copad guarantee.
EOF
