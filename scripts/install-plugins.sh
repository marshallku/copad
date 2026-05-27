#!/usr/bin/env bash
# Install first-party plugins into ~/.config/copad/plugins/ with
# binaries symlinked from a build directory. Solves the common
# "service X is not running" startup error caused by copad being
# launched from a desktop entry whose PATH doesn't include the
# built binaries.
#
# Run after `cargo build --release --workspace`.
#
# Usage:
#   ./scripts/install-plugins.sh           # install every first-party plugin
#   ./scripts/install-plugins.sh todo git  # install just these two
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGINS_SRC_DIR="$REPO_ROOT/plugins"
TARGET_DIR="$REPO_ROOT/target/release"
PLUGIN_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/copad/plugins"

if [ ! -d "$PLUGINS_SRC_DIR" ]; then
    echo "error: $PLUGINS_SRC_DIR not found — run from a copad checkout" >&2
    exit 1
fi

# Idempotent claude-prompt layer scaffolding. The Todo plugin reads
# these files at `todo.start` time to assemble the layered prompt
# fed into claude.start (Phase 18.2). They're plain markdown — vim
# them whenever the common preamble drifts. We only create empty
# stubs when missing; existing user content is never touched.
#
# Path derivation MUST match `copad-plugin-todo::prompt::docs_root_for`
# at runtime: `parent(COPAD_TODO_ROOT)`. Default is `~/docs/todos` →
# stubs at `~/docs/claude/`. Anything else here would create stubs
# the plugin never reads.
TODO_ROOT="${COPAD_TODO_ROOT:-$HOME/docs/todos}"
DOCS_ROOT="$(dirname -- "$TODO_ROOT")"
ensure_claude_stub() {
    local path="$1"
    local body="$2"
    if [ ! -e "$path" ]; then
        mkdir -p "$(dirname "$path")"
        printf '%s\n' "$body" > "$path"
        echo "stub  $path"
    fi
}
ensure_claude_stub "$DOCS_ROOT/claude/global.md" "# Global preamble for claude sessions

Common context applied to every Todo's claude.start prompt. Edit
freely — the Todo plugin re-reads this file at start time, so
changes apply on the next click without restarting copad.

(stub created by scripts/install-plugins.sh — replace with your
own coding rules, language conventions, project-wide reminders.)"

# Default to every plugin that has a plugin.toml.
if [ "$#" -eq 0 ]; then
    set -- $(cd "$PLUGINS_SRC_DIR" && find . -mindepth 2 -maxdepth 2 -name plugin.toml -printf '%h\n' | sed 's|^./||' | sort)
fi

mkdir -p "$PLUGIN_DIR"

for name in "$@"; do
    src="$PLUGINS_SRC_DIR/$name"
    dst="$PLUGIN_DIR/$name"
    if [ ! -f "$src/plugin.toml" ]; then
        echo "skip $name: $src/plugin.toml not found"
        continue
    fi
    mkdir -p "$dst"
    # Copy every non-binary, non-build file (manifest + panel.html etc).
    # Plugin authors put HTML/CSS/JS alongside plugin.toml; copy
    # them all so the plugin's WebView panel can find its assets.
    # Exclude Cargo.toml — it lives alongside plugin.toml in the
    # source tree (one dir per plugin) but is build-time only and
    # has no runtime use in ~/.config/copad/plugins/<name>/.
    find "$src" -maxdepth 1 -type f ! -name 'Cargo.toml' -exec cp -f {} "$dst/" \;

    # Symlink the binary if it's a [[services]] plugin. Bare-
    # panel plugins (no services entry) don't need a binary.
    exec_name=$(awk '/^\[\[services\]\]/,/^$/' "$src/plugin.toml" \
                | awk -F'=' '/^[[:space:]]*exec[[:space:]]*=/ { gsub(/[[:space:]"]/, "", $2); print $2; exit }')
    if [ -n "$exec_name" ]; then
        bin_src="$TARGET_DIR/$exec_name"
        if [ ! -x "$bin_src" ]; then
            echo "warn  $name: $bin_src not built — run 'cargo build --release -p $exec_name' first" >&2
            continue
        fi
        ln -sf "$bin_src" "$dst/$exec_name"
        echo "ok    $name (binary: $exec_name)"
    else
        echo "ok    $name (panel-only)"
    fi
done

echo
echo "Restart copad so discover_plugins() picks up the changes."
