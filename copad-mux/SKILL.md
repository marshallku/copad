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

### How panes are addressed

- **By token** — the `token` field from `comux list --json`, which is that pane's own
  `$COPAD_MUX_PANE`. It names exactly one pane, anywhere in the mux, for as long as the
  server lives. Every verb you need takes one: `send`, `capture-pane`, `wait-output`,
  `wait-agent`, `split --from`, `jump`, `notify`.
- **By index** — a position in whatever tab the SERVER considers active, i.e. the tab the
  *user* is looking at. `send` still accepts one, and `focus`/`close`/`resize` take only one.

**Use tokens. Always.** An index is not a stable address: you keep running when the user
switches tabs, so between one command and the next, index 2 can become a pane in a
completely different tab, and nothing warns you. There is no way to make an index-addressed
write race-free — only to narrow the window.

Do not record a pane's `id` (`term0`, `term1`, …) instead of its token. Those restart from
zero every time the server does, so a saved one can address an unrelated pane later. `send`
rejects them for that reason.

And never send to your own pane, `$COPAD_MUX_PANE` — you would be typing into yourself.

## Run something in another pane

```bash
comux split --from "$COPAD_MUX_PANE"
```

That splits **your** pane — not "the focused one", which the user can change at any moment —
and prints the new pane's token. `--json` gives you the same thing as `.pane`. If it fails,
or the token comes back empty, stop: do not guess which pane appeared by diffing listings,
because a concurrent focus or tab change defeats that.

The new pane inherits the cwd of the pane it was split from, which is why splitting *your*
pane matters: split something else and the command runs in a directory you did not choose.

```bash
comux send <new-token> "make build"
comux send <new-token> $'\n'
```

`send` writes raw bytes: it does not escape anything and it does not press Enter for you.

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

So for "run a command and wait for it to finish":

```bash
pane=$(comux split --from "$COPAD_MUX_PANE")
id=$(date +%s%N)
comux send "$pane" "make build; printf 'DONE-%s\n' '$id'"
comux send "$pane" $'\n'
comux wait-output "$pane" "DONE-$id" --timeout 600
```

Everything addresses the same token, so nothing the user does to their tabs can retarget it.

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

`idle` means comux recognised no agent UI at all — "no reading", not a state — which is why
it is not waitable. A pane running a tool whose UI comux does not know stays `idle` forever,
so never block on one; poll it with `capture-pane`, or have it signal you with
`comux notify`.

## Coordinate with another agent

```bash
comux list-agents --json                   # token, tool, status, for_secs — across all sessions
comux wait-agent <token> --status blocked  # it is probably asking something
comux capture-pane <token> -S 80           # read what it asked
comux send <token> "use the schema in ./docs"
comux send <token> $'\n'
```

All of it is token-addressed, so it reaches an agent in any session without switching the
user's view. **That also means the user may not see it happen** — if what you are sending is
consequential, tell them rather than assuming they are watching that pane.

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

It prints where the notification was attributed (`claude · backend/review` — tool, space and
tab). Check it: that line is the only non-transient evidence you addressed the right pane.

## Do not

- **Do not run any of this when `$COPAD_MUX` is unset.** See the top.
- **Do not `send` to your own pane** (`$COPAD_MUX_PANE`).
- **Do not `kill-session`, `close`, `close-tab` or `kill-server`** unless the user asked in
  this conversation. They destroy panes you did not create and cannot be undone.
- **Do not `jump`, `select-session` or `select-tab`** without asking. They move what the
  user is looking at, and token addressing means you almost never need to.
- **Do not poll in a shell loop.** `wait-output` / `wait-agent` already poll, under one
  deadline, with a floor on the interval.
- **Do not treat a `124` as success.** Report the timeout.
