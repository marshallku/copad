#!/usr/bin/env bash
# End-to-end verification of the comux L0 host API (decisions #166) against a REAL headless
# server on a throwaway socket + state file — never the user's server.
#
# Steps:
#   1. subscribe: welcome (proto + incarnation) then ONE snapshot carrying every topic
#   2. a command over the subscription mutates and its reply + the new tree state arrive
#   3. a real client attached with a binding tag shows the TUI sidebar + status bar
#   4. host-ready for that attachment hides both from the rendered frame; panes take the space
#   5. a second host cannot steal the claim (refused)
#   6. the claiming host disconnecting brings the TUI chrome back
#   7. a subscriber that never reads does not stall rendering
#   8. notifications: a notify event + unread count, and notif-read clears it
#   9. an outcome line (flash) reaches the host as an event
#  10. `comux proxy --stdio` bridges the same server (same incarnation)
#  11. an attach naming the wrong incarnation is refused BEFORE it joins the shared view
#  12. a restarted server announces a new incarnation
#  13. a render-channel Command runs AFTER the keys sent before it on that connection
#  14. ping/pong on the render channel; Hello advertises commands
#  15. an attach that replaces its earlier attachment (matching tag) evicts it; a wrong tag does not
#  16. a heartbeat client that goes silent is detached (and its size stops pinning the view)
#  17. kill-server sent as a render-channel Command is answered before the server exits

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMUX="$REPO/target/debug/comux"
if [[ "${COMUX_E2E_NO_BUILD:-0}" != 1 ]]; then
    (cd "$REPO" && cargo build -q -p copad-mux) || { echo "build failed"; exit 2; }
fi
[[ -x "$COMUX" ]] || { echo "build first: cargo build -p copad-mux"; exit 2; }

# Short: the socket path must fit SUN_LEN (104 on macOS).
WORK="$(mktemp -d /tmp/cmxh.XXXXXX)"
export COPAD_MUX_SOCK="$WORK/s"
export COPAD_MUX_STATE="$WORK/state.json"
export COPAD_MUX_NOTIFY=0 COPAD_MUX_USAGE=0 COPAD_MUX_UPDATE_CHECK=0 COPAD_MUX_QUIET_SSH=1
export XDG_CONFIG_HOME="$WORK/cfg"
mkdir -p "$WORK/cfg/copad"
# Usually run inside a comux pane: never let a verb resolve the caller on the test server.
unset COPAD_MUX_PANE COPAD_MUX COPAD_SOCKET COPAD_PANEL_ID COPAD_MUX_HOST_TAG

SERVER_PID=""
cleanup() {
    [[ -n "$SERVER_PID" ]] && kill -9 "$SERVER_PID" 2>/dev/null || true
    # A second server from step 12 is reaped through its own socket.
    "$COMUX" kill-server >/dev/null 2>&1 || true
    rm -rf "$WORK"
    return 0
}
trap cleanup EXIT

start_server() {
    "$COMUX" server >"$WORK/server.log" 2>&1 &
    SERVER_PID=$!
    for _ in $(seq 1 100); do
        "$COMUX" list-sessions >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "FAIL: server did not come up"; cat "$WORK/server.log"; exit 1
}
start_server

cat >"$WORK/drive.py" <<'PYEOF'
import json, os, pty, re, select, socket, struct, subprocess, sys, time, fcntl, termios

COMUX, SOCK, WORK = sys.argv[1], os.environ["COPAD_MUX_SOCK"], sys.argv[2]
ROWS, COLS = 24, 110
HOST = socket.gethostname().split(".")[0]

def fail(msg):
    print("FAIL:", msg); sys.exit(1)

def ok(msg):
    print("  ok —", msg)

class Sub:
    """An L0 subscriber over the raw socket."""
    def __init__(self, read=True):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SOCK)
        self.buf = b""
        self.send({"t": "subscribe", "v": 1})
        self.model = {}
        self.events = []
        self.n = 0
    def send(self, m):
        self.s.sendall((json.dumps(m) + "\n").encode())
    def recv(self, timeout):
        end = time.time() + timeout
        while b"\n" not in self.buf:
            left = end - time.time()
            if left <= 0:
                return None
            r, _, _ = select.select([self.s], [], [], left)
            if not r:
                return None
            c = self.s.recv(1 << 20)
            if not c:
                return None
            self.buf += c
        line, self.buf = self.buf.split(b"\n", 1)
        m = json.loads(line)
        if m["t"] == "snapshot":
            for k, v in m["topics"].items():
                self.model[k] = v["data"]
        elif m["t"] == "state":
            self.model[m["topic"]] = m["data"]
        elif m["t"] in ("event",):
            self.events.append(m)
        return m
    def wait(self, pred, timeout=8, what="condition"):
        end = time.time() + timeout
        while time.time() < end:
            if pred(self):
                return
            self.recv(0.3)
        fail(f"timed out waiting for {what}; model={json.dumps(self.model)[:600]}")
    def cmd(self, req, timeout=8):
        self.n += 1
        cid = f"e{self.n}"
        self.send({"t": "cmd", "id": cid, "req": req})
        end = time.time() + timeout
        while time.time() < end:
            m = self.recv(0.5)
            if m and m["t"] == "reply" and m["id"] == cid:
                return m["resp"]
        fail(f"no reply to {req}")
    def close(self):
        self.s.close()

