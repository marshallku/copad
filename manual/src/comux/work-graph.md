# The Work Graph

comux keeps a **work graph** for every Claude Code and Codex agent running in a pane: the files
it read, edited, wrote and searched, the commands it ran, the prompts it was given, and the
subagents it started — in order. copad draws it as a live, animated graph next to the terminal;
anywhere else you can read it with `comux graph`.

## In copad

When a copad window hosts a comux client, the native chrome has a **top bar** over the
terminal:

- on the left, where you are: session › tab › the focused agent and what it is doing now;
- on the right, three buttons: **All sessions**, **Work graph**, and the **bell** (the
  notification list — it used to be opened from the sidebar — now drops down under it).

**Work graph** (`Cmd+Shift+G` on macOS, `Ctrl+Shift+G` on Linux) opens the graph in a panel on
the right side of the window. Drag its left edge to resize it; whether it is open, which view it
shows and its width are remembered. The panel never squeezes the terminal below 40 columns — in
a narrow window it gives way, and comes back when the window grows. On iOS the graph button sits
beside the bell and opens the graph in a sheet.

### Agent view

The agent sits in the middle with its subagents around it. Files hang off their directories;
a file's size grows with how often it was touched and its colour is the last thing done to it
(read blue, edit peach, write green, search mauve). A ring marks a file that was edited; a
turning dashed ring marks a call still waiting for its result, or a subagent still running.

Each new step **flares** its file and sends a beam from whoever did it — the agent or a
subagent. Under the graph:

- the step's card: what it did, to what, by whom, and when (`✕` = it failed);
- the **timeline**: every step as a tick (prompts are the tall ones). Drag on it to scrub; the
  files not yet touched at that point disappear, so you can watch the work grow;
- **▶** replays from the start; **Live** follows the newest step again;
- a summary: files, reads, edits, searches, subagents, failures.

Hover a node for its details. Click a file to put the cursor on its latest step and mark its
steps on the timeline; double-click it to open it (local comux only). Click the agent to jump to
its pane. Keys: `←`/`→` step, `Space` play/pause, `End` live, `f` fit the view.

The panel follows the agent you are looking at; pick another one in its menu and it stays on
that one for 30 seconds before following you again.

### All sessions

Every live agent at once, grouped by session, each with the files it touched most recently. A
file that two agents touched is **one** node linked to both, with a yellow ring — the quick way
to see agents working on the same thing. Click an agent to jump to it.

## From the command line

```bash
comux graph                    # every live agent: totals, its most touched files, its subagents
comux graph <pane>             # one agent's steps, newest last
comux graph <pane> --json --limit 50 --before 120   # a page of history, as JSON
```

## Where it is kept

Graphs are read from each agent's own transcript (`~/.claude/projects/…` for Claude, the
rollout file for Codex) and saved under comux's state directory, in `graphs/`
(`~/.local/state/copad/graphs/<tool>-<conversation>.json`, readable by you only). A saved graph
survives a server restart and carries on when the conversation is resumed. Graphs older than
`graph_retain_days` are deleted.

Opt out in `mux.toml` (both apply on `comux reload`):

| Key | Default | Effect |
| --- | --- | --- |
| `graph` | `true` | `false` reads no transcripts and shows no graphs |
| `graph_persist` | `true` | `false` keeps graphs in memory only; files already saved are left alone (delete `graphs/` yourself) |
| `graph_retain_days` | `30` | Saved graphs older than this are deleted |

## What it does not see

- Edits an agent makes through shell commands (`sed -i`, `cat >`), and Codex patches applied
  from inside a script — only Claude's edit tools and Codex's `apply_patch` tool count.
- Codex subagents (Claude subagents are shown).
- Agents other than Claude Code and Codex.

It lags the agent by about a second, and a step counts only once its result is in: a read that
failed is shown as failed, not as a read.
