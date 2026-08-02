# Installation

copad supports **Linux** (x86_64) and **macOS** (Apple Silicon). Pick the install that matches what you want to run.

- [Full copad](#full-copad) — the desktop terminal + `coctl` + `comux` + `copadd`
- [comux only](#comux-only) — just the multiplexer, a single binary
- [macOS via Homebrew](#macos-via-homebrew)
- [Building from source](#building-from-source)
- [Updating](#updating)

All install scripts fetch prebuilt artifacts from GitHub Releases (`marshallku/copad`). Pass `--version vX.Y.Z` to any of them to pin a specific release instead of the latest.

---

## Full copad

One command detects your OS and installs the matching release:

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/copad/master/install.sh | bash
```

- On **Linux**: the GTK app `copad` + `coctl` + `comux` + `copadd`, a desktop entry and icons, and the `copadd` systemd `--user` unit (started for you).
- On **macOS** (Apple Silicon): `Copad.app` + `coctl` + `copadd` + `comux` + all first-party plugins + the `copadd` LaunchAgent. The bundle is ad-hoc signed, so the installer strips the quarantine attribute for you.

### Passing options through the pipe

Because the script is piped into `bash`, use `bash -s --` to forward flags:

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/copad/master/install.sh \
  | bash -s -- --version v1.0.0 --system --no-daemon
```

| Flag | Effect |
| --- | --- |
| `--version VERSION` | Install a specific release tag (e.g. `v1.0.0`). Default: latest. |
| `--system` | Install system-wide (`/usr/local/bin`, `/Applications`). Requires `sudo`. |
| `--no-daemon` | Don't enable/start `copadd`; also stops & disables any existing daemon. The binary is still installed. |
| `-h`, `--help` | Show usage. |

### Where things land

| Item | User default | With `--system` |
| --- | --- | --- |
| `copad` / `Copad.app` | `~/.local/bin/copad` · `~/Applications/Copad.app` | `/usr/local/bin/copad` · `/Applications/Copad.app` |
| `coctl`, `comux`, `copadd` | `~/.local/bin/` (Linux) · `~/.local/bin/` (macOS) | `/usr/local/bin/` |
| Plugins (macOS) | `~/Library/Application Support/copad/plugins/<name>/` | same |
| Desktop entry + icons (Linux) | `~/.local/share/applications` · `~/.local/share/icons/hicolor` | `/usr/share/...` |

> **Linux runtime libraries.** The GTK app needs `gtk4`, `vte4` (`libvte-2.91-gtk4`), `webkitgtk-6.0`, and the GStreamer good/bad plugin sets. The installer warns (but does not fail) if any are missing. On Arch: `sudo pacman -S gtk4 vte4 webkitgtk-6.0 gst-plugins-good gst-plugins-bad`.

> **Linux plugins are not in the release tarball.** To get first-party plugins on Linux, [build from source](#building-from-source).

> **Keeping the daemon alive across logins.** On **macOS** the daemon runs as a per-user LaunchAgent and is loaded for you. On **Linux** it runs as a systemd `--user` unit — for it to survive across SSH sessions and reboots, enable linger once: `sudo loginctl enable-linger $USER`.

---

## comux only

If all you want is the multiplexer, install just `comux` — no GUI app, no daemon, no plugins, a single self-contained binary:

```bash
curl -fsSL https://raw.githubusercontent.com/marshallku/copad/master/install-comux.sh | bash
```

| Flag | Effect |
| --- | --- |
| `--version VERSION` | Pin a release tag. |
| `--system` | Install to `/usr/local/bin` (needs `sudo`). Default: `~/.local/bin`. |
| `--musl` | Force the fully-static, glibc-free build (**Linux x86_64 only**). Auto-selected on musl systems like Alpine. |
| `-h`, `--help` | Show usage. |

Standalone `comux` requires release **v1.0.1 or newer** (earlier releases bundle it in the full tarball only).

> **Status-bar usage gauge.** comux's status bar can show your Claude/Codex subscription rate-limit utilization. It's resolved **in-process** by comux itself (via the shared `copad-usage` crate), so the standalone binary needs nothing else installed — no `coctl` dependency. To turn the gauge off entirely, set `usage = off` in `~/.config/copad/mux.toml`.

Then start it:

```bash
comux          # start (or attach to) the server
comux help     # list all commands
```

Head to the [comux Overview](./comux/index.md) next.

---

## macOS via Homebrew

Apple Silicon only:

```bash
brew install --cask marshallku/copad/copad
```

This installs `Copad.app` to `/Applications`, `coctl` + `copadd` + `comux` to `$(brew --prefix)/bin`, the first-party plugins, shell hooks, and the `copadd` LaunchAgent.

> **macOS 26 (Tahoe) and later.** Ad-hoc-signed releases stop launching on Tahoe until a Developer-ID-signed release is published. Until then, [install from source](#building-from-source) with `scripts/install-macos.sh` — a self-signed identity survives Tahoe. Supported today: macOS 14 (Sonoma) and 15 (Sequoia).

---

## Building from source

You need a Rust toolchain (edition 2024). On Linux also install the GTK/VTE/WebKitGTK/GStreamer libraries listed above; on macOS install the Xcode Command Line Tools (Swift 6, macOS 14+).

```bash
git clone https://github.com/marshallku/copad.git
cd copad
cargo build                       # build all crates
cargo run -p copad-linux          # run the Linux terminal
cargo run -p copad-cli -- ping    # run the CLI against a running instance
```

Generate a default config file for the GUI:

```bash
cargo run -p copad-linux -- --init-config
```

### Dev installs (from the working tree)

These build the working tree and install it — handy while iterating.

**Linux** — `scripts/install-dev.sh`:

```bash
./scripts/install-dev.sh            # cargo build --release, install to ~/.local/bin (no sudo) + plugins
./scripts/install-dev.sh --system   # install to /usr/local/bin (needs sudo)
./scripts/install-dev.sh --restart  # also `pkill -x copad` afterwards
```

Other flags: `--no-build` (use an existing `target/release`), `--no-plugins`, `--no-daemon`.

**macOS** — `scripts/install-macos.sh`:

```bash
./scripts/install-macos.sh          # ~/Applications + ~/.cargo/bin (no sudo)
./scripts/install-macos.sh --system # /Applications (sudo) + ~/.cargo/bin
./scripts/install-macos.sh --launch # open Copad.app after installing
```

Other flags: `--no-build`, `--no-coctl`, `--no-copadd`, `--no-mux`, `--no-plugins`, `--no-daemon`. Note the CLIs land in `~/.cargo/bin` (via `cargo install`), not `/usr/local/bin`.

### comux alone from source

```bash
cargo build --release -p copad-mux
# static musl build (Linux):
cargo zigbuild --release --target x86_64-unknown-linux-musl -p copad-mux
```

---

## Updating

```bash
coctl update check          # is there a newer release?
coctl update apply          # download and install it (Linux only)
```

- `coctl update apply` is **Linux only**. On macOS, re-run the `install.sh` one-liner, `brew upgrade --cask marshallku/copad/copad`, or `scripts/install-macos.sh`.
- With the daemon running, `copadd` checks GitHub for a newer release once a day and fires a desktop notification. Disable the check with `COPAD_UPDATE_CHECK=0` (or install with `--no-daemon`).
- `comux` shows a `⬆ x.y.z` hint in its status bar when a newer release exists. Disable it with `update_check = false` in `mux.toml` (or `COPAD_MUX_UPDATE_CHECK=0`).

There is no dedicated uninstall script. To tear down the background daemon, run the installer with `--no-daemon`, which stops and disables the systemd unit / LaunchAgent while leaving binaries in place.
