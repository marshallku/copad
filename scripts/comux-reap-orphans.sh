#!/usr/bin/env bash
# Find — and optionally stop — comux servers that are NOT the user's default one.
#
# A comux server outlives the terminal that launched it by design, and it only exits
# when its last pane's shell exits. That combination means any server started on a
# throwaway socket (an e2e run, a manual `COPAD_MUX_SOCK=... comux server`) survives
# the test that spawned it and keeps running for as long as its shell does — which,
# for an interactive `sh -l` nobody will ever type `exit` into, is forever. Nine such
# servers accumulated over a single testing session before this script existed.
#
# The default server is identified via `comux doctor --json` (single source of truth
# for the socket path) and is NEVER touched.
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

# The default socket, straight from the binary that owns the path convention. An
# empty result (no server section / no doctor) is fine: nothing is then exempt, and
# a dry run still shows what is out there before anything is killed.
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

# The pid holding the default socket. This — not the socket path — is what actually
# protects the user's server, because a server whose socket FILE has been deleted no
# longer resolves to any path (see the socket-less case below) and would otherwise
# fall through the path comparison.
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

# Is this pid really a comux server, rather than something that merely MENTIONS one?
# Any command line containing the words would otherwise qualify — a wrapper
# (`sh -c '... comux server ...'`), an editor, or this very script's shell — and then be
# sent SIGTERM. Require the executable's basename to be exactly `comux` and its arguments
# to be exactly `server`, which is the real server's argv and nothing else's.
is_comux_server() {
    local pid="$1" args comm
    args="$(ps -o args= -p "$pid" 2>/dev/null || true)"
    comm="$(ps -o comm= -p "$pid" 2>/dev/null || true)"
    [[ "${comm##*/}" == "comux" ]] || return 1
    # Exactly two fields: the binary path and the literal `server`.
    read -r -a parts <<<"$args"
    [[ "${#parts[@]}" -eq 2 && "${parts[0]##*/}" == "comux" && "${parts[1]}" == "server" ]]
}

# Is `$1` still the same comux server we discovered, whose start time was `$2`? A pid is
# reusable the instant its process is reaped, so pid existence alone is never enough to
# justify a signal.
still_ours() {
    is_comux_server "$1" || return 1
    [[ "$(ps -o lstart= -p "$1" 2>/dev/null || true)" == "$2" ]]
}

