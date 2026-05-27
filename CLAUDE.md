# copad

Cross-platform custom terminal emulator with shared Rust core and platform-native UIs.

## Documentation

**Always read `docs/INDEX.md` first** when starting a session. Read only the specific doc files relevant to your current task.

**Always update docs** when making changes:

- New features or modules → update `docs/architecture.md` and relevant doc
- Bug fixes or gotchas → add to `docs/troubleshooting.md`
- Design decisions → add to `docs/decisions.md`
- Completed/new tasks → update `docs/roadmap.md`

## Project Structure

- `copad-core/` — Shared Rust library (config, background, plugin, protocol, theme, error)
- `copad-linux/` — GTK4 + VTE4 native terminal app (binary: `copad`)
- `copad-cli/` — CLI control tool (binary: `coctl`)
- `copad-macos/` — Swift/AppKit app (stub)
- `plugins/<name>/` — First-party plugins. Each dir holds the Rust crate (`Cargo.toml` + `src/`) and its runtime manifest/assets (`plugin.toml`, `panel.html`, `triggers.example.toml`) together. Crate name remains `copad-plugin-<name>` (binary name unchanged).
- `examples/plugins/hello/` — Tutorial plugin demonstrating a panel + a bash command (no Rust crate)
- `docs/` — Project documentation (architecture, decisions, troubleshooting, roadmap)

## Build & Run

```bash
# Build all
cargo build

# Run terminal
cargo run -p copad-linux

# Run CLI
cargo run -p copad-cli -- <command>
```

## Local development install

`install.sh` is for end users on Linux (downloads from GitHub Releases). For dev iteration on the working tree, use:

```bash
# Linux
./scripts/install-dev.sh           # cargo build --release + install ~/.local/bin/{copad,coctl} + plugins (no sudo)
./scripts/install-dev.sh --system  # /usr/local/bin instead of ~/.local/bin (requires sudo)
./scripts/install-dev.sh --restart # also pkill -x copad afterwards

# macOS
./scripts/install-macos.sh             # swift build -c release + ~/Applications/Copad.app + ~/.cargo/bin/coctl (no sudo)
./scripts/install-macos.sh --system    # /Applications/Copad.app instead (sudo for /Applications)
./scripts/install-macos.sh --launch    # open the installed .app afterwards
```

Why these exist:

- **Linux**: `install-dev.sh` defaults to user install at `~/.local/bin/copad` (no sudo) — matches `install.sh`'s end-user default and avoids sudo prompts during dev iteration. Use `--system` explicitly when you want the system-wide copy at `/usr/local/bin`. If both `~/.local/bin/copad` and `/usr/local/bin/copad` exist and differ, PATH lookup typically picks `/usr/local/bin` first, so a stale system copy can silently shadow your fresh user-local build (and a desktop-entry-launched copad will use the system copy too). The script warns loudly in that case and lists the four resolutions.
- **macOS**: `cargo install copad-cli` fails (not on crates.io) and `cargo install --path .` fails from the repo root (workspace virtual manifest). The `copad` GUI app is SwiftPM, not cargo. Before this script, `copad-macos/run.sh` was the only path and it only built an ephemeral debug bundle under `.build/debug/`. The script wraps `swift build -c release` + bundle layout + `cargo install --path copad-cli` so the user gets a real `/Applications`-style install.

## Install first-party plugins

`install-dev.sh` runs `install-plugins.sh` automatically. To install plugins on their own (e.g. you only changed a plugin manifest):

```bash
./scripts/install-plugins.sh           # all plugins with a manifest
./scripts/install-plugins.sh todo git  # just these two
```

Plugins live in `plugins/<name>/` — each directory holds the Rust crate (`Cargo.toml` + `src/`) **and** its runtime manifest/assets (`plugin.toml`, `panel.html`, `triggers.example.toml`, …) side-by-side. copad's runtime discovers them from `~/.config/copad/plugins/<name>/` at startup. The install script copies the manifest + assets (everything except `Cargo.toml`) and symlinks the built binary into the plugin dir. `<plugin_dir>/<exec>` takes precedence over `PATH`, which matters because copad is often launched from a desktop entry whose env doesn't include `~/.local/bin`. After installing, **restart copad** — `discover_plugins()` only runs at startup. Symptom of an outdated install: `service X is not running and X.action cannot trigger its activation (OnStartup)` from the supervisor.

`examples/plugins/hello/` is a tutorial example (a panel + a bash command, no Rust crate); it stays under `examples/` to mark it as illustrative rather than first-party.

## Git Hooks

After cloning, enable the repo-tracked hooks once:

```bash
git config core.hooksPath .githooks
```

- `pre-commit` — runs `rustfmt --edition 2024` on the working-tree copy of every staged `.rs` file and re-stages each one. Aborts on syntax errors. Caveat: this does not honor partial staging — if you used `git add -p` on a `.rs` file, the formatted full file (including your unstaged edits) will be pulled into the commit. Stage the whole file or skip the hook (`git commit --no-verify`) for partial-stage workflows.
- `pre-push` — runs `cargo clippy --workspace --all-targets -- -D warnings`; blocks push on warnings. Stricter than CI's clippy step (CI omits `--all-targets`), but does **not** run CI's `fmt-check`/`test`/`build` steps — those can still fail in CI.

## Key Conventions

- Rust edition 2024, Cargo workspace with `resolver = "2"`
- GTK4 with `gnome_46` feature flag
- VTE handles PTY on Linux (no custom PTY management)
- Unix socket (`/tmp/copad-{PID}.sock`) for IPC
- Config: `~/.config/copad/config.toml` (TOML)
- Cache: `~/.cache/terminal-wallpapers.txt` (Linux) / `~/Library/Caches/copad/wallpapers.txt` (macOS, falls back to Linux path)
- Theme: Catppuccin Mocha (hardcoded)
- Dark theme forced via GTK settings

## Critical Implementation Details

- **Background images**: Must call `terminal.set_clear_background(false)` for VTE transparency
- **GTK thread safety**: D-Bus → mpsc channel → glib::timeout_add_local polling
- **Binary names**: `copad` (app) and `coctl` (CLI) — do not rename to collide