class Client:
    """A real `comux` client in a pty, with a binding tag; its frames are replayed to a grid."""
    def __init__(self, tag):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.environ["COPAD_MUX_HOST_TAG"] = tag
            os.execvp(COMUX, [COMUX])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.grid = [[" "] * COLS for _ in range(ROWS)]
        self.cy = self.cx = 0
        self.pend = ""
    def pump(self, sec):
        end = time.time() + sec
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.1)
            if r:
                try:
                    c = os.read(self.fd, 65536)
                except OSError:
                    return
                if not c:
                    return
                self.feed(c.decode("utf-8", "replace"))
    def feed(self, text):
        text = self.pend + text
        self.pend = ""
        i = 0
        while i < len(text):
            ch = text[i]
            if ch == "\x1b":
                m = re.match(r"\x1b\[([0-9;?<>=]*)([A-Za-z@`])", text[i:])
                if m:
                    params, fin = m.group(1), m.group(2)
                    if fin == "H":
                        p = [int(x) for x in params.split(";") if x.isdigit()] or [1, 1]
                        self.cy = p[0] - 1
                        self.cx = (p[1] - 1) if len(p) > 1 else 0
                    elif fin == "J" and params in ("2", ""):
                        self.grid = [[" "] * COLS for _ in range(ROWS)]
                    i += m.end(); continue
                m2 = re.match(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", text[i:])
                if m2:
                    i += m2.end(); continue
                if len(text) - i < 32:
                    self.pend = text[i:]; return
                i += 1; continue
            if ch == "\r": self.cx = 0; i += 1; continue
            if ch == "\n": self.cy += 1; i += 1; continue
            if ch < " ": i += 1; continue
            if 0 <= self.cy < ROWS and 0 <= self.cx < COLS:
                self.grid[self.cy][self.cx] = ch
            self.cx += 1; i += 1
    def text(self):
        return "\n".join("".join(r) for r in self.grid)
    def row(self, y):
        return "".join(self.grid[y])
    def keys(self, b):
        os.write(self.fd, b)
    def until(self, pred, timeout=8, what="frame"):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.3)
            if pred(self):
                return
        fail(f"timed out waiting for {what}:\n{self.text()}")
    def detach(self):
        try:
            os.write(self.fd, b"\x02d"); time.sleep(0.5); os.close(self.fd)
        except OSError:
            pass
        try: os.waitpid(self.pid, 0)
        except ChildProcessError: pass

# 1. welcome + one snapshot with every topic
a = Sub()
w = a.recv(5)
if not w or w["t"] != "welcome" or w["proto"] != 1 or not w["incarnation"]:
    fail(f"first line is not a welcome: {w}")
snap = a.recv(5)
if not snap or snap["t"] != "snapshot":
    fail(f"second line is not a snapshot: {snap}")
topics = sorted(snap["topics"])
if topics != ["agents", "clients", "notifications", "status", "tree"]:
    fail(f"snapshot topics: {topics}")
if len(a.model["tree"]["sessions"]) != 1:
    fail("expected one session")
INC = w["incarnation"]
ok(f"welcome (incarnation {INC}) then one snapshot with {len(topics)} topics")

# 2. a command over the subscription
r = a.cmd({"cmd": "new-session", "name": "hostapi-b", "detached": True})
if not r.get("ok"):
    fail(f"new-session: {r}")
a.wait(lambda s: len(s.model["tree"]["sessions"]) == 2, what="the new session in the tree")
names = [x["name"] for x in a.model["tree"]["sessions"]]
if "hostapi-b" not in names:
    fail(f"tree names: {names}")
ok("cmd new-session replied ok and the tree state followed")

# 3. a tagged client renders the TUI chrome
c = Client("e2e-tag")
c.until(lambda c: "spaces" in c.text() and HOST in c.row(ROWS - 1), what="sidebar + status bar")
a.wait(lambda s: any(x["tag"] == "e2e-tag" for x in s.model["clients"]["clients"]),
       what="the tagged attachment in the clients topic")
att = [x for x in a.model["clients"]["clients"] if x["tag"] == "e2e-tag"][0]["id"]
ok(f"tagged client attached (attachment {att}); TUI sidebar + status bar drawn")

