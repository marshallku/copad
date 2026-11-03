#!/usr/bin/env bash
# e2e for the comux work-graph page (decisions #204): the exact page() bytes copad loads, in a
# real (headless) Chromium, driven through window.comuxGraph with a fixture `graph` topic.
#
#   1. it runs under its production CSP and says ready
#   2. agent mode: the focused agent's files, its subagent, its steps, the live cursor
#   3. a new step arriving live moves the cursor and lights its file
#   4. scrubbing back hides files not touched yet; replay plays from the start
#   5. all-sessions mode: both agents, the file both touched is ONE shared node
#   6. it asks the host for history it lacks, and merges the page it gets
#
# Screenshots land in $OUT (default: a temp dir) — look at them.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
command -v "$CHROME" >/dev/null 2>&1 || [[ -x "$CHROME" ]] || CHROME="$(command -v chromium || command -v google-chrome || true)"
[[ -n "$CHROME" ]] || { echo "SKIP: no Chrome/Chromium"; exit 0; }
WORK="$(mktemp -d "${TMPDIR:-/tmp}/graph-page-e2e.XXXXXX")"
OUT="${OUT:-$WORK}"
trap 'rm -rf "$WORK/profile"' EXIT
fail() { echo "FAIL: $*"; exit 1; }
ok() { echo "  ok — $*"; }

(cd "$REPO" && cargo run -q -p copad-graph-web --example dump) >"$WORK/page.html"
FIXTURE="$(cat "$REPO/copad-graph-web/tests/fixture.json")"

# The page plus a driver that runs the scenario and writes every check's state into the DOM;
# --dump-dom prints it once the virtual time budget has run out.
run() { # scenario-js → $WORK/<name>.json (+ .png)
    local name="$1" js="$2"
    python3 - "$WORK/page.html" "$WORK/$name.html" "$FIXTURE" "$js" <<'PY'
import sys
page, out, fixture, js = sys.argv[1:]
html = open(page).read()
driver = """
<pre id="e2e-out"></pre>
<script>
window.__posts = [];
window.webkit = { messageHandlers: { comux: { postMessage: (m) => window.__posts.push(m) } } };
</script>
"""
runner = """
<script>
(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const G = window.comuxGraph, FIX = %s, out = {};
  try { %s } catch (e) { out.error = String(e && e.stack || e); }
  out.posts = window.__posts;
  document.getElementById("e2e-out").textContent = "@@" + "OUT@@" + JSON.stringify(out) + "@@" + "END@@";
})();
</script>
""" % (fixture, js)
# The bridge must exist before the page script runs (it posts `ready` at once).
html = html.replace("<body>", "<body>" + driver, 1).replace("</body>", runner + "</body>", 1)
open(out, "w").write(html)
PY
    "$CHROME" --headless=new --disable-gpu --no-first-run --user-data-dir="$WORK/profile" \
        --window-size=900,640 --virtual-time-budget=10000 --dump-dom "file://$WORK/$name.html" 2>/dev/null \
        | python3 -c 'import sys,re; m=re.search(r"@@OUT@@(.*?)@@END@@", sys.stdin.read(), re.S); print(m.group(1) if m else "{}")' >"$WORK/$name.json"
    "$CHROME" --headless=new --disable-gpu --no-first-run --user-data-dir="$WORK/profile" \
        --window-size=900,640 --virtual-time-budget=10000 --screenshot="$OUT/$name.png" "file://$WORK/$name.html" >/dev/null 2>&1 || true
}
q() { python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))' "$WORK/$1.json" "$2"; }

echo "1-2. agent mode"
run agent '
  G.setTheme({ accent: "#89b4fa", font: "Menlo", fontSize: 12 });
  G.setHost({ local: true });
  G.setData(FIX);
  await sleep(1500);
  out.state = G.state();
'
[[ "$(q agent 'd.get("error","")')" == "" ]] || fail "the page threw: $(q agent 'd["error"]')"
[[ "$(q agent 'd["posts"][0]["type"]')" == ready ]] || fail "no ready under the production CSP: $(cat "$WORK/agent.json")"
[[ "$(q agent 'd["state"]["agent"]')" == "n1-1" ]] || fail "the focused agent should be shown"
[[ "$(q agent '(d["state"]["files"], d["state"]["subagents"], d["state"]["steps"])')" == "(5, 1, 11)" ]] \
    || fail "agent view: $(q agent 'd["state"]')"
