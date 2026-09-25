// A board row must open the agent it names — the defect being fixed is that all 27 rows called
// `enterMux()` with no target and therefore landed on whatever comux last focused.
import fs from "fs";
const src = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const results = [];
const check = (l, c) => { results.push(!!c); console.log(`  ${c ? "ok  " : "FAIL"} ${l}`); };

const m = src.match(/    async function openAgent\(el, token\) \{[\s\S]*?\n    \}/);
if (!m) { console.log("FAIL: openAgent not found"); process.exit(1); }

const env = ({ fail = null, defer = false } = {}) => {
  let release = null;
  const calls = { api: [], enterMux: 0, render: 0 };
  const state = { jumpBusy: null, boardNote: "", muxEpoch: 0, mode: "overview" };
  const el = { cls: new Set(), classList: { add: (c) => el.cls.add(c), remove: (c) => el.cls.delete(c) } };
  const api = (path, opts) => {
    calls.api.push([path, JSON.parse(opts.body)]);
    if (defer) return new Promise((res, rej) => { release = () => (fail ? rej(fail) : res({ ok: true })); });
    return fail ? Promise.reject(fail) : Promise.resolve({ ok: true });
  };
  const openAgent = new Function("state", "api", "enterMux", "render",
    m[0] + "; return openAgent;")(state, api, () => { calls.enterMux++; }, () => { calls.render++; });
  return { openAgent, calls, state, el, release: () => release && release() };
};

// --- the row's identity is what gets opened ---
{
  const e = env();
  await e.openAgent(e.el, "17bca6ab617c6-4");
  check("the tapped row's token is what is jumped to",
        e.calls.api.length === 1 && e.calls.api[0][0] === "/api/board/jump"
        && e.calls.api[0][1].token === "17bca6ab617c6-4");
  check("...and the terminal opens after the jump, not before", e.calls.enterMux === 1);
  check("...leaving nothing latched", e.state.jumpBusy === null && !e.el.cls.has("pending"));
}

// --- a refused jump does not open somebody else's session ---
{
  const e = env({ fail: { code: "failed", message: "unknown pane: 17bca6ab617c6-4" } });
  await e.openAgent(e.el, "17bca6ab617c6-4");
  check("a stale token does NOT fall back to the focused session", e.calls.enterMux === 0);
  check("...and says why", e.state.boardNote.includes("unknown pane"));
  check("...and unlatches so the next tap works", e.state.jumpBusy === null);
}

// --- two taps cannot race two server-side mutations ---
{
  const e = env();
  const first = e.openAgent(e.el, "aaa-1");
  await e.openAgent(e.el, "bbb-2");          // while the first is still in flight
  await first;
  check("a second tap during a jump is ignored rather than racing it",
        e.calls.api.length === 1 && e.calls.api[0][1].token === "aaa-1");
}

// --- a pane with no token is not jumped to by a reusable id ---
{
  const e = env();
  await e.openAgent(e.el, "");
  check("a pre-token pane opens the terminal without moving anything",
        e.calls.api.length === 0 && e.calls.enterMux === 1);
  check("...and the row never offers a terminal id as a target",
        !/data-token="\$\{escapeHtml\(a\.token \|\| a\.terminal/.test(src));
}

// --- a jump that lands after the user has navigated away ---
{
  const e = env({ defer: true });
  const p = e.openAgent(e.el, "aaa-1");     // suspended on the request
  e.state.muxEpoch++; e.state.mode = "mux";        // the header button opened the terminal
  e.state.muxEpoch++; e.state.mode = "overview";   // …and Back left it again
  e.release();
  await p;
  check("a jump completing after navigation does not reopen the terminal", e.calls.enterMux === 0);
  check("...and still unlatches", e.state.jumpBusy === null);
}
{
  const e = env({ defer: true, fail: { code: "unknown_pane", message: "unknown pane" } });
  const p = e.openAgent(e.el, "aaa-1");
  e.state.muxEpoch++; e.state.mode = "mux";
  e.release();
  await p;
  check("a failure landing after navigation does not banner over another screen",
        e.state.boardNote === "" && e.calls.render === 0);
}

const ok = results.every(Boolean);
console.log(ok ? "PASS — a row opens the agent it names, or says why it cannot" : "FAIL");
process.exit(ok ? 0 : 1);
