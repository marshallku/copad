# The Status Bar

comux draws an always-on bottom status bar (Catppuccin Mocha colors, matching the owner's tmux). Left to right it carries:

**session pill · tab chips · `^b` prefix flag · `⚑N` attention count · `● N` agent count · scroll flag · usage/limits readout · `⬆ x.y.z` update hint · clock · host**

Tabs live here (there is no top bar). Tab chips are windowed around the active tab with `‹`/`›` overflow markers so they stay visible even with many tabs; agent tabs are yellow with a `● ` marker.

---

## The prefix flag

Press `Ctrl-b` and a red **`^b`** pill appears at the left edge of the right-hand cluster: the prefix is armed and comux is waiting for the second key of the chord. It disappears the moment the chord resolves — whether the key was bound to something or not.

It shows the prefix you actually configured, so `prefix = "C-a"` in `mux.toml` renders `^a`.

> With several clients attached to one session, the flag shows when **any** of them is mid-chord. The prefix itself stays per-client — a `Ctrl-b` on one client can never be completed by another's keypress — but the rendered frame is shared, the same way an open context menu or a drag selection is visible to everyone attached.

---

## Agent notifications & attention

The server watches each agent's status **transitions** and fires a native desktop toast when an agent finishes a turn or starts awaiting input — **even while you're detached**.

- The `⚑N` indicator counts agents needing attention.
- `Ctrl-b !` jumps to a blocked agent.
- `Ctrl-b a` opens a **notification center** listing logged turn events (jump / dismiss).

Disable toasts with `notify = false` in `mux.toml`, or `COPAD_MUX_NOTIFY=0` (the env var wins).

### Clicking a toast

The toast carries a **click action**: clicking it switches to the pane that raised it — its session, its tab, that pane — and brings the terminal window to the front. This works on Linux (dunst / libnotify actions) and macOS (`terminal-notifier`), and from any session, not just the one you're looking at.

The click runs `comux jump <pane-token>`, which you can also run yourself:

```bash
comux jump "$COPAD_MUX_PANE"   # from inside a pane
comux list --json              # prints every pane's token
comux jump <token> --no-raise  # switch, but don't touch window focus
```

Two caveats, both deliberate:

- **Tokens don't survive a server restart.** They're qualified by server incarnation, so clicking a toast left over from a previous server says `unknown pane` instead of jumping to whatever pane inherited that slot.
- **Raising is application-level.** A pid identifies your terminal emulator, not which of its windows or native tabs holds the client — with several windows open, the one your WM considers current comes forward.

On macOS the first click may ask for Automation permission (the raise is a `System Events` call).

### Raising one from an agent hook

An agent's own hooks know the exact moment a turn ended and the real message; comux's sweep can only infer a transition at ~2 Hz. A hook running inside a pane can push directly:

```bash
comux notify --pane "$COPAD_MUX_PANE" --kind blocked "waiting on a permission prompt"
comux notify --kind done "turn finished"        # --pane defaults to $COPAD_MUX_PANE
```

`--kind` is `done` or `blocked`. Once a pane's agent has pushed a kind, the status sweep stops toasting that kind for it, so the two don't double-fire. Ownership is per `(pane, agent pid, kind)`: a hook that only reports `done` still gets sweep-detected `blocked` toasts, and a new agent in the same pane starts fresh rather than inheriting its predecessor's silence. The one case that can still double is the very first event of an agent's life, if the sweep notices the transition before the hook's push arrives.

If neither `--pane` nor `$COPAD_MUX_PANE` is set, `comux notify` is a usage error — it never guesses the active pane, because a misattributed toast sends its click somewhere misleading.

> After upgrading the comux binary, run `comux server restart` before these verbs work: the *running* server is what handles them.

---

## The usage / limits readout

The most configurable part of the status bar. It shows your **subscription rate-limit utilization** — how much of each rolling window you've used — for Claude (5h + weekly) and Codex (weekly). It's resolved in-process by a poller that refreshes every 60s, and shows only when the terminal is at least 100 columns wide.

> This readout uses the shared `copad-usage` crate, which reads the same data as `coctl usage --limits`. If you installed comux standalone, that's built in — no separate `coctl` needed for the readout itself. Set `COPAD_MUX_USAGE=0` (or `usage = off`) to disable the poller entirely.

There are three orthogonal knobs.

### 1. Gauge style — `usage`

| Value | Effect |
| --- | --- |
| `bar` (default) | A progress bar per window (`5h ━━━╌╌╌╌╌ 34%`) when wide enough, else percentages |
| `text` | Always percentages |
| `off` | Hide the readout (same as `COPAD_MUX_USAGE=0`) |

Each window's gauge is **threshold-colored**: green below 70%, yellow 70–90%, red above 90%. Labels, separators, and reset times stay muted. A token-lapsed / cached Claude value is prefixed with `~`.

### 2. Layout — `usage_layout`

| Value | Effect |
| --- | --- |
| `paged` (default) | A **carousel** showing one window (or provider) at a time **with a reset countdown** and an `n/N` page indicator. Its width is padded to the widest page so tab chips don't shift as you scroll. This is the only layout that shows resets. |
| `inline` | The legacy single row of every window: `claude 5h 6% wk 27% · codex wk 45%` (no resets) |

In the paged layout: **wheel over the readout pages it**, a **left-click advances** (wrapping), and a **right-click is inert**.

### 3. Page granularity — `usage_page_unit` (paged layout only)

| Value | Pages |
| --- | --- |
| `window` (default) | One window per page — 3 pages |
| `provider` | All of a provider's windows on one page — 2 pages |
| `metric` | A metric split: page 1 = every window's usage %, page 2 = every window's reset time (cross-provider entries render inline, e.g. `claude wk 4% · codex wk 1%`) |

### Reset display — `usage_reset`

| Value | Effect |
| --- | --- |
| `relative` (default) | `⟳ 2h13m` countdown |
| `absolute` | A clock time — `⟳ 14:32` / `⟳ Wed 09:00` |
| `off` | Hide reset times |

### Auto-rotate & width

- `usage_rotate_secs` (default `0` = off) auto-advances the carousel every N seconds. A manual wheel/click resets the timer. It's driven by the server's render loop, so it keeps rotating even while you're detached.
- `usage_bar_width` (default `8`) sets the cells per progress bar when `usage = "bar"`.
- `usage_windows` picks *which* windows appear — a list of `claude-5h`, `claude-wk`, `codex-wk` (aliases: `claude` = both Claude windows, `codex` = Codex). Omit it for all three; `[]` hides the readout.

### Example config

```toml
# A compact, auto-rotating carousel that pages by provider and shows absolute resets
usage             = "bar"
usage_layout      = "paged"
usage_page_unit   = "provider"
usage_reset       = "absolute"
usage_rotate_secs = 8
usage_bar_width   = 10
```

---

## The update hint

A background thread checks the GitHub `releases/latest` tag every 6 hours (in-process). When the latest release is semver-newer than your running build, a `⬆ x.y.z` indicator appears — the nudge to update for `install-comux.sh` users. Disable it with `update_check = false` or `COPAD_MUX_UPDATE_CHECK=0`.

---

## Tab chip labels — `tab_labels`

Controls what each tab chip shows:

| Value | Shows |
| --- | --- |
| `number` (default) | The tab index |
| `name` | The process / command name |
| `both` | `number:name` (a custom [tab name](./sessions-tabs-panes.md#naming-tabs) always wins) |

All of the above are **live-reloadable** — edit `mux.toml` and run `comux reload`.
