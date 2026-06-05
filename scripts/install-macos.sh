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
DO_BUILD=true
SYSTEM_INSTALL=false
DO_COCTL=true
DO_COPADD=true
DO_PLUGINS=true
DO_DAEMON=true
DO_LAUNCH=false

# macOS-buildable plugins. All first-party plugins now compile on macOS:
# - PR 4 added git (no platform-specific deps).
# - PR 5a added llm — proved `keyring` `apple-native` reaches Apple
#   Keychain at runtime.
# - PR 5b added calendar — validated the polling-daemon supervisor
#   lifecycle on macOS (background poller publishing
#   `calendar.event_imminent`). RPC actions still work without Google
#   OAuth creds thanks to `Config::minimal()` fallback.
# - kb / todo / bookmark formerly required Linux's `renameat2(RENAME_NOREPLACE)`;
#   the shared `copad_core::fs_atomic` primitive now selects between
#   `renameat2` (Linux) and `renamex_np(RENAME_EXCL)` (Darwin), so all
#   three install and run on macOS.
# - slack / discord install fine; full functionality needs user-supplied
#   Slack `xoxb-` tokens / Discord bot tokens in Keychain (see plugin
#   READMEs). Without creds the plugins return RPC errors gracefully
#   rather than crashing the supervisor.
MACOS_PLUGINS=(echo git llm calendar kb todo bookmark slack discord jira claude)

while [[ $# -gt 0 ]]; do
    case "$1" in
        --system)      SYSTEM_INSTALL=true ; shift ;;
        --no-build)    DO_BUILD=false ; shift ;;
        --no-coctl)  DO_COCTL=false ; shift ;;
        --no-copadd)  DO_COPADD=false ; shift ;;
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

# 1. Build the macOS app via SwiftPM (release config).
#    The Copad executable links libcopad_ffi.a from the Rust staticlib crate;
#    SwiftPM cannot run cargo as a prebuild step from Package.swift, so we
#    invoke cargo here first. swift build's linker phase then picks up the
#    archive at $REPO_ROOT/target/release/libcopad_ffi.a via the
#    -L../target/release flag baked into Package.swift.
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

# 2. Stop any running instance so we can replace the bundle's executable.
pkill -x Copad 2>/dev/null || true
sleep 0.3

# 3. Stage the bundle in a tmp dir so the install is atomic — the user
#    never sees a half-written .app at $APP_DEST.
STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGING_DIR"' EXIT
STAGING="$STAGING_DIR/$APP_NAME"
CONTENTS="$STAGING/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
mkdir -p "$MACOS" "$RESOURCES"
cp "$BUILT_BIN" "$MACOS/Copad"

# Bundle icon. CFBundleIconFile expects the basename ("AppIcon") and
# Finder/Dock/Launchpad pull pixels from Resources/AppIcon.icns. The
# .icns is checked in (generated from assets/icons/copad.png — see
# scripts/build-icons.sh) so swift build alone is enough to produce a
# fully-iconed bundle.
ICNS_SRC="$REPO_ROOT/copad-macos/Resources/AppIcon.icns"
if [[ -f "$ICNS_SRC" ]]; then
    cp "$ICNS_SRC" "$RESOURCES/AppIcon.icns"
else
    echo "warn: $ICNS_SRC missing — bundle will fall back to the generic app icon" >&2
fi

# Info.plist — kept in sync with copad-macos/run.sh by hand. Two copies is
# acceptable (Rule of Three); a third would mean extracting to a template.
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
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
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

# 4. Sign the staging bundle with a stable self-signed cert. Without
#    this, swift's ad-hoc linker signature gives a fresh cdhash on every
#    build, so macOS TCC treats each install as a different app and
#    re-prompts for every permission grant. Signing at staging means
#    --system installs don't need sudo for codesign.
"$REPO_ROOT/scripts/codesign-dev.sh" "$STAGING"

# 5. Install — replace any prior bundle in one rename so a partially-failed
#    install never leaves $APP_DEST in a broken state.
echo "==> installing $APP_NAME to $APP_DEST"
mkdir -p "$APP_DEST" 2>/dev/null || $SUDO_APP mkdir -p "$APP_DEST"
$SUDO_APP rm -rf "$APP_DEST/$APP_NAME"
$SUDO_APP mv "$STAGING" "$APP_DEST/$APP_NAME"

# 6. Install coctl + copadd via cargo install (writes to ~/.cargo/bin).
#    Same rationale as coctl: `cargo install <name>` fails (not on
#    crates.io) and `cargo install --path .` fails (workspace virtual
#    manifest), so we always pass `--path <crate-dir>`.
#
#    copadd is the background daemon (status bar, triggers, plugin
#    runtime). The Swift app auto-spawns it on launch when missing, so
#    a fresh install without copadd in PATH would warn-and-skip the
#    status bar / plugin features.
if $DO_COCTL; then
    echo "==> cargo install --path copad-cli (coctl → ~/.cargo/bin)"
    cargo install --path "$REPO_ROOT/copad-cli"
