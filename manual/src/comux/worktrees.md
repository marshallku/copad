# Git Worktrees

comux treats git worktrees as a first-class workflow: one command creates a worktree, runs a per-repo setup hook, and drops you into a fresh session inside it. It's a port of the owner's `tmx twt` workflow.

## Create a worktree

```bash
cd ~/dev/copad
comux worktree create feat/login
```

This:

1. Runs `git worktree add -b feat/login`, placing the new worktree as a **sibling** of the repo's main worktree, named per the `[worktree] naming` pattern (default `{repo}-{branch}` → `~/dev/copad-feat-login`; `/` in the branch becomes `-`).
2. Runs the configured **post-create hook** for that repo (see below).
3. Opens a new session in the worktree and switches to it.

Inside the TUI, `Ctrl-b W` does the same thing with an inline branch-name prompt.

### Options

| Flag | Effect |
| --- | --- |
| `--from <ref>` | Base the new branch on `<ref>` instead of `HEAD` |
| `--no-attach` / `--keep-current` | Create the session but stay in your current shell |
| `--json` | Emit the raw JSON result (implies no attach) |

`comux worktree create` **starts the server if none is running**.

### Attach behavior

- From a **plain shell** outside comux, `worktree create` attaches you into the new session and blocks until you detach — like `tmux attach-session`.
- From **inside a comux pane**, the attached view just follows the switch (no nested client). comux detects "inside comux" via the `COPAD_MUX=1` marker it injects into every pane.
- `--no-attach`, `--keep-current`, and `--json` all suppress the attach.

---

## The post-create hook

Configure a per-repo setup command in `mux.toml`. The key is the repo's **main-worktree path**; the value is a shell command:

```toml
[worktree]
naming = "{repo}-{branch}"       # {repo} = main worktree dir name, {branch} with / → -

[worktree.scripts]
"~/dev/copad"   = "mise trust && cargo fetch"
"~/dev/web-app" = "mise trust && yarn"
```

The command runs via `bash -c` **in the new worktree**, with `$WORKTREE_PATH` exported. Keys are `~`-expanded and canonicalized; if two keys canonicalize to the same path, the last one wins (with a warning). This is where you install dependencies, trust a `mise`/`direnv` config, or seed local files so the new worktree is ready to work in immediately.

---

## List worktrees

```bash
comux worktree list
comux worktree list --plain     # just the paths
comux worktree list --json      # machine-readable
```

The output shows each worktree's path, branch, whether it's the main worktree, whether a **live comux session** is inside it, and lock state. `worktree list` works even with **no server running** (in which case nothing shows as live).

---

## Remove a worktree

```bash
comux worktree rm feat/login          # by branch
comux worktree rm ~/dev/copad-feat-login   # by path
comux worktree rm                     # no target → fuzzy-pick one
```

With no target it lists the repo's removable worktrees and lets you type to narrow them (`Enter` removes, `Esc` cancels). The main worktree, locked worktrees, and the one you're standing in are left out — they'd be refused anyway — and the header counts the locked/current ones it hid, so a missing entry is never a mystery. (The main worktree isn't counted; it's never a removal target.) A worktree with a live session **is** offered and marked `· live`; removing it still needs `-f`.

| Flag | Effect |
| --- | --- |
| `-f` / `--force` | Kill any live sessions inside the worktree first, then remove it |
| `-d` / `--delete-branch` | Also delete the branch |
| `--json` | Machine-readable result |

`worktree rm` refuses to remove the main worktree, a locked worktree, or the worktree you're currently in, and refuses `-d` on a detached HEAD. Without `--force` it also refuses to remove a worktree that has a live session inside it. Like `list`, it falls back to a pure-git path when no server is running (taking the server flock so it never leaves one behind).

---

## A complete example

```bash
cd ~/dev/copad
comux worktree create feat/status-bar --from main
# → creates ~/dev/copad-feat-status-bar
# → runs the "~/dev/copad" hook (mise trust && cargo fetch) inside it
# → opens + attaches a session there

# ... work on the feature, run agents, etc. ...

comux worktree list                    # confirm which worktrees are live
comux worktree rm feat/status-bar -f -d   # done: kill the session, remove worktree + branch
```
