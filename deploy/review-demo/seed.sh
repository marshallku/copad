#!/bin/sh
# As `review`, on every container start: a fresh comux with a small layout to look at.
set -eu
. /usr/local/lib/review-demo/env.sh
# Panes run $SHELL; `su -s /bin/sh` left it at sh.
export SHELL=/bin/bash
cd "$HOME"
# A reset: nothing a reviewer left behind survives a restart.
find "$HOME" -mindepth 1 -delete
find /tmp -mindepth 1 -user review -delete 2>/dev/null || true
cp -R /usr/local/lib/review-demo/home/. "$HOME/"
mkdir -p "$HOME/.comux" "$HOME/bin"
ln -sf /bin/sleep "$HOME/bin/claude"
cd "$HOME/projects/hello"
git init -q -b main
git add -A
git -c user.name=demo -c user.email=demo@localhost commit -qm "Start hello"
comux server start >/dev/null
for _ in $(seq 1 50); do comux list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
comux rename-session 0 hello >/dev/null
first="$(comux list-tree --json | grep -o '"token":"[^"]*"' | head -1 | cut -d'"' -f4)"
# A pretend agent (a renamed `sleep`): comux lists it under agents.
comux split --from "$first" -- "$HOME/bin/claude" 86400 >/dev/null
# The split focused the agent; typing belongs in the shell.
comux jump "$first" --no-raise >/dev/null
comux new-session --detached --cwd "$HOME/projects/notes" notes >/dev/null