# 4. claim it: the chrome disappears and the pane takes the space
a.send({"t": "host-ready", "attachment": att, "incarnation": INC, "features": ["sidebar", "status"]})
a.wait(lambda s: sorted(s.model["clients"]["hidden"]) == ["sidebar", "status"], what="hidden features")
c.until(lambda c: "spaces" not in c.text() and HOST not in c.row(ROWS - 1), what="chrome gone")
c.keys(b"echo WIDE-$((40+2))\r")
c.until(lambda c: "WIDE-42" in c.text(), what="the shell, now full width")
line = [l for l in c.text().split("\n") if "WIDE-42" in l and "echo" not in l][0]
if line.index("WIDE-42") > 2:
    fail(f"the pane did not move to column 0 once the sidebar went: {line!r}")
ok("host-ready hid the sidebar and status bar; the pane starts at column 0")

# 5. a second host cannot steal the claim
b = Sub()
b.recv(5); b.recv(5)
b.send({"t": "host-ready", "attachment": att, "incarnation": INC, "features": ["sidebar"]})
end = time.time() + 5
err = None
while time.time() < end and not err:
    m = b.recv(0.5)
    if m and m["t"] == "error":
        err = m
if not err or err["code"] != "refused":
    fail(f"second claim not refused: {err}")
b.close()
ok("a second host's claim was refused")

# 6. the owner disconnecting brings the chrome back
a.close()
c.until(lambda c: "spaces" in c.text() and HOST in c.row(ROWS - 1), what="chrome back")
ok("closing the claiming subscription restored the TUI chrome")

# 7. a subscriber that never reads does not stall rendering
deaf = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
deaf.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
deaf.connect(SOCK)
deaf.sendall(b'{"t":"subscribe","v":1}\n')
for i in range(40):
    subprocess.run([COMUX, "rename-session", "0", f"churn-{i}-" + "x" * 200],
                   capture_output=True, timeout=10)
c.keys(b"echo LIVE-$((7*6))\r")
c.until(lambda c: "LIVE-42" in c.text(), timeout=6, what="rendering with a deaf subscriber")
ok("a deaf subscriber did not stall the render loop")
deaf.close()

# 8. notifications
a = Sub()
a.recv(5); a.recv(5)
tok = a.model["tree"]["sessions"][0]["tabs"][0]["panes"][0]["token"]
subprocess.run([COMUX, "notify", "--pane", tok, "--kind", "done", "e2e body"],
               capture_output=True, timeout=10, check=True)
a.wait(lambda s: any(e["kind"] == "notify" for e in s.events), what="notify event")
a.wait(lambda s: s.model["notifications"]["unread"] >= 1, what="unread count")
nid = a.model["notifications"]["items"][0]["id"]
r = a.cmd({"cmd": "notif-read", "id": nid})
if not r.get("ok"):
    fail(f"notif-read: {r}")
a.wait(lambda s: s.model["notifications"]["unread"] == 0, what="unread cleared")
ok("notify event + unread, and notif-read cleared it")

# 9. a flash reaches the host as an event (Ctrl-b u with nothing unread says so)
c.keys(b"\x02u")
a.wait(lambda s: any(e["kind"] == "flash" and "unread" in e["data"]["text"] for e in s.events),
       what="flash event")
ok("an outcome line arrived as a flash event")
a.close()

