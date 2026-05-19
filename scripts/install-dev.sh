#!/usr/bin/env bash
# Build + install the local working-tree nestty binaries + plugins
# in one shot. Companion to `install.sh` (which downloads from
# GitHub Releases for end users); this is the dev-iteration path
# for working on nestty itself.
#
# Why this exists: `install.sh --system` puts nestty at
# `/usr/local/bin/nestty`. After that, `cargo build --release` only
# refreshes `target/release/nestty` — `/usr/local/bin/nestty` stays at
# whatever version was last installed via Releases. That's how a
# stale system binary silently shadowed a freshly-built fix and
# wasted real debugging time. Run THIS script after every
# meaningful change so the GUI nestty and CLI nestctl on PATH stay
# in sync with the working tree.
#
# Usage:
#   ./scripts/install-dev.sh                # build + install everything to ~/.local/bin (no sudo)
#   ./scripts/install-dev.sh --system       # install to /usr/local/bin (requires sudo)
#   ./scripts/install-dev.sh --no-build     # skip cargo build (use existing target/release)
#   ./scripts/install-dev.sh --no-plugins   # skip the plugin install step
#   ./scripts/install-dev.sh --no-daemon    # skip the systemd --user unit install
#   ./scripts/install-dev.sh --restart      # also `pkill -x nestty` afterwards
#
# By default this is a USER install (`~/.local/bin`, no sudo). User
# install matches `install.sh`'s end-user default and avoids password
# prompts during dev iteration — important for headless / agent-driven
# rebuild loops where a sudo prompt halts everything. Use `--system`
# explicitly when you want the system-wide copy at `/usr/local/bin`
# (e.g., to overwrite a pre-existing system install that's shadowing
# your user-local one — see the drift warning below).
#
# `--user` is kept as a deprecated alias for the default (no-op) so
# pre-flip muscle memory keeps working.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="$REPO_ROOT/target/release"
DO_BUILD=true
DO_PLUGINS=true
DO_DAEMON=true
DO_RESTART=false
USER_INSTALL=true

while [ "$#" -gt 0 ]; do
    case "$1" in
        --user)        USER_INSTALL=true ; shift ;;   # deprecated alias for default
        --system)      USER_INSTALL=false ; shift ;;
        --no-build)    DO_BUILD=false ; shift ;;
        --no-plugins)  DO_PLUGINS=false ; shift ;;
        --no-daemon)   DO_DAEMON=false ; shift ;;
        --with-daemon) DO_DAEMON=true ; shift ;;       # explicit; default is on
        --restart)     DO_RESTART=true ; shift ;;
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

if $USER_INSTALL; then
    INSTALL_DIR="$HOME/.local/bin"
    DESKTOP_DIR="$HOME/.local/share/applications"
    ICON_BASE="$HOME/.local/share/icons/hicolor"
    SUDO=""
else
    INSTALL_DIR="/usr/local/bin"
    DESKTOP_DIR="/usr/share/applications"
    ICON_BASE="/usr/share/icons/hicolor"
    SUDO="sudo"
fi

if $DO_BUILD; then
    echo "==> cargo build --release --workspace"
    cargo build --release --workspace --manifest-path "$REPO_ROOT/Cargo.toml"
fi

for bin in nestty nestctl nesttyd; do
    src="$TARGET/$bin"
    if [ ! -x "$src" ]; then
        echo "error: $src not built — run with default flags or 'cargo build --release'" >&2
        exit 1
    fi
done

echo "==> installing nestty + nestctl + nesttyd into $INSTALL_DIR"
if [ -n "$SUDO" ]; then
    # `install -m755` on existing files just rewrites; safe to repeat.
    # nesttyd is the always-on daemon for trigger dispatch + plugin
    # supervision — bundling it here means a single install gives
    # the user the full GUI + CLI + daemon set.
    $SUDO install -Dm755 "$TARGET/nestty" "$INSTALL_DIR/nestty"
    $SUDO install -Dm755 "$TARGET/nestctl" "$INSTALL_DIR/nestctl"
    $SUDO install -Dm755 "$TARGET/nesttyd" "$INSTALL_DIR/nesttyd"
else
    mkdir -p "$INSTALL_DIR"
    install -Dm755 "$TARGET/nestty" "$INSTALL_DIR/nestty"
    install -Dm755 "$TARGET/nestctl" "$INSTALL_DIR/nestctl"
    install -Dm755 "$TARGET/nesttyd" "$INSTALL_DIR/nesttyd"
fi

echo "==> installing desktop entry + hicolor icons into ${DESKTOP_DIR%/applications} / $ICON_BASE"
# Desktop file basename matches the GTK app_id (com.marshall.nestty)
# so Wayland compositors can map the running window to this launcher
# entry. Without that mapping, the WM falls back to a generic icon
# and the StartupNotify cookie does not flow through.
$SUDO install -Dm644 \
    "$REPO_ROOT/nestty-linux/com.marshall.nestty.desktop" \
    "$DESKTOP_DIR/com.marshall.nestty.desktop"
# Cleanup: a pre-rename "nestty.desktop" lingering at the same dest
# would show up as a second, broken launcher entry.
$SUDO rm -f "$DESKTOP_DIR/nestty.desktop"
for size in 16 22 24 32 48 64 128 256 512; do
    $SUDO install -Dm644 \
        "$REPO_ROOT/nestty-linux/icons/hicolor/${size}x${size}/apps/nestty.png" \
        "$ICON_BASE/${size}x${size}/apps/nestty.png"
