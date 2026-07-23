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
- `copad-mux/` — standalone agent-orchestration terminal multiplexer (binary: `comux`; crate/dir stays `copad-mux`, like `copad-cli`→`coctl`): **server/client split** (persistent server owns `State` + PTYs and survives the launching terminal; thin client renders + forwards input). Bare `comux` = connect-or-spawn the server + attach; `comux server` runs it headless; `comux ctl <cmd>` controls it — and **any control verb works without `ctl`** (`comux new-session work` == `comux ctl new-session work`, tmux-style; `comux help` lists them). Features: **configurable keybindings + options** via `~/.config/copad/mux.toml` (tmx-style: TOML, overlay-merge onto defaults, warn-once on bad values; `[keys]` = prefix table, `[global]` = prefix-less, action→chord where an override replaces that action's default chord set; plus `prefix`/`mouse`/`notify`/`sidebar`/`sidebar_width`/`sidebar_min_cols`/`scroll_step`/`persist`/`autosave_secs`/`restore_processes`; see `copad-mux/mux.example.toml`; zero-config = identical to before), **session persistence** (tmux-resurrect/continuum-style: sessions/tabs/split-layout/per-pane-cwd autosaved to `$COPAD_MUX_STATE` else `~/.local/state/copad/mux-session.json` by an off-loop writer thread — temp+fsync+rename+dir-fsync — and restored on server start; continuum semantics = always restore the last layout, `persist = false` or delete the file for a fresh start; **whitelisted programs re-run** — a pane whose foreground command's basename is in `restore_processes` (default = the AI agents) has its full command line saved and re-injected into the fresh shell on restore, so agents relaunch; non-whitelisted panes restore as bare shells; restored panes start at the scrollback bottom (history not saved); transactional restore spawns PTYs off-state + prunes failures, validates untrusted snapshots with depth/leaf/pane/command caps), multi-pane splits, tabs (`Ctrl-b c`/`n`/`p`/`&`, `Ctrl-b 1`–`9` jump, plus prefix-less **`Alt`/`Opt`+`1`–`9`** à la tmux `bind -n M-1` — needs the terminal set to send Option/Alt as Meta), **multi-session** (`Ctrl-b C` new — opens an inline name prompt, `Ctrl-b $` rename, **`Ctrl-b X` kill-session** (y/n confirm), `)`/`(` next/prev; `comux new-session [name]`/`rename-session <idx> <name>`/`list-sessions` (shows NAME)/`select-session`; **tmux-style named sessions** — name shown in the status-bar pill + sidebar; **cwd inheritance** — `comux new-session` starts the shell in the CALLER's directory (like `tmx $name`: `cd dir; comux new-session name`), and TUI new session/tab/split inherit the focused pane's cwd (tmux `-c '#{pane_current_path}'`); **`comux new-session` auto-starts the server** if none is running (tmux-style); each session = an isolated workspace of shells, switching preserves state), directional pane focus via `Ctrl-b h/j/k/l` (vim) or arrows, plus prefix-less `Ctrl+Shift+h/j/k/l` / `Ctrl+Shift+arrow`, **pane resize `Ctrl-b H/J/K/L`** (`ctl resize`), **force full repaint `Ctrl-b r`** (tmux `refresh-client`: server re-sends a `full` frame + the client `terminal.clear()`s, wiping any drift/ghosting from a resize / alt-screen transition / nested emulator; the client also auto-`clear`s on EVERY `full` frame), **scrollback** (`Ctrl-b [` copy-mode: `g/G/j/k/PgUp/PgDn`, bound per-pane; **mouse wheel** scrolls the pane under the cursor — but if the focused pane's app has mouse reporting on (Claude Code, nvim `set mouse`) the wheel is FORWARDED to it (SGR/legacy button 64/65 by the app's negotiated encoding), and an alt-screen pager with alternate-scroll (`less`/`man`) gets cursor-key presses instead — only a plain shell scrolls comux's own scrollback (`term.rs::wheel_bytes`); **click** focuses it — and **clicking a status-bar tab chip switches tabs, a sidebar `spaces` row switches sessions, a sidebar `agents` row jumps to that agent's pane**), an always-on **bottom status bar** (Catppuccin Mocha, matching the owner's tmux: session pill · tab chips (**windowed around the active tab with `‹`/`›` overflow markers** so it stays visible with many tabs; agent tabs yellow + `● ` marker) · **`⚑N` attention count** · `● N` agent count · scroll flag · **usage/limits readout** (`claude 5h 6% wk 27% · codex wk 45%` — subscription rate-limit window utilization from `coctl usage --limits --oneline`, refreshed off-loop by a `usagepoll` thread every 60s, shown when `cols >= 100`; `COPAD_MUX_USAGE=0` disables) · clock · host — tabs live here now, no top bar), **agent turn notifications** (the server watches each agent's status TRANSITIONS and fires a native desktop toast on turn-finished / awaiting-input — even while detached; `COPAD_MUX_NOTIFY=0` to disable; `Ctrl-b !` jumps to a blocked agent; **`Ctrl-b a` opens a notification center** (logged turn events — jump/dismiss). This CAN replace the `~/.claude` notify-stop/notification/attention hooks (retirement checklist in decisions #66), but the owner kept that stack for now — do not auto-retire it), an **always-on left sidebar** (herdr-style, `Ctrl-b s` toggles, size-adaptive): **spaces** (sessions + **git-branch subtitle**, read straight from `.git/HEAD`; **windowed around the active session with a `+N more · Ctrl-f` hint** when they overflow) on top, **agents** (every agent pane across sessions with `status · tool`; **real status ported from `~/dev/tmx`** — Claude's `~/.claude/sessions/<pid>.json` `busy`/`idle`/`waiting` → `working`/`ready`/`blocked`, screen-text fallback for Codex/others; also on `ctl list`) on bottom, **keyboard-focusable sidebar** (`Ctrl-b e`: `↑↓`/`jk` move, `←→`/`hl` switch spaces↔agents, Enter select, Esc exit — nvim-explorer-style; `Ctrl-b s` still toggles it hidden), **`Ctrl-f` fuzzy switcher** (SESSIONS + AGENTS as two tabs — `←→` switch tab, `↑↓`/Ctrl-n/p move, type to fuzzy-filter, Enter switches session / jumps to agent, **Ctrl-r/F2 renames** the selected session — the owner's tmux `Ctrl-f` `tmx switch` + `prefix g` `tmx agents` in one), **configurable session order** (`sort_by = created|alphabetical|recent|activity` — applies to sidebar/switcher/cycle), **detach `Ctrl-b d` / reattach** (shell state survives), **shared multi-client** (several clients attach at once, view sized to the SMALLEST — tmux-style; shared input; `Ctrl-b d` detaches only that client; bigger terminals letterbox), `ctl kill-server`. (Mouse capture takes over native selection — Shift bypasses on most terminals; wheel IS forwarded to mouse-aware pane apps, but click/drag are not yet forwarded.) Built on an authoritative single-writer state model (control lease / geometry / mutation, property-tested); v1 render transport is a composed cell-diff broadcast to all clients (smallest-fit) — the per-pane semantic-grid protocol (heterogeneous simultaneous sizes) is deferred (decisions #66). See `docs/agent-mux-spec.md` + decisions #63–#66.
- `copad-macos/` — Swift/AppKit app (full secondary platform: alacritty_terminal renderer, tabs/splits, webview, plugins, daemon client — see `docs/macos-post-renderer-catchup.md` for the remaining polish backlog)
- `copad-ios/` — SwiftUI + WKWebView thin native shell around the `web-bridge` PWA (mobile client; xcodegen project, Simulator-verified). See `copad-ios/README.md` and `docs/mobile-access.md`.
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
./scripts/install-dev.sh           # cargo build --release + install ~/.local/bin/{copad,coctl,copadd,comux} + plugins (no sudo)
./scripts/install-dev.sh --system  # /usr/local/bin instead of ~/.local/bin (requires sudo)
./scripts/install-dev.sh --restart # also pkill -x copad afterwards

# macOS
./scripts/install-macos.sh             # swift build -c release + ~/Applications/Copad.app + ~/.cargo/bin/{coctl,copadd,comux} (no sudo)
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
- Unix sockets for IPC: GUI per-instance at `$XDG_RUNTIME_DIR/copad/gui-{PID}.sock`, daemon at its well-known path (`copad_core::paths`); legacy `/tmp/copad-{PID}.sock` recognized for back-compat
- Config: `~/.config/copad/config.toml` (TOML)
- Cache: `~/.cache/terminal-wallpapers.txt` (Linux) / `~/Library/Caches/copad/wallpapers.txt` (macOS, falls back to Linux path)
- Theme: configurable via `[theme] name` — 10 built-ins (catppuccin variants, dracula, nord, tokyo-night, gruvbox-dark, one-dark, solarized-dark), default `catppuccin-mocha`, hot-reloads
- Dark theme forced via GTK settings

## Critical Implementation Details

- **Background images**: Must call `terminal.set_clear_background(false)` for VTE transparency
- **GTK thread safety**: D-Bus → mpsc channel → glib::timeout_add_local polling
- **Binary names**: `copad` (app), `coctl` (CLI), `copadd` (daemon), `comux` (multiplexer; crate/dir `copad-mux`) — do not rename to collide