[[ "$(q agent '(d["state"]["live"], d["state"]["seq"])')" == "(True, 11)" ]] || fail "live cursor on the newest step"
q agent 'd["state"]["card"]' | grep -q "Read src/parse.rs" || fail "card: $(q agent 'd["state"]["card"]')"
q agent 'd["state"]["summary"]' | grep -q "5 files · 4 reads · 3 edits · 1 searches · 1 subagents · 1 failed" \
    || fail "summary: $(q agent 'd["state"]["summary"]')"
ok "ready under its CSP; 5 files, 1 subagent, 11 steps; live on #11; summary counts"

echo "3. a live step"
run live '
  G.setData(FIX);
  await sleep(300);
  const next = JSON.parse(JSON.stringify(FIX));
  const a = next.graph.agents[0];
  a.steps.push({ seq: 12, at_ms: a.updated_ms + 1000, kind: "edit", file: 1, outcome: "ok" });
  a.settled = [{ seq: 11, outcome: "ok" }]; a.pending = [];
  G.setData(next);
  await sleep(200);
  out.state = G.state();
'
[[ "$(q live '(d["state"]["seq"], 1 in d["state"]["lit"])')" == "(12, True)" ]] || fail "the live step: $(q live 'd["state"]')"
ok "a new step moves the live cursor to #12 and lights lex.rs"

echo "4. scrub and replay"
run scrub '
  G.setData(FIX);
  await sleep(300);
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft" }));
  for (let i = 0; i < 7; i++) window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft" }));
  out.back = G.state();
  document.getElementById("play").click();
  // The topic keeps arriving (same value) while it plays: the replay must not stall.
  const tick = setInterval(() => G.setData(FIX), 50);
  await sleep(200);
  out.playing = G.state();
  await sleep(8500); // 8 steps × the 900 ms gap cap
  clearInterval(tick);
  out.done = G.state();
'
[[ "$(q scrub '(d["back"]["seq"], d["back"]["live"])')" == "(3, False)" ]] || fail "scrubbed back: $(q scrub 'd["back"]')"
[[ "$(q scrub 'd["playing"]["playing"]')" == True ]] || fail "replay should be playing"
[[ "$(q scrub '(d["done"]["playing"], d["done"]["seq"], d["done"]["live"])')" == "(False, 11, True)" ]] || fail "replay should end live: $(q scrub 'd["done"]')"
ok "← steps back to #3 (not live); ▶ replays to the end and returns to live"

echo "5. all sessions"
run all '
  G.setData(FIX);
  G.setMode("all");
  await sleep(1200);
  out.state = G.state();
'
[[ "$(q all '(d["state"]["mode"], d["state"]["agents"], d["state"]["shared"])')" == "('all', 2, 1)" ]] || fail "all view: $(q all 'd["state"]')"
[[ "$(q all 'd["state"]["files"]')" == 6 ]] || fail "6 distinct files across both agents: $(q all 'd["state"]')"
ok "2 agents, 6 distinct files, parse.rs shared by both"

echo "6. history"
run history '
  const f = JSON.parse(JSON.stringify(FIX));
  const a = f.graph.agents[0];
  a.steps = a.steps.slice(6); a.oldest_seq = 1;
  G.setData(f);
  await sleep(100);
  out.asked = window.__posts.filter((p) => p.type === "history");
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft" }));
  out.before = G.state().seq;
  G.addHistory({ token: "n1-1", conversation: "c1", gen: "g1", files: FIX.graph.agents[0].files, subagents: [], steps: FIX.graph.agents[0].steps.slice(0, 6), oldest_seq: 1, page_first_seq: 1, page_last_seq: 6, next_before: null });
  G.addHistory({ token: "n1-1", conversation: "c1", gen: "OLD", files: [], subagents: [], steps: [{ seq: 99, at_ms: 0, kind: "read", outcome: "ok" }], oldest_seq: 1, next_before: null });
  await sleep(300);
  out.state = G.state();
'
[[ "$(q history '[(p["token"], p["before"]) for p in d["asked"]]')" == "[('n1-1', 7)]" ]] || fail "should ask for steps before #7: $(q history 'd["asked"]')"
[[ "$(q history 'd["state"]["steps"]')" == 11 ]] || fail "the page should merge (and the other gen's be ignored): $(q history 'd["state"]')"
[[ "$(q history '(d["before"], d["state"]["seq"])')" == "(10, 10)" ]] || fail "older history must not move the cursor off its step: $(q history '(d["before"], d["state"])')"
ok "asked for history before #7, merged 6 older steps (the cursor stayed on #10), ignored a page of another gen"

echo
echo "PASS — the work-graph page; screenshots in $OUT"
