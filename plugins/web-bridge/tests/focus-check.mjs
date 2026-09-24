// The review's C1: a keybar tap must not hand focus back to xterm while in compose mode.
import fs from "fs";
const src0 = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const m = src0.match(/      const refocus = \(\) => \{[\s\S]*?\n      \};/);
if (!m) { console.log("FAIL: refocus not found"); process.exit(1); }
const results = [];
const check = (l, c) => { results.push(c); console.log(`  ${c ? "ok  " : "FAIL"} ${l}`); };

const run = (state) => {
  let focused = null;
  const term = { focus: () => { focused = "term"; } };
  const composeEl = () => ({ focus: () => { focused = "compose"; } });
  const st = { term, ...state };
  new Function("state", "composeEl", m[0] + "; refocus();")(st, composeEl);
  return focused;
};

check("compose mode keeps focus on the compose bar",
      run({ mode: "mux", rawInput: false }) === "compose");
check("raw mode gives focus to the terminal",
      run({ mode: "mux", rawInput: true }) === "term");
check("the legacy attach view still focuses the terminal",
      run({ mode: "attach", rawInput: false }) === "term");

// The round-4 defect: preventDefault alone does not stop xterm's descendant handler, which
// calls focus() unconditionally. The event has to be stopped in capture.
const guardSrc = src0.match(/      const keepFocus = \(ev\) => \{[\s\S]*?\n      \};/);
check("the terminal-tap guard stops propagation, not just the default",
      !!guardSrc && guardSrc[0].includes("stopPropagation"));
const wiring = src0.match(/host\?\.addEventListener\("mousedown", keepFocus, true\)/);
check("...and is registered in the CAPTURE phase", !!wiring);

const ok = results.every(Boolean);
console.log(ok ? "PASS — a keybar tap never steals the IME's keyboard in compose mode" : "FAIL");
process.exit(ok ? 0 : 1);