# 10. proxy --stdio reaches the same server
p = subprocess.Popen([COMUX, "proxy", "--stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
p.stdin.write(b'{"t":"subscribe","v":1}\n'); p.stdin.flush()
first = json.loads(p.stdout.readline())
p.stdin.close()
p.wait(timeout=10)
if first.get("incarnation") != INC:
    fail(f"proxy reached a different server: {first}")
ok("proxy --stdio bridged to the same server")

# 11. an attach naming the wrong incarnation is refused before it joins
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(SOCK)
s.sendall(b'{"t":"attach","cols":20,"rows":5,"expect_incarnation":"not-this-one"}\n')
s.settimeout(5)
line = s.makefile().readline()
if '"refused"' not in line or "stale" not in line:
    fail(f"wrong-incarnation attach: {line}")
s.close()
q = Sub(); q.recv(5); q.recv(5)
if any(x["cols"] == 20 for x in q.model["clients"]["clients"]):
    fail("the refused attach registered anyway")
q.close()
ok("an attach expecting another incarnation was refused and never registered")

class Raw:
    """A native render client over the raw socket (what the iOS app speaks)."""
    def __init__(self, tag, cols=90, rows=20, **attach):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SOCK)
        self.f = self.s.makefile("rb")
        m = {"t": "attach", "cols": cols, "rows": rows}
        m.update(attach)
        self.send(m)
        self.hello = self.read_until(lambda m: m["t"] in ("hello", "refused"))
        self.send({"t": "env", "vars": [], "host_tag": tag})
    def send(self, m):
        self.s.sendall((json.dumps(m) + "\n").encode())
    def read_until(self, pred, timeout=8):
        self.s.settimeout(timeout)
        while True:
            line = self.f.readline()
            if not line:
                return None
            m = json.loads(line)
            if pred(m):
                return m
    def key(self, code, mods=""):
        self.send({"t": "key", "code": code, "modifiers": mods, "kind": "Press", "state": ""})
    def close(self):
        self.s.close()

def ids():
    q = Sub(); q.recv(5); q.recv(5)
    out = [x["id"] for x in q.model["clients"]["clients"]]
    q.close()
    return out

# 13. a Command runs after the keys sent before it: Ctrl-b c (new tab, handled synchronously
# by the server) then list-tabs on the same connection must already count the new tab.
r1 = Raw("raw-1")
if not r1.hello.get("commands"):
    fail(f"Hello does not advertise commands: {r1.hello}")
r1.send({"t": "command", "id": "x0", "req": {"cmd": "list-tabs"}})
before = len(r1.read_until(lambda m: m["t"] == "reply" and m["id"] == "x0")["resp"]["tabs"])
r1.key({"Char": "b"}, "CONTROL")
r1.key({"Char": "c"})
r1.send({"t": "command", "id": "x1", "req": {"cmd": "list-tabs"}})
rep = r1.read_until(lambda m: m["t"] == "reply" and m["id"] == "x1")
after = len(rep["resp"]["tabs"])
if after != before + 1:
    fail(f"the command overtook the keys before it: tabs {before} -> {after}")
ok(f"a render-channel Command saw the effect of the keys sent before it (tabs {before} -> {after})")

# 14. ping / pong
r1.send({"t": "ping", "id": 77})
if not r1.read_until(lambda m: m["t"] == "pong" and m["id"] == 77):
    fail("no pong")
ok("render-channel ping answered")

# 15. replace: a wrong tag does not evict, the right one does
old = r1.hello["attachment"]
bad = Raw("raw-2", replaces={"attachment": old, "tag": "not-raw-1"})
time.sleep(0.5)
if old not in ids():
    fail("a replace with the wrong tag evicted the attachment")
good = Raw("raw-1", replaces={"attachment": old, "tag": "raw-1"})
time.sleep(0.5)
now = ids()
if old in now or good.hello["attachment"] not in now:
    fail(f"replace did not swap {old} -> {good.hello['attachment']}: {now}")
bad.close(); good.close(); r1.close()
ok("replace evicts only the attachment whose tag matches")

# 16. a silent heartbeat client is detached
hb = Raw("raw-hb", cols=40, rows=10, heartbeat=True)
hid = hb.hello["attachment"]
time.sleep(0.5)
if hid not in ids():
    fail("heartbeat client never attached")
time.sleep(33)
if hid in ids():
    fail("a silent heartbeat client was not detached")
hb.close()
ok("a heartbeat client silent past the expiry was detached")

c.detach()

# 17. kill-server sent as a render-channel Command is answered before the server goes
k = Raw("raw-kill")
k.send({"t": "command", "id": "k1", "req": {"cmd": "kill-server"}})
rep = k.read_until(lambda m: m["t"] == "reply" and m["id"] == "k1")
if not rep or not rep["resp"].get("ok"):
    fail(f"kill-server over the render channel got no answer: {rep}")
ok("kill-server over the render channel was answered before shutdown")
print("INC", INC)
PYEOF

python3 "$WORK/drive.py" "$COMUX" "$WORK" | tee "$WORK/out"
grep -q '^FAIL' "$WORK/out" && exit 1
OLD_INC="$(awk '/^INC /{print $2}' "$WORK/out")"
[[ -n "$OLD_INC" ]] || { echo "FAIL: driver did not finish"; exit 1; }

# 12. a restarted server is a new incarnation (step 17 already stopped it)
"$COMUX" kill-server >/dev/null 2>&1 || true
for _ in $(seq 1 50); do kill -0 "$SERVER_PID" 2>/dev/null || break; sleep 0.1; done
sleep 1.1 # the incarnation is pid + boot second; make the second differ too
start_server
NEW_INC="$(python3 - <<'PYEOF'
import json, os, socket
s = socket.socket(socket.AF_UNIX); s.connect(os.environ["COPAD_MUX_SOCK"])
s.sendall(b'{"t":"subscribe","v":1}\n')
print(json.loads(s.makefile().readline())["incarnation"])
PYEOF
)"
[[ "$NEW_INC" != "$OLD_INC" ]] || { echo "FAIL: incarnation did not change across a restart"; exit 1; }
echo "  ok — a restarted server announced a new incarnation ($OLD_INC -> $NEW_INC)"
echo "PASS: comux L0 host API"
