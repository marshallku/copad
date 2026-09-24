// Exercise the REAL enterMux/teardownMux/muxPreflight from the file against a fake api(),
// reproducing the Back-then-Forward-while-pending sequence codex found.
import fs from "fs";
const src0 = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const grab = (re) => { const m = src0.match(re); if (!m) throw new Error("not found: " + re); return m[0]; };
const src = [
  grab(/    function disposeMuxTerminal\(\) \{[\s\S]*?\n    \}/),
  grab(/    function teardownMux\(\) \{[\s\S]*?\n    \}/),
  grab(/    async function muxPreflight\(\) \{[\s\S]*?\n    \}/),
  grab(/    async function enterMux\(pushHistory\) \{[\s\S]*?\n    \}/),
].join("\n");

let pending = [];
const api = () => new Promise((res) => pending.push(res));
const state = { mode: "overview", muxError: "", muxChecking: false, muxEpoch: 0,
                muxResizeOff: null, term: null, fit: null, ws: { attach: null } };
let renders = 0;
const render = () => { renders++; };
const history = { state: null, pushState(s) { this.state = s; }, back() {} };
// The board-polling calls are lifecycle side effects, not part of what this harness checks —
// stub them so the navigation logic can be exercised in isolation.
let polls = { start: 0, stop: 0 };
const startBoardPolling = () => { polls.start++; };
const stopBoardPolling = () => { polls.stop++; };
const fn = new Function("state", "api", "render", "history", "startBoardPolling", "stopBoardPolling",
  src + "; return { enterMux, teardownMux };");
const { enterMux, teardownMux } = fn(state, api, render, history, startBoardPolling, stopBoardPolling);

(async () => {
  console.log("1. click terminal (preflight goes out, stays pending)");
  const p1 = enterMux(true);
  console.log(`   mode=${state.mode} checking=${state.muxChecking} epoch=${state.muxEpoch}`);

  console.log("2. Back while it is still pending");
  teardownMux();
  console.log(`   mode=${state.mode} checking=${state.muxChecking} epoch=${state.muxEpoch}`);

  console.log("3. Forward -> enterMux(false), the case that used to be swallowed");
  const p2 = enterMux(false);
  console.log(`   mode=${state.mode} checking=${state.muxChecking} epoch=${state.muxEpoch}`);

  console.log("4. the FIRST (stale) response lands");
  pending[0]({ ok: false, message: "stale-should-be-ignored" });
  await p1;
  console.log(`   mode=${state.mode} error=${JSON.stringify(state.muxError)}`);

  console.log("5. the second response lands");
  pending[1]({ ok: true });
  await p2;
  console.log(`   mode=${state.mode} checking=${state.muxChecking} error=${JSON.stringify(state.muxError)}`);

  const ok = state.mode === "mux" && state.muxChecking === false && state.muxError === "";
  console.log(ok ? "PASS — Forward-while-pending reopens mux and the stale response is dropped"
                 : "FAIL");
  process.exit(ok ? 0 : 1);
})();
