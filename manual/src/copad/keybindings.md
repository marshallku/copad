# Keybindings & Shortcuts

copad's desktop shortcuts use `Ctrl+Shift+…` on Linux and the `Cmd`-equivalent on macOS. All of them can be overridden in `config.toml`.

## Built-in shortcuts

| Action | Linux | macOS |
| --- | --- | --- |
| New tab | `Ctrl+Shift+T` | `Cmd+T` |
| New web (webview) tab | `Ctrl+Shift+U` | `Cmd+Shift+T` |
| Close pane / tab | `Ctrl+Shift+W` | `Cmd+W` |
| Toggle tab bar (collapse) | `Ctrl+Shift+B` | `Ctrl+Shift+B` |
| Focus next pane | `Ctrl+Shift+N` | — |
| Focus previous pane | `Ctrl+Shift+Left` | — |
| Search in terminal | `Ctrl+Shift+F` | `Cmd+F` |
| Font size up / down / reset | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | `Cmd+=` / `Cmd+-` / `Cmd+0` |
| Command palette | `Ctrl+Shift+P` | `Cmd+Shift+P` |
| Agent cockpit | `Ctrl+Shift+Y` | (`coctl call cockpit.open`) |

> On Linux, `Ctrl+Shift+U` collides with IBus Unicode input. If you use IBus, rebind the web-tab shortcut (below) or open web tabs from the tab-bar "+" popover instead.

Font scaling on Linux ranges 0.3×–3.0× in 0.1 steps.

---

## Customizing keybindings

Add a `[keybindings]` table to `config.toml`. Each entry maps a chord to an action. Custom bindings are checked **first**, so they override built-ins.

Chords use `ctrl`, `shift`, `alt` plus a key name (GDK naming on Linux — `a`, `bracketright`, `f1`).

Two action forms:

- **`spawn:<cmd>`** — run a background shell command (tilde-expanded). Spawned commands inherit `COPAD_SOCKET`, so `coctl` inside them targets this instance automatically.
- **`action:<method> [k=v …]`** — dispatch an in-process action. All values arrive as strings, so an integer-argument method like `tab.switch` won't work through `action:` — use `spawn:coctl tab switch 0` instead.

```toml
[keybindings]
# Open the git plugin panel
"ctrl+shift+g" = "spawn:coctl plugin open git"

# Jump to the first tab (integer arg → use spawn + coctl)
"ctrl+shift+1" = "spawn:coctl tab switch 0"

# Split the focused pane vertically
"ctrl+shift+backslash" = "spawn:coctl split vertical"

# Toggle the status bar
"ctrl+shift+s" = "action:statusbar.toggle"
```

On Linux, a value with no `spawn:` / `action:` prefix is treated as `spawn:`. On macOS an unprefixed value warns and is skipped, so always prefix it.

Keybinding changes hot-reload with the rest of `config.toml`.

---

## Everything is also a `coctl` command

Any shortcut you'd want has a CLI equivalent you can bind, script, or run directly — see the [coctl Command Reference](../coctl/reference.md). That's the recommended way to build custom shortcuts: bind a key to `spawn:coctl …`.
