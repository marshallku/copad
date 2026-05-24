# nestty Documentation Index

## File Structure

| File                                       | Purpose                                     | When to Read                              |
| ------------------------------------------ | ------------------------------------------- | ----------------------------------------- |
| [architecture.md](./architecture.md)       | Project structure, crate layout, tech stack | Starting work, understanding the codebase |
| [linux-app.md](./linux-app.md)             | GTK4 + VTE4 Linux app internals             | Working on nestty-linux                     |
| [macos-app.md](./macos-app.md)             | Swift/AppKit + SwiftTerm macOS app          | Working on nestty-macos                     |
| [macos-porting-guide.md](./macos-porting-guide.md) | Onboarding guide for picking up macOS work on a Mac (current state, build/dev loop, paths, phased TODO) | First session on the Mac, or coming back after Linux-only stretch |
| [macos-parity-plan.md](./macos-parity-plan.md) | Tiered plan to bring macOS to Linux parity (codex-reviewed) | Picking next macOS work item |
| [macos-daemon-migration-plan.md](./macos-daemon-migration-plan.md) | 7-PR plan to migrate macOS from monolithic to daemon-client (codex round 1/2/3 reflected) | After parity-plan Tier 4; this is the next architectural gate |
| [macos-renderer-migration-plan.md](./macos-renderer-migration-plan.md) | Vertical-slice plan to replace SwiftTerm with alacritty_terminal + custom AppKit/CoreText renderer (decision #31) | After daemon migration; the long-running 3-6 month effort that addresses SwiftTerm's structural limits |
| [macos-post-renderer-catchup.md](./macos-post-renderer-catchup.md) | Living backlog for after Phase 10a default flip — renderer polish + Linux-parity catch-up + Phase 10b SwiftTerm removal | Picking next macOS work item after the renderer migration plan |
| [core-lib.md](./core-lib.md)               | Shared Rust core library modules            | Working on nestty-core                      |
| [cli.md](./cli.md)                         | CLI tool (nestctl) and D-Bus interface      | Working on remote control features        |
| [config.md](./config.md)                   | Configuration format and defaults           | Adding config options                     |
| [decisions.md](./decisions.md)             | Key technical decisions and rationale       | Understanding "why" behind choices        |
| [troubleshooting.md](./troubleshooting.md) | Known issues, fixes, gotchas                | Debugging problems                        |
| [plugins.md](./plugins.md)                 | Plugin development guide + JS bridge API    | Creating plugins                          |
| [workflow-runtime.md](./workflow-runtime.md) | Event Bus, Action Registry, Context Service design | Designing integrations, triggers, AI context |
| [service-plugins.md](./service-plugins.md) | End-state vision, plugin-first pivot, Phase 9–18 plan | Planning beyond Phase 8 — every external integration goes here |
| [kb-protocol.md](./kb-protocol.md)         | KB action contract (search/read/append/ensure) | Building anything that reads or writes the user's notes |
| [roadmap.md](./roadmap.md)                 | Implementation phases, pending work         | Planning next steps                       |
| [harness-integration.md](./harness-integration.md) | Daemon-first pivot + integrations with the user's external harness/tools (~/dotfiles/claude, ~/dev/browser, codex-plugin-cc, life-assistant) | Picking next harness-coupled work |
| [gui-daemon-protocol.md](./gui-daemon-protocol.md) | GUI ↔ daemon wire protocol spec (Invoke, gui.register, capabilities, origin tagging) | Implementing daemon-first migration step 1+ |

## Quick Reference

- **Binary names**: `nestty` (terminal app), `nestctl` (CLI control tool)
- **Config path**: `~/.config/nestty/config.toml`
- **Cache path**: `~/.cache/terminal-wallpapers.txt` (Linux) / `~/Library/Caches/nestty/wallpapers.txt` (macOS, falls back to Linux path)
- **GTK app ID**: `com.marshall.nestty`
- **Theme**: Catppuccin Mocha
- **Rust edition**: 2024