done
# Refresh the icon cache so launchers pick up Icon=nestty without a logout.
# gtk-update-icon-cache is in libgtk-4 / gtk4 packages — present on any
# system that already builds nestty, so the missing-binary branch is rare.
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    $SUDO gtk-update-icon-cache -q -t "$ICON_BASE" || true
fi

# Sanity: user might have BOTH ~/.local/bin/nestty and
# /usr/local/bin/nestty. PATH typically prefers /usr/local/bin, so
# if they're out of sync the user gets the wrong one. We can't
# auto-fix without making policy decisions about which copy to
# trust, but we can flag the drift so the next "why isn't my fix
# applied?" debug session is shorter. Concrete remedies the user
# can pick from are listed in the warning.
if [ -x "$HOME/.local/bin/nestty" ] && [ -x "/usr/local/bin/nestty" ]; then
    if ! cmp -s "$HOME/.local/bin/nestty" "/usr/local/bin/nestty"; then
        echo
        echo "warn: ~/.local/bin/nestty and /usr/local/bin/nestty differ." >&2
        echo "warn: PATH lookup typically picks /usr/local/bin first;" >&2
        echo "warn: a desktop-entry-launched nestty will use the system copy." >&2
        echo "warn: to resolve, pick one of:" >&2
        echo "warn:   - re-run with --system (overwrites the system copy with this build)" >&2
        echo "warn:   - re-run without --system (overwrites the user copy with this build)" >&2
        echo "warn:   - sudo rm /usr/local/bin/nestty (let the user-local copy win)" >&2
        echo "warn:   - rm $HOME/.local/bin/nestty (drop the user-local copy entirely)" >&2
        echo
    fi
fi

if $DO_PLUGINS; then
    echo "==> installing first-party plugin manifests + binary symlinks"
    bash "$REPO_ROOT/scripts/install-plugins.sh"
fi

if $DO_DAEMON; then
    # `systemd --user` unit install. Without this, nesttyd never starts
    # automatically — every reboot or new SSH session leaves the user
    # with no daemon, and harness publishes silently no-op via the
    # --quiet path. The block below copies the unit + does
    # daemon-reload / enable / restart so the binary is picked up on
    # both first install and subsequent re-installs (where the unit
    # is already active and `enable --now` would no-op the start).
    # On systems with `loginctl enable-linger <user>` set, the daemon
    # also survives logout. See docs/harness-hooks.md "SSH + daemon
    # lifecycle".
    if command -v systemctl >/dev/null 2>&1; then
        UNIT_DST="$HOME/.config/systemd/user"
        UNIT_SRC="$REPO_ROOT/dist/systemd/nestty-daemon.service"
        if [ ! -f "$UNIT_SRC" ]; then
            echo "warn: $UNIT_SRC missing — skipping daemon service install" >&2
        else
            echo "==> installing systemd --user unit into $UNIT_DST"
            mkdir -p "$UNIT_DST"
            # Rewrite NESTTYD_BIN_PATH to the actual install location.
            # Bash 5.3+ defaults `patsub_replacement` ON, which makes
            # unescaped `&` in `${var//pat/repl}` expand to the
            # matched text — same hazard as sed/awk. Disable the
            # shopt so the replacement is guaranteed literal. The
            # shopt name was introduced in bash 5.3 so the unset
            # error-no-ops on older bash. Codex C1 step 7 rounds 2-3.
            shopt -u patsub_replacement 2>/dev/null || true
            UNIT_TEXT=$(cat "$UNIT_SRC")
            UNIT_TEXT=${UNIT_TEXT//NESTTYD_BIN_PATH/$INSTALL_DIR/nesttyd}
            printf '%s\n' "$UNIT_TEXT" > "$UNIT_DST/nestty-daemon.service"
            chmod 644 "$UNIT_DST/nestty-daemon.service"
            # daemon-reload picks up the unit. `enable` creates the
            # WantedBy symlink so the unit starts on next user-session
            # boot. `restart` covers BOTH the first-install case
            # (restart of an inactive unit ≡ start) AND the re-install
            # case where the binary changed and we need the running
            # daemon to pick it up. `enable --now` would silently
            # leave the old binary in place if the unit was already
            # active. Codex C1 step 7 round 6.
            systemctl --user daemon-reload
            systemctl --user enable nestty-daemon.service
            systemctl --user restart nestty-daemon.service
            echo "    systemctl --user status nestty-daemon  # to inspect"
            echo "    journalctl --user -u nestty-daemon -f  # to tail logs"
            # Linger reminder — not enabled automatically (per-user
            # policy decision). Without it, daemon dies on last logout.
            if ! loginctl show-user "$USER" --property=Linger 2>/dev/null | grep -q '^Linger=yes$'; then
                echo
                echo "note: linger is OFF for $USER — nesttyd will die on last logout."
                echo "      to keep it running across logout (so boot starts the daemon"
                echo "      and SSH connects to an already-running instance):"
                echo "          sudo loginctl enable-linger $USER"
            fi
        fi
    else
        echo "warn: systemctl not on PATH; skipping daemon service install."
        echo "warn: on non-systemd systems, run nesttyd via your own init"
        echo "warn: (OpenRC / runit / direct nohup ~/.local/bin/nesttyd)."
    fi
fi

if $DO_RESTART; then
    echo "==> pkill -x nestty (you'll need to relaunch via desktop entry / shell)"
    pkill -x nestty 2>/dev/null || true
else
    echo
    echo "Restart nestty to pick up the new binary:"
    echo "  pkill -x nestty"
    echo "  # then relaunch via your usual path (desktop entry / shell)"
fi
