---
name: comux
description: Drive the comux terminal multiplexer from inside one of its panes — inspect sessions/tabs/panes, run commands in sibling panes, read their output back, and wait on them. Use when you are running inside comux and the task needs a second pane (a dev server, a build, another agent) rather than blocking your own.
---

# Operating comux from inside a pane

comux is a terminal multiplexer with a persistent server. You are reading this because you
may be running inside one of its panes.

## Before anything else: check you are inside comux

```bash
[ -n "$COPAD_MUX" ] || echo "not inside comux"
```

**If `$COPAD_MUX` is unset, stop — do not run any command below.** Without it, a `comux`
command would reach whatever server the user has running and mutate panes you do not own:
a `send` into an unrelated pane types into someone's live shell, and `kill-session` destroys
their work. Being inside comux is what makes these commands yours to run.

Your own pane is `$COPAD_MUX_PANE`. Never send input to it — you would be typing into
yourself.

## Look around

```bash
comux list-sessions           # every session (workspace)
comux list-tabs               # tabs in the active session
comux list                    # panes in the active tab: index, token, label, status
comux list-agents             # agent panes across EVERY session, with status and age
```

Add `--json` to any of them when you need to parse rather than read.

### How panes are addressed — read this before sending anything

- **By token** — the `token` field, which is that pane's own `$COPAD_MUX_PANE`. Resolves
  anywhere in the mux and stays valid. `capture-pane`, `wait-output`, `wait-agent`, `jump`
  and `notify` all take one.
- **By index** — the position printed by `comux list`. `send`, `focus`, `close` and `resize`
  take ONLY an index.

**An index is not a stable address.** It numbers the panes of whatever tab the SERVER
considers active — which is the tab the *user* is looking at, not "yours". You keep running
when they switch tabs, so between one command and the next, index 2 can become a pane in a
completely different tab. Nothing warns you.

So there are two things you cannot do, and pretending otherwise types into someone's live
shell:

- You **cannot type into a pane by token**. There is no token-addressed `send`.
- You **cannot make an index-addressed send safe** against the user switching tabs. You can
  only narrow the window.

### The check to run before any `send`

```bash
comux list --json
```

Two things must hold in that listing, and you must re-check them immediately before each
send — not once at the start:

1. **Your own `$COPAD_MUX_PANE` appears in it.** If it does not, the active tab is not the
   one you are in, every index refers to panes you know nothing about, and you must stop.
2. **`panes[<index>].token` is still the token you recorded** for your target — and is not
   your own.

If either fails, do not send. Re-list, or tell the user what you were about to do.

## Run something in another pane

```bash
comux list --json
```

Confirm your own token is in that listing (check 1 above), and **keep every
`panes[].token` from it**. Then:

```bash
comux split
comux list --json
```

Your sibling is the token in the second listing that was **not** in the first. If exactly
one token is new, that is yours. If none or several are, stop and say so — something else
changed the tab and you can no longer tell which pane you created.

**Do not take `panes[focused]`.** comux does focus the pane it just created, but the user
can move focus between your `split` and your `list`, and then "focused" is one of THEIR
panes. Nothing downstream catches it: their pane is genuinely live and in the active tab, so
your own-token check passes and your target-token check passes, and you type into their
shell.

Record your sibling's `index` for `send` and its `token` for every read and wait. Only the
token survives a tab change.

```bash
comux send <sibling-index> "make build"
comux send <sibling-index> $'\n'
```

Re-run the pre-send check immediately before each of those. `send` writes raw bytes: it does
not escape anything and it does not press Enter for you.

## Read a pane back

```bash
comux capture-pane <token>              # the pane's visible screen
comux capture-pane <token> -S 500       # 500 physical grid rows, reaching into scrollback
comux capture-pane <token> --json       # text + rows + truncated + the resolved pane
```

`-S` counts **physical grid rows, not logical lines** — a soft-wrapped line spans several
rows. Capture reads the **live screen bottom**, so it is unaffected by the user scrolling.
It is bounded: a very large request is cut and `truncated` says so.

