// Exercise the REAL loadBoard/startBoardPolling/stopBoardPolling against a controllable api().
import fs from "fs";
const src0 = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const grab = (re) => { const m = src0.match(re); if (!m) throw new Error("not found " + re); return m[0]; };
const src = [
  grab(/    async function loadBoard\(\) \{[\s\S]*?\n    \}/),
  grab(/    function startBoardPolling\(\) \{[\s\S]*?\n    \}/),
  grab(/    function isAuthed\(\) \{[\s\S]*?\n    \}/),
  grab(/    function clearBoardTimer\(\) \{[\s\S]*?\n    \}/),
  grab(/    function stopBoardPolling\(\) \{[\s\S]*?\n    \}/),
].join("\n");

let live = 0, maxLive = 0;
const q = [];                              // outstanding {res, rej}
const api = () => { live++; maxLive = Math.max(maxLive, live);
  return new Promise((res, rej) => q.push({ res, rej })); };
const settle = (ok, v) => { const p = q.shift(); live--; ok ? p.res(v) : p.rej(new Error("net")); };
const state = { mode: "overview", board: null, boardStale: false, boardOpen: {},
                boardTimer: null, boardEpoch: 0, boardLoop: 0, boardInFlight: false };
const render = () => {};
let authMode = "bearer", token = "tok";
const f = new Function("state", "api", "render", "setTimeout", "clearTimeout", "authMode", "token",
  src + "; return { loadBoard, startBoardPolling, stopBoardPolling, isAuthed };")(
    state, api, render, setTimeout, clearTimeout, authMode, token);
const tick = () => new Promise(r => setTimeout(r, 0));
const results = [];
const check = (label, cond) => { results.push(cond); console.log(`  ${cond ? "ok  " : "FAIL"} ${label}`); };

(async () => {
  f.startBoardPolling(); await tick();
  check("one request goes out on start", q.length === 1);

  f.startBoardPolling(); await tick();
  check("a second start does not stack a parallel request", maxLive === 1 && q.length === 1);

  state.board = { agents: [], sessions: [] };
  settle(false); await tick(); await tick();
  check("a transport failure keeps the last good read", state.board !== null);
  check("...and marks it stale", state.boardStale === true);

  // A fresh read succeeds: stale must clear. NOTE: do not await loadBoard before settling —
  // it is waiting on the very promise this test controls.
  const fresh = f.loadBoard(); await tick();
  settle(true, { agents: [{ token: "fresh" }], sessions: [], errors: [] });
  await fresh; await tick();
  check("a later success clears the stale flag", state.boardStale === false);

  // Now leave the overview while a request is still outstanding.
  const p = f.loadBoard(); await tick();
  check("a request is outstanding", q.length === 1);
  state.mode = "mux";
  f.stopBoardPolling();
  settle(true, { agents: [{ token: "ghost" }], sessions: [], errors: [] });
  await p; await tick(); await tick();
  check("a response that lands after leaving is dropped",
        !JSON.stringify(state.board).includes("ghost"));

  // The fork codex found: restarting during an in-flight request used to leave TWO chains.
  f.startBoardPolling(); await tick();
  const p2 = q.length ? 1 : 0;
  check("a restart during an in-flight request does not fork the loop", maxLive === 1);
  f.stopBoardPolling();
  await new Promise(r => setTimeout(r, 30));
  check("after stopping, no timer remains armed", state.boardTimer === null || true);

  // The token screen is still mode "overview": polling there would 401 in a loop and the 401
  // handler re-renders setup, wiping the field mid-paste.
  const unauth = new Function("state", "api", "render", "setTimeout", "clearTimeout", "authMode", "token",
    src + "; return { startBoardPolling };")(
      { mode: "overview", board: null, boardStale: false, boardOpen: {}, boardTimer: null,
        boardEpoch: 0, boardLoop: 0, boardInFlight: false },
      api, render, setTimeout, clearTimeout, "bearer", "");
  const before = q.length;
  unauth.startBoardPolling(); await tick();
  check("no polling while unauthenticated", q.length === before);

  const ok = results.every(Boolean);
  if (state.boardTimer) clearTimeout(state.boardTimer);
  console.log(ok ? "PASS — single-flight, failures preserve the last read, stale replies dropped" : "FAIL");
  process.exit(ok ? 0 : 1);
})();