fi
if $DO_COPADD; then
    echo "==> cargo install --path copad-daemon (copadd → ~/.cargo/bin)"
    cargo install --path "$REPO_ROOT/copad-daemon"
fi

# 7. Build + install macOS-buildable plugins. PluginSupervisor (PR 3) reads
#    ~/Library/Application Support/copad/plugins/<name>/ at startup; we
#    cargo-build the binary and copy the manifest. Manifest's
#    `services.exec` is resolved against the plugin dir first, so we drop
#    the binary alongside plugin.toml so the supervisor finds it without
#    a $PATH dance.
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
        # Copy every loose file next to plugin.toml (manifest + panel.html
        # if any) so panel-bearing plugins land complete. Cargo.toml lives
        # in the same dir but is build-time only — exclude it.
        find "$REPO_ROOT/plugins/$name" -maxdepth 1 -type f ! -name 'Cargo.toml' \
            -exec cp -f {} "$plugin_dir/" \;
        # Copy (don't symlink) the binary so a `git clean` of target/ doesn't
        # silently break the install. Cheap — these binaries are small.
        cp -f "$bin_src" "$plugin_dir/$crate"
        chmod 755 "$plugin_dir/$crate"
        echo "ok    plugin $name → $plugin_dir/"
    done
fi

# 8. Install the LaunchAgent plist so copadd auto-starts at login
#    (and `KeepAlive=true` restarts it on crash). The plist template
#    in dist/launchd/ uses HOME_PLACEHOLDER tokens because launchd
#    does not expand `~` — we rewrite them with the user's actual
#    HOME during install. `launchctl bootstrap` is the modern
#    replacement for `launchctl load`; we `bootout` first to make
#    this idempotent across re-runs.
if $DO_DAEMON; then
    if command -v launchctl >/dev/null 2>&1 && $DO_COPADD; then
        PLIST_SRC="$REPO_ROOT/dist/launchd/com.marshall.copad.daemon.plist"
        PLIST_DST="$HOME/Library/LaunchAgents/com.marshall.copad.daemon.plist"
        if [[ ! -f "$PLIST_SRC" ]]; then
            echo "warn: $PLIST_SRC missing — skipping LaunchAgent install" >&2
        else
            echo "==> installing LaunchAgent plist into $PLIST_DST"
            mkdir -p "$(dirname "$PLIST_DST")"
            # XML-escape `$HOME` then drop it into the plist. Two
            # bash-version pitfalls compound here:
            # 1. Bash 5.3+ defaults `patsub_replacement` ON, which
            #    expands `&` in `${var//pat/repl}` replacements to
            #    the matched substring. That breaks BOTH the
            #    escape passes below (`&amp;`/`&lt;`/`&gt;` all
            #    contain `&`) AND the final HOME_PLACEHOLDER
            #    substitution. Disable it FIRST so every later
            #    substitution treats `repl` as literal text.
            #    Codex C1 step 7 rounds 2-3 + round 5.
            # 2. plist `<string>...</string>` is XML — raw `&`/`<`
            #    breaks plistlib. Order escapes so the `&` we
            #    inject for `&amp;` is not re-escaped by later
            #    passes. Codex C1 step 7 round 4.
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

# Shell hooks for live-cwd reporting. macOS alacritty backend can't
# capture OSC 7 (no vte handler) and proc_pidinfo is EPERM under
# hardened runtime; the in-shell hook calls `coctl call
# panel.report_cwd` on every chpwd instead. Files land under
# ~/.config/copad/shell-hooks/; users source one from their rc file.
SHELL_HOOK_SRC="$REPO_ROOT/copad-macos/shell-hooks"
SHELL_HOOK_DEST="$HOME/.config/copad/shell-hooks"
if [[ -d "$SHELL_HOOK_SRC" ]]; then
    mkdir -p "$SHELL_HOOK_DEST"
    for f in "$SHELL_HOOK_SRC"/copad-cwd.*; do
        [[ -f "$f" ]] || continue
        cp -f "$f" "$SHELL_HOOK_DEST/$(basename "$f")"
    done
fi

# Phase 22.1 context bridge (zsh). Single source of truth at
# examples/shell/copad-context.zsh — same script Linux uses. macOS
# zsh ≥ 5.0 ships `zmodload zsh/datetime` natively, so no platform
# fork is needed. Sourcing is opt-in: the dossier panel (Phase 22.2)
# is the eventual consumer, and the script is a no-op outside copad
# PTY children regardless.
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
