#!/usr/bin/env bash
# Find, and optionally stop, comux servers that are NOT the user's default one.
#
# A server outlives its launching terminal and exits only when its last pane's shell exits,
# so one started on a throwaway socket (an e2e run, a manual `COPAD_MUX_SOCK=... comux
# server`) survives its test, forever if the shell is an interactive `sh -l`.
#
# The default server is identified via `comux doctor --json` and is NEVER touched.
#
# Usage:
#   scripts/comux-reap-orphans.sh          # list orphans (dry run)
#   scripts/comux-reap-orphans.sh --kill   # stop them and clean their runtime dirs

set -euo pipefail

KILL=0
case "${1:-}" in
    --kill) KILL=1 ;;
    "") ;;
    *)
        echo "usage: $(basename "$0") [--kill]" >&2
        exit 2
        ;;
esac

command -v lsof >/dev/null 2>&1 || {
    echo "error: lsof is required to map a server pid to its socket" >&2
    exit 2
}

COMUX="$(command -v comux || true)"
[[ -n "$COMUX" ]] || {
    REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    COMUX="$REPO/target/debug/comux"
}
[[ -x "$COMUX" ]] || {
    echo "error: no comux binary found (build it or put it on PATH)" >&2
    exit 2
}

# The default socket, from the binary that owns the path convention. Empty is fine: nothing
# is then exempt, and a dry run still shows what is out there.
DEFAULT_SOCK="$("$COMUX" doctor --json 2>/dev/null |
    python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for s in d.get("sections", []):
    if s.get("title") == "server" and s.get("path"):
        print(s["path"])
        break' || true)"

if [[ -n "$DEFAULT_SOCK" ]]; then
    echo "default server socket: $DEFAULT_SOCK"
else
    echo "warning: could not resolve the default socket — nothing will be exempt" >&2
    [[ "$KILL" -eq 1 ]] && {
        echo "refusing to --kill without knowing which server to spare" >&2
        exit 2
    }
fi

# The pid holding the default socket is what protects the user's server: one whose socket
# FILE was deleted resolves to no path and would slip past the path comparison.
DEFAULT_PID=""
if [[ -n "$DEFAULT_SOCK" && -S "$DEFAULT_SOCK" ]]; then
    DEFAULT_PID="$(lsof -t -- "$DEFAULT_SOCK" 2>/dev/null | head -1 || true)"
fi
if [[ -n "$DEFAULT_PID" ]]; then
    echo "default server pid:    $DEFAULT_PID"
else
    echo "default server pid:    (none running)"
fi

found=0
reaped=0

# Require basename `comux` with arguments exactly `server`: a command line merely containing
# those words (a wrapper `sh -c`, an editor, this script's shell) must not be sent SIGTERM.
is_comux_server() {
    local pid="$1" args comm
    args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    comm="$(ps -o comm= -p "$pid" 2>/dev/null || true)"
    [[ "${comm##*/}" == "comux" ]] || return 1
    read -r -a parts <<<"$args"
    [[ "${#parts[@]}" -eq 2 && "${parts[0]##*/}" == "comux" && "${parts[1]}" == "server" ]]
}

# Is `$1` still the discovered server, whose start time was `$2`? A pid is reusable the
# instant it is reaped, so existence alone never justifies a signal.
still_ours() {
    is_comux_server "$1" || return 1
    [[ "$(ps -o lstart= -p "$1" 2>/dev/null || true)" == "$2" ]]
}

while read -r pid; do
    [[ -n "$pid" ]] || continue
    [[ -n "$DEFAULT_PID" && "$pid" == "$DEFAULT_PID" ]] && continue
    is_comux_server "$pid" || continue
    # Select on lsof's TYPE field, not the file name: the socket is named by
    # `COPAD_MUX_SOCK` and need not end in "sock". It is the only socket the server holds
    # that has a filesystem path.
    sock="$(lsof -p "$pid" -Fnt 2>/dev/null |
        awk '/^t/ { type = substr($0, 2) }
             /^n\// { if (type == "unix") { print substr($0, 2); exit } }' || true)"
    # No resolvable socket path means the socket file was deleted out from under the server.
    # It is unreachable yet still holds its panes, but it looks the same as the user's real
    # server in that state (DEFAULT_PID is resolved from the missing file), so it is only
    # killed when the default server's pid is positively known; otherwise it is reported.
    socketless=0
    if [[ -z "$sock" ]]; then
        sock="(socket deleted)"
        socketless=1
    elif [[ "$sock" == "$DEFAULT_SOCK" ]]; then
        continue
    fi

    found=$((found + 1))
    etime="$(ps -o etime= -p "$pid" | tr -d ' ')"
    cputime="$(ps -o time= -p "$pid" | tr -d ' ')"
    # Start time recorded at discovery, so the re-check before `kill` can tell this process
    # from a new one that inherited its pid.
    started="$(ps -o lstart= -p "$pid" 2>/dev/null || true)"
    echo "orphan pid=$pid  up=$etime  cpu=$cputime  sock=$sock"

    [[ "$KILL" -eq 1 ]] || continue

    if [[ "$socketless" -eq 1 && -z "$DEFAULT_PID" ]]; then
        echo "  skipped: socket is gone and no running default server to tell it apart from" >&2
        echo "  (start your server, or kill pid $pid by hand once you have checked it)" >&2
        continue
    fi

    # kill-server does the final layout save and reaps the panes' shells, which a bare
    # SIGTERM would orphan. Unavailable for a socket-less server.
    if [[ "$sock" != "(socket deleted)" ]] &&
        COPAD_MUX_SOCK="$sock" "$COMUX" kill-server >/dev/null 2>&1; then
        sleep 1
    fi
    # A pid is reusable once reaped, so between the shutdown request and the signal it may
    # belong to something else: re-verify it is still the same comux server (same start
    # time) immediately before the kill.
    if kill -0 "$pid" 2>/dev/null; then
        if still_ours "$pid" "$started"; then
            kill "$pid" 2>/dev/null || true
            sleep 1
        else
            echo "  pid $pid is no longer that server (exited; pid reused) — not signalling"
        fi
    fi
    # Only the pid being gone proves the kill; an unconfirmed identity (transient `ps`
    # failure) must not count, because the cleanup below acts on it.
    if kill -0 "$pid" 2>/dev/null; then
        echo "  warning: pid $pid still alive after TERM — leaving it (nothing removed)" >&2
        continue
    fi
    reaped=$((reaped + 1))
    [[ "$socketless" -eq 1 ]] && continue

    # The socket and lock are deliberately NOT deleted. A clean shutdown already unlinks the
    # socket, and shell cannot check-then-unlink atomically (no `flock` on macOS): losing the
    # race with a replacement server would unlink its live endpoint or its LOCK inode, after
    # which two servers could own the same socket.
    if [[ -e "$sock" || -e "$sock.lock" ]]; then
        echo "  left $(dirname -- "$sock") in place (remove it yourself if nothing else needs it)"
    fi
    # Candidates come from `ps`, not `pgrep -f`: on this macOS `pgrep -f comux` misses the
    # long-running server entirely.
done < <(ps -Ao pid=,args= 2>/dev/null |
    awk '$2 ~ /(^|\/)comux$/ && $3 == "server" { print $1 }' || true)

if [[ "$found" -eq 0 ]]; then
    echo "no orphan comux servers"
elif [[ "$KILL" -eq 1 ]]; then
    echo "reaped $reaped of $found orphan server(s)"
else
    echo "$found orphan server(s) — re-run with --kill to stop them"
fi