while read -r pid; do
    [[ -n "$pid" ]] || continue
    [[ -n "$DEFAULT_PID" && "$pid" == "$DEFAULT_PID" ]] && continue
    is_comux_server "$pid" || continue
    # The server's listening unix socket is the only socket it holds that still has a
    # filesystem PATH — connected clients and the panes' socketpairs show as kernel
    # addresses. So select on lsof's TYPE field, not on the file name: the socket is
    # named by `COPAD_MUX_SOCK` and need not end in "sock" (`COPAD_MUX_SOCK=/tmp/x/s` is
    # perfectly legal, and a name-pattern filter silently misreports it as socket-less).
    sock="$(lsof -p "$pid" -Fnt 2>/dev/null |
        awk '/^t/ { type = substr($0, 2) }
             /^n\// { if (type == "unix") { print substr($0, 2); exit } }' || true)"
    # A server with no resolvable socket path had its socket file deleted out from under
    # it — the state left behind when someone removes a test's temp dir but not its
    # process. It is unreachable (no client can connect, `kill-server` has nowhere to
    # send) yet still holds its panes' shells and still sweeps, so it is the most
    # orphaned a server gets.
    #
    # But it is ALSO indistinguishable from the user's real server in that state: the
    # path comparison cannot save it, and `DEFAULT_PID` is resolved from the socket file
    # that is, by definition, missing. So a socket-less server is only ever KILLED when
    # the default server's pid is positively known (i.e. its socket does exist and this
    # is not it). Otherwise it is reported and left alone.
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
    # The process's start time, recorded at DISCOVERY. Together with the identity check
    # this is what distinguishes "still the process we looked at" from "a brand-new
    # process that inherited its pid" — see the re-verification before `kill` below.
    started="$(ps -o lstart= -p "$pid" 2>/dev/null || true)"
    echo "orphan pid=$pid  up=$etime  cpu=$cputime  sock=$sock"

    [[ "$KILL" -eq 1 ]] || continue

    if [[ "$socketless" -eq 1 && -z "$DEFAULT_PID" ]]; then
        echo "  skipped: socket is gone and no running default server to tell it apart from" >&2
        echo "  (start your server, or kill pid $pid by hand once you have checked it)" >&2
        continue
    fi

    # Ask it to shut down properly first: kill-server does the final layout save and
    # reaps the panes' shells, which a bare SIGTERM to the server would orphan. Not
    # possible for a socket-less server — signal is the only channel left.
    if [[ "$sock" != "(socket deleted)" ]] &&
        COPAD_MUX_SOCK="$sock" "$COMUX" kill-server >/dev/null 2>&1; then
        sleep 1
    fi
    # `kill-server` may well have succeeded, and a pid is reusable the moment its process
    # is reaped — so between the shutdown request above and the signal below, this pid can
    # already belong to something else entirely. Never signal on pid existence alone:
    # re-verify that it is STILL a comux server AND still the same one (same start time)
    # immediately before the kill. (Same reasoning as the delegate job manager, which
    # verifies liveness against a recorded process start time for exactly this reason.)
    if kill -0 "$pid" 2>/dev/null; then
        if still_ours "$pid" "$started"; then
            kill "$pid" 2>/dev/null || true
            sleep 1
        else
            echo "  pid $pid is no longer that server (exited; pid reused) — not signalling"
        fi
    fi
    # The ONLY proof the process is gone is that the pid no longer exists. Deliberately
    # not "we could not confirm its identity" — a transient `ps` failure must not be read
    # as a successful kill, because the cleanup below deletes files on the strength of it.
    if kill -0 "$pid" 2>/dev/null; then
        echo "  warning: pid $pid still alive after TERM — leaving it (nothing removed)" >&2
        continue
    fi
    reaped=$((reaped + 1))
    [[ "$socketless" -eq 1 ]] && continue

    # This script does NOT delete the server's socket or lock, on purpose.
    #
    # A clean shutdown already unlinks the socket itself, and what would be left is a
    # zero-byte lock file — worth almost nothing to remove. Against that: the path is no
    # longer ours the moment the old owner dies. A replacement server can bind it at any
    # time, and there is no way in portable shell to check-then-unlink atomically (no
    # `flock` on macOS), so any removal here races. Losing that race unlinks a live
    # server's endpoint — stranding it, reachable by nobody — or, worse, removes a
    # reclaimed LOCK inode, after which the flock that guarantees one server per path is
    # no longer mutually exclusive and two servers can own the same socket.
    #
    # Killing a runaway process is this script's job; reclaiming a few bytes of /tmp is
    # not worth being able to corrupt a healthy server. Report the path instead.
    if [[ -e "$sock" || -e "$sock.lock" ]]; then
        echo "  left $(dirname -- "$sock") in place (remove it yourself if nothing else needs it)"
    fi
    # Candidates come from `ps`, NOT `pgrep -f`. On this macOS, `pgrep -f comux` reports
    # only the client and cannot see the long-running server at all, even though
    # `ps -Ao args=` lists it as `/Users/…/comux server` — so pgrep-based discovery
    # silently misses the very processes this script exists to find. `ps` is also what
    # `is_comux_server` already reads, and behaves the same on Linux.
done < <(ps -Ao pid=,args= 2>/dev/null |
    awk '$2 ~ /(^|\/)comux$/ && $3 == "server" { print $1 }' || true)

if [[ "$found" -eq 0 ]]; then
    echo "no orphan comux servers"
elif [[ "$KILL" -eq 1 ]]; then
    echo "reaped $reaped of $found orphan server(s)"
else
    echo "$found orphan server(s) — re-run with --kill to stop them"
fi
