#!/usr/bin/env bash
# Linux e2e for copad's native comux sidebar (decisions #169): headless GTK (gtk4-broadwayd,
# so nothing appears on the live desktop), an isolated comux (own socket + state), a REAL
# copad-linux running a REAL comux client in a tab. Checks the claim, that comux stops drawing
# its TUI sidebar in that tab, and that quitting copad gives it back.
#
#   scripts/e2e-copad-comux-linux.sh                 # after cargo build -p copad-linux -p copad-mux -p copad-cli
#   SHOT_WAIT=25 scripts/e2e-copad-comux-linux.sh    # hold the window for a screenshot of
#                                                    # http://127.0.0.1:8097/ (the broadway page)
#   OVERLAY_DRIVER=1 …   # also open comux's rename prompt (Ctrl-b $) and wait up to 90s for
#                        # something to type "linuxrenamed" + Enter into the native dialog on
#                        # the broadway page (the Mac drives it with tabd through an ssh tunnel);
#                        # then Ctrl-b C (BROWSER_OPEN): the driver types "proj" Enter, "create"
#                        # Enter, "browsed" Enter, and a session must appear in ~/proj
set -euo pipefail
R="$(cd "$(dirname "$0")/.." && pwd)"
COMUX=$R/target/debug/comux; COCTL=$R/target/debug/coctl; COPAD=$R/target/debug/copad
W=$(mktemp -d /tmp/cml.XXXX)
BPID=""; CPID=""
cleanup() {
  # Let copad close its window (which removes its socket) BEFORE its display goes away: killed
  # together with broadwayd it dies mid-shutdown and leaves a dead gui-<pid>.sock every run.
  if [[ -n "${APP:-}" ]]; then
    kill "$APP" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$APP" 2>/dev/null || break; sleep 0.1; done
    rm -f "$XDG_RUNTIME_DIR/copad/gui-$APP.sock" "/tmp/copad-prewarm-$APP.html"
  fi
  [[ -n "$CPID" ]] && kill "$CPID" 2>/dev/null || true
  [[ -n "${APID:-}" ]] && kill "$APID" 2>/dev/null || true
  [[ -n "$BPID" ]] && kill "$BPID" 2>/dev/null || true
  COPAD_MUX_SOCK=$W/s $COMUX kill-server >/dev/null 2>&1 || true
  # The session bus's activated services (portal, at-spi) outlive dbus-run-session and keep
  # copad's log open; one that ran away once kept a deleted 12 GB log alive in /tmp. Anything
  # still holding a file under $W is this run's.
  sleep 0.5
  for f in /proc/[0-9]*/fd/*; do
    case "$(readlink "$f" 2>/dev/null)" in "$W"/*) p=${f#/proc/}; echo "${p%%/*}" ;; esac
  done 2>/dev/null | sort -u | xargs -r kill 2>/dev/null || true
  [[ "${KEEP:-}" == 1 ]] || rm -rf "$W"
}
trap cleanup EXIT
fail() { echo "FAIL: $*"; exit 1; }
export COPAD_MUX_SOCK=$W/s COPAD_MUX_STATE=$W/st.json COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 \
       COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID
# The server's HOME is a fixture: Ctrl-b C browses it (decisions #172).
mkdir -p $W/home/proj/sub
# Panes the server spawns run under a stand-in shell that reads no profile: a login zsh with
# this empty HOME took ~40 s to start on thinkarch (the work-graph step spawns one). `-i` keeps
# job control on, so a spawned program is the pane's FOREGROUND process (what the label sweep
# reads), as in a real interactive shell.
mkdir -p $W/fastshell
printf '#!/bin/sh\nwhile [ $# -gt 0 ] && [ "$1" != "-c" ]; do shift; done\nif [ "$1" = "-c" ]; then shift; script="$1"; shift; exec /bin/sh -i -c "$script" "$@"; fi\nexec /bin/sh -i\n' >$W/fastshell/sh
chmod +x $W/fastshell/sh
HOME=$W/home SHELL=$W/fastshell/sh $COMUX server >$W/server.log 2>&1 &
for _ in $(seq 1 50); do $COMUX list-sessions >/dev/null 2>&1 && break; sleep 0.1; done
$COMUX new-session --detached linux-two >/dev/null

clients() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(json.dumps(json.loads(f.readline())["topics"]["clients"]["data"]))
PY
}

gtk4-broadwayd :17 >$W/broadway.log 2>&1 & BPID=$!
sleep 1
# Its own state dir: copad restores and saves ~/.local/state/copad/session.json, which belongs
# to the copad the user is running.
XDG_STATE_HOME=$W/state XDG_CONFIG_HOME=$W/cfg GDK_BACKEND=broadway BROADWAY_DISPLAY=:17 dbus-run-session -- $COPAD >$W/copad.log 2>&1 &
CPID=$!
GUI=""
for _ in $(seq 1 100); do
  for p in $(pgrep -x copad); do
    if tr '\0' '\n' </proc/$p/environ 2>/dev/null | grep -qx "COPAD_MUX_SOCK=$W/s" && [ -S "$XDG_RUNTIME_DIR/copad/gui-$p.sock" ]; then
      GUI=$XDG_RUNTIME_DIR/copad/gui-$p.sock; APP=$p; break 2
    fi
  done
  sleep 0.2
done
[[ -n "$GUI" ]] || { tail -20 $W/copad.log; fail "dev copad did not open its socket"; }
echo "gui $GUI"
sleep 3
$COCTL --socket "$GUI" terminal exec "env COPAD_MUX_SOCK=$W/s COPAD_MUX_QUIET_SSH=1 $COMUX" >/dev/null
ok=""
for _ in $(seq 1 60); do
  c=$(clients)
  python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if any(x["tag"].startswith("copad:") and {"sidebar","status","overlays"} <= set(x["claimed"]) for x in c["clients"]) and {"sidebar","status"} <= set(c["hidden"]) else 1)' "$c" && { ok=1; break; }
  sleep 0.5
done
[[ -n "$ok" ]] || fail "copad-linux never claimed the sidebar: $(clients)"
echo "ok — copad-linux found its tab's comux by tag and claimed sidebar, status and overlays"
SCREEN=$($COCTL --socket "$GUI" terminal read 2>/dev/null || true)
grep -q "spaces" <<<"$SCREEN" && fail "TUI sidebar still drawn: $SCREEN"
echo "ok — comux in the copad tab no longer draws its sidebar"
$COMUX new-session --detached linux-three >/dev/null
if [[ -n "${OVERLAY_DRIVER:-}" ]]; then
  names() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(" ".join(x["name"] for x in json.loads(f.readline())["topics"]["tree"]["data"]["sessions"]))
PY
  }
  $COCTL --socket "$GUI" terminal feed $'\x02$' >/dev/null
  echo "OVERLAY_OPEN"
  for _ in $(seq 1 180); do names | grep -q linuxrenamed && break; sleep 0.5; done
  names | grep -q linuxrenamed || fail "the native rename dialog never renamed the session: $(names)"
  SCREEN=$($COCTL --socket "$GUI" terminal read 2>/dev/null || true)
  grep -q "rename session" <<<"$SCREEN" && fail "the TUI drew the prompt as well"
  echo "ok — Ctrl-b \$ became a native GTK dialog and its answer renamed the session"
  dirs() { python3 - <<'PY'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n'); f = s.makefile(); f.readline()
print(" ".join(x["name"] + "=" + x["dir"] for x in json.loads(f.readline())["topics"]["tree"]["data"]["sessions"]))
PY
  }
  $COCTL --socket "$GUI" terminal feed $'\x02C' >/dev/null
  echo "BROWSER_OPEN"
  for _ in $(seq 1 180); do dirs | grep -q "browsed=.*/proj" && break; sleep 0.5; done
  dirs | grep -q "browsed=.*/proj" || fail "the native browser never created a session in proj: $(dirs)"
  echo "ok — Ctrl-b C was a native GTK browser: descending into proj and creating there made the session"
fi
# The work graph (decisions #204): a (fake) claude agent's transcript, drawn in the right panel
# (a WebKitGTK page: under Broadway it never draws a frame, and its state is a function of time
# for exactly that reason).
graph() {
  local p="${1:-}"; [[ -n "$p" ]] || p='{}'
  $COCTL --socket "$GUI" call comux.graph --params "$p" --json
}
gq() { graph "${2:-}" | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d.get("result",d); print(eval(sys.argv[1]))' "$1" 2>/dev/null; }
gwait() {
  local got=""
  for _ in $(seq 1 "$3"); do got="$(gq "$1")"; [[ "$got" == "$2" ]] && return 0; sleep 0.5; done
  fail "graph panel: expected $1 == $2, got '$got': $(graph)"
}
FID="0f0f0f0f-1234-4abc-8def-0123456789ab"
mkdir -p $W/bin $W/home/.claude/sessions $W/home/.claude/projects/e2e/$FID/subagents
ln -sf "$(command -v sleep)" $W/bin/claude
TP=$W/home/.claude/projects/e2e/$FID.jsonl
use() { printf '{"type":"assistant","timestamp":"2026-11-04T00:00:%02d.000Z","message":{"content":[{"type":"tool_use","id":"%s","name":"%s","input":%s}]}}\n' "$4" "$1" "$2" "$3" >>"$TP"; }
res() { printf '{"type":"user","timestamp":"2026-11-04T00:00:%02d.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"%s","is_error":%s}]}}\n' "$3" "$1" "$2" >>"$TP"; }
printf '{"type":"user","timestamp":"2026-11-04T00:00:00.000Z","message":{"content":"Fix the parser"}}\n' >"$TP"
for i in $(seq 1 30); do use "r$i" Read "{\"file_path\":\"/repo/src/m$i.rs\"}" 1; res "r$i" false 1; done
use g1 Edit '{"file_path":"/repo/src/parse.rs"}' 2; res g1 false 3
use g2 Grep '{"pattern":"fn parse","path":"/repo/src"}' 4; res g2 false 4
use g3 Agent '{"description":"Map the tests","subagent_type":"Explore"}' 5
printf '{"agentType":"Explore","description":"Map the tests","toolUseId":"g3"}' >$W/home/.claude/projects/e2e/$FID/subagents/agent-m1.meta.json
printf '{"type":"assistant","isSidechain":true,"timestamp":"2026-11-04T00:00:06.000Z","message":{"content":[{"type":"tool_use","id":"s1","name":"Read","input":{"file_path":"/repo/tests/t.rs"}}]}}\n' \
  >$W/home/.claude/projects/e2e/$FID/subagents/agent-m1.jsonl
$COMUX spawn --keep -- $W/bin/claude 600 >/dev/null || fail "spawning the fake agent failed"
APID=""
# The pane's login shell starts first (an interactive zsh can take seconds).
for _ in $(seq 1 300); do
  # Through a file: awk exiting early would SIGPIPE ps, and pipefail + set -e end the run.
  ps -Ao pid=,args= >$W/ps.txt
  APID="$(awk -v p="$W/bin/claude" '$2==p {print $1; exit}' $W/ps.txt)"
  [[ -n "$APID" ]] && break; sleep 0.1
done
[[ -n "$APID" ]] || fail "no fake agent pid"
printf '{"pid":%s,"sessionId":"%s","status":"busy"}\n' "$APID" "$FID" >$W/home/.claude/sessions/$APID.json
[[ "$(gq 'd["dock_width"]')" == 0 ]] || fail "the never-opened panel takes room: $(graph)"
[[ "$(gq 'd["open"]' '{"open":true,"mode":"agent"}')" == True ]] || fail "comux.graph would not open: $(graph)"
gwait 'd["shown"] and d["loaded"]' True 40
gwait '(d["page"] or {}).get("agents")' 1 60
gwait '(d["page"]["subagents"], d["page"]["summary"].split(" · ")[0])' "(1, '33 files')" 60
gwait 'd["page"]["steps"]' 35 60
echo "ok — the right panel shows the agent's 33 files, its Explore subagent and all 35 steps (paged)"
use g4 Write '{"file_path":"/repo/src/new.rs"}' 7; res g4 false 7
gwait 'd["page"]["seq"]' 36 40
echo "ok — a step appended to the transcript reached the panel live (#36)"
# WebKit's own render of the panel (Broadway's screenshots of a web view are not trustworthy).
[[ "$(gq 'd["snapshot"]' '{"snapshot":"'"$W"'/graph-webkit.png"}')" == True ]] || fail "no WebKit snapshot"
[[ -s "$W/graph-webkit.png.canvas.png" ]] || fail "no canvas render of the graph"
echo "ok — WebKit rendered the panel: $W/graph-webkit.png (+ the graph canvas: .canvas.png)"
[[ "$(gq 'd["mode"]' '{"open":true,"mode":"all"}')" == all ]] || fail "mode all"
gwait '(d["page"]["mode"], d["page"]["agents"])' "('all', 1)" 20
if [[ -n "${SHOT_WAIT:-}" ]]; then echo "GRAPH_SHOT"; sleep "$SHOT_WAIT"; fi
# Measured right before closing: a broadway client (a screenshot) can resize the window, and a
# window too narrow for the dock hides it (the width budget) — then there is nothing to give back.
TOPW="$(gq 'd["terminal_width"]')"
WAS_SHOWN="$(gq 'd["shown"]')"
[[ "$(gq 'd["shown"]' '{"open":false}')" == False ]] || fail "the panel should close"
sleep 1
# A closed revealer used to keep ~12 px beside the terminal.
[[ "$(gq 'd["dock_width"]')" == 0 ]] || fail "the closed panel still takes room: $(graph)"
if [[ "$WAS_SHOWN" == True ]]; then
  [[ "$(gq 'd["terminal_width"] > '"$TOPW")" == True ]] || fail "closing the panel should give the terminal its width back: $(graph)"
fi
echo "ok — all-sessions view lists the agent; closing the panel gives the terminal back its width"
# A remembered panel wider than the window can spare: the terminal keeps its minimum.
graph '{"open":true,"mode":"agent","width":5000}' >/dev/null
sleep 1
[[ "$(gq 'd["terminal_width"] >= 320 and d["shown"]')" == True ]] || fail "the terminal must keep its minimum: $(graph)"
echo "ok — with a remembered 5000 px panel the terminal kept $(gq 'd["terminal_width"]') px"
graph '{"open":false}' >/dev/null
$COMUX notify --pane "$($COMUX list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["panes"][0]["token"])')" --kind done "graph e2e" >/dev/null
sleep 1.5
N="$($COCTL --socket "$GUI" call comux.notifications --params '{"open":true}' --json)"
python3 -c 'import json,sys; d=json.loads(sys.argv[1]); d=d.get("result",d); sys.exit(0 if d["shown"] and d["halign_end"] and d["valign_start"] else 1)' "$N" \
  || fail "the bell's dropdown did not open under the top bar: $N"
if [[ -n "${SHOT_WAIT:-}" ]]; then echo "NOTIF_SHOT"; sleep "$SHOT_WAIT"; fi
$COCTL --socket "$GUI" call comux.notifications --params '{"open":false}' --json >/dev/null
echo "ok — the notifications drop down under the top bar's bell"
kill "$APID" 2>/dev/null || true

sleep 2
echo "BROADWAY http://127.0.0.1:8097/  W=$W"
if [[ -n "${SHOT_WAIT:-}" ]]; then sleep "$SHOT_WAIT"; fi
# Quit copad and let it finish (window destroy removes its socket) before its session bus goes.
kill "$APP"
for _ in $(seq 1 50); do kill -0 "$APP" 2>/dev/null || break; sleep 0.1; done
kill "$CPID" 2>/dev/null || true; CPID=""
for _ in $(seq 1 40); do
  python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if "sidebar" not in c["hidden"] else 1)' "$(clients)" && break
  sleep 0.5
done
python3 -c 'import json,sys; c=json.loads(sys.argv[1]); sys.exit(0 if "sidebar" not in c["hidden"] else 1)' "$(clients)" || fail "claim survived copad exit"
echo "ok — quitting copad released the claim"
echo "PASS: copad-linux native comux sidebar"
