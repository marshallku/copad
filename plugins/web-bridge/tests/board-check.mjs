// Render the REAL renderBoard() from the file against the REAL /api/board payload and assert
// the partition is exhaustive — the defect that would have hidden 26 of 27 agents.
import fs from "fs";
const src0 = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const grab = (re) => { const m = src0.match(re); if (!m) throw new Error("not found " + re); return m[0]; };
const src = [
  grab(/    const BOARD_ATTENTION = new Set\(\[[^\]]*\]\);/),
  grab(/    const BOARD_ACTIVE = new Set\(\[[^\]]*\]\);/),
  grab(/    function boardGroupKey\(a\) \{[\s\S]*?\n    \}/),
  grab(/    function boardRow\(a\) \{[\s\S]*?\n    \}/),
  grab(/    function humanSecs\(n\) \{[\s\S]*?\n    \}/),
  grab(/    function renderBoard\(\) \{[\s\S]*?\n    \}/),
].join("\n");
const board = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const state = { board, boardStale: false, boardOpen: {} };
const escapeHtml = (x) => String(x ?? "");
const renderBoard = new Function("state", "escapeHtml", src + "; return renderBoard;")(state, escapeHtml);

const out = renderBoard();
const agents = board.agents || [];
const rendered = [...out.matchAll(/data-token="([^"]*)"/g)].map(m => m[1]);
// Idle groups start collapsed, so expand them all and re-render to count every agent.
for (const a of agents) state.boardOpen[a.space_id ? `id:${a.space_id}` : `name:${a.space || "?"}`] = true;
const outOpen = renderBoard();
const renderedOpen = new Set([...outOpen.matchAll(/data-token="([^"]*)"/g)].map(m => m[1]));

const byStatus = {};
for (const a of agents) byStatus[a.status] = (byStatus[a.status] || 0) + 1;
console.log("payload:", agents.length, "agents", JSON.stringify(byStatus));
console.log("rendered while collapsed:", rendered.length);
console.log("rendered with groups open:", renderedOpen.size);
const missing = agents.filter(a => !renderedOpen.has(a.token || a.terminal || ""));
console.log("agents never rendered:", missing.length, missing.map(a => `${a.space}/${a.title}:${a.status}`).slice(0,5));
const counts = out.match(/idle <span[^>]*>(\d+)</);
// Partial failure: agents unreadable while sessions succeeded must NOT print a confident
// "idle 0" — that count would simply be false.
state.board = { sessions: board.sessions, agents: null, errors: [{ what: "agents", code: "timeout", message: "x" }] };
const partial = renderBoard();
const liesAboutIdle = /idle <span[^>]*>0</.test(partial) || partial.includes("no idle agents");
console.log("partial failure (agents null, sessions ok):");
console.log("  claims an empty fleet:", liesAboutIdle);
console.log("  says it could not read:", partial.includes("could not read the agent list"));

// ...and a transport failure on top of a partial response must still say the data is stale.
state.boardStale = true;
const partialStale = renderBoard();
console.log("  partial + stale shows the stale banner:", partialStale.includes("last good read"));
state.boardStale = false;

// Both null.
state.board = { sessions: null, agents: null, errors: [] };
const both = renderBoard();
console.log("both null -> could not read:", both.includes("could not read the agent list"));

const ok = partialStale.includes("last good read") && missing.length === 0 && renderedOpen.size === agents.length
  && !liesAboutIdle && partial.includes("could not read the agent list")
  && both.includes("could not read the agent list");
console.log(ok ? "PASS — exhaustive partition, and an unreadable list never reads as empty" : "FAIL");
process.exit(ok ? 0 : 1);