## Wait for something

```bash
comux wait-output <token> "PATTERN" --timeout 120
comux wait-agent  <token> --status blocked --timeout 600
```

Exit codes: `0` matched · `124` timed out · `1` failed · `2` you used it wrong. **Check the
exit code** — a timeout is not a match.

### The one thing to get right about waiting

Both waits are **level-triggered and best-effort**: they return as soon as the condition is
CURRENTLY true, including when it was already true before you called. That makes them
race-free only if you give them something that cannot have been true before.

So for "run a command and wait for it to finish", using the sibling you created above —
never a hard-coded index, which on a single-pane tab is YOU:

```bash
comux list --json
```

Re-check that `panes[<sibling-index>].token` is still your sibling's, then:

```bash
id=$(date +%s%N)
comux send <sibling-index> "make build; printf 'DONE-%s\n' '$id'"
comux send <sibling-index> $'\n'
comux wait-output <sibling-token> "DONE-$id" --timeout 600
```

The sends take the index; the wait takes the token, so a tab change cannot make the wait
watch the wrong pane.

Two traps this recipe exists to dodge, both of which make a wait return instantly and
wrongly:

1. **The shell echoes the command you sent.** If the finished marker appears literally in
   the command line, it is on screen before the command runs. Assemble it from fragments —
   `printf 'DONE-%s\n' "$id"` — so the whole marker only ever appears in the OUTPUT.
2. **A marker from a previous run is still on screen.** Make it fresh per call.

`wait-output` can also MISS a match: `\r` overwrites, line erasure and alternate-screen
redraws can remove text between polls, and the alternate screen has no scrollback. Raising
`-S` does not fix that. If you need certainty, have the command write to a **file** and
check the file.

`wait-agent` is a **current-state** wait, not turn-completion detection. Right after you
send a prompt, the agent may still read `ready` from its PREVIOUS turn and the wait returns
at once. Its statuses are cached and inferred (Claude's from its session file, everything
else from screen text, refreshed every 500ms attached / 5s detached), so `blocked` can be a
false reading from ordinary output. Treat it as a hint, and confirm with `capture-pane`.

## Coordinate with another agent

Observing another agent works from anywhere, because reads take a token:

```bash
comux list-agents --json                   # token, tool, status, for_secs — across all sessions
comux wait-agent <token> --status blocked  # it is probably asking something
comux capture-pane <token> -S 80           # read what it asked
```

**Prompting one is different.** `send` is index-only and indexes are active-tab-only, so you
can only type into an agent that shares your tab:

```bash
comux list --json                          # is that token in THIS tab?
comux send <index> "review the diff in ~/work"
comux send <index> $'\n'
```

If it is not in your tab, the only way to reach it is `comux jump <token>`, which switches
the user's session, tab and focus and raises their window. **That is a visible change to
their workspace — ask first.** Otherwise, report what you observed and let the user act.

`wait-agent` addresses a **pane**, not a specific agent run: if the agent exits and another
starts in that pane, they are indistinguishable.

## Tell the user something finished

```bash
comux notify --pane "$COPAD_MUX_PANE" --kind done "tests passed"
comux notify --pane "$COPAD_MUX_PANE" --kind blocked "need a decision on the schema"
```

This raises a desktop toast whose click jumps back to your pane — it works while the user is
detached, which is the point. Always pass your own `$COPAD_MUX_PANE`; a misattributed
notification sends the user's click somewhere misleading.

## Do not

- **Do not run any of this when `$COPAD_MUX` is unset.** See the top.
- **Do not `send` to your own pane** (`$COPAD_MUX_PANE`).
- **Do not `kill-session`, `close`, `close-tab` or `kill-server`** unless the user asked in
  this conversation. They destroy panes you did not create and cannot be undone.
- **Do not `jump`, `select-session` or `select-tab`** to get at a pane without asking. They
  move what the user is looking at.
- **Do not poll in a shell loop.** `wait-output` / `wait-agent` already poll, under one
  deadline, with a floor on the interval.
- **Do not treat a `124` as success.** Report the timeout.
