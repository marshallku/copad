// Who owns focus, and — the regression this file exists for — whether a tap still reaches the
// terminal. The previous version stopped `mousedown` in the capture phase to protect the IME, and
// that is why the terminal window was dead to the touch: no focus, no selection, and no mouse
// report, so comux's own clickable chrome (status-bar tab chips, sidebar rows, panes) did nothing.
import fs from "fs";
const src = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const results = [];
const check = (l, c) => { results.push(!!c); console.log(`  ${c ? "ok  " : "FAIL"} ${l}`); };

// --- the keybar's refocus, executed ---
const refocus = src.match(/      const refocus = \(\) => \{[\s\S]*?\n      \};/);
if (!refocus) { console.log("FAIL: refocus not found"); process.exit(1); }
const run = (mode) => {
  let focused = null;
  const st = { mode, term: { focus: () => { focused = "term"; } } };
  new Function("state", "imeEl", refocus[0] + "; refocus();")(st, () => ({ focus: () => { focused = "ime"; } }));
  return focused;
};
check("a keybar tap in mux keeps the keyboard on the IME strip", run("mux") === "ime");
check("the legacy attach view still focuses xterm itself", run("attach") === "term");

// --- taps must reach xterm ---
check("no mousedown guard is installed on the terminal any more",
      !/term-host[\s\S]{0,400}addEventListener\("mousedown"/.test(src));
check("nothing stops propagation inside the terminal host",
      !/host\?\.addEventListener\("(mousedown|contextmenu)"/.test(src));
const bounce = src.match(/      host\?\.addEventListener\("focusin", \(ev\) => \{[\s\S]*?\}, true\);/);
check("a focusin bounce is still the backstop", !!bounce);
check("...and it neither prevents nor stops the event",
      !!bounce && !/preventDefault|stopPropagation/.test(bounce[0]));

// --- xterm's own focus call is redirected rather than fought ---
check("xterm's textarea.focus is redirected to the strip",
      /term\.textarea\.focus = \(\) => imeEl\(\)\?\.focus\(\{ preventScroll: true \}\)/.test(src));
check("the cursor keeps rendering because focus is mirrored back to xterm",
      /dispatchEvent\(new FocusEvent\(type\)\)/.test(src));

// --- the mouse classifier, executed ---
const cls = src.match(/    function isMouseReport\(d\) \{[\s\S]*?\n    \}/);
const isMouseReport = new Function(cls[0] + "; return isMouseReport;")();
check("an SGR report is a mouse report", isMouseReport("\x1b[<0;40;12M"));
check("a DEFAULT-encoding report is too", isMouseReport("\x1b[M !!"));
check("a device-attributes reply is NOT ordered behind a composition", !isMouseReport("\x1b[?62;c"));
check("a cursor-position reply is not either", !isMouseReport("\x1b[24;80R"));
check("bracketed paste is not either", !isMouseReport("\x1b[200~ls\x1b[201~"));

// --- both mouse transports are subscribed ---
check("onBinary is subscribed, so a DEFAULT-encoding report is not lost", /term\.onBinary\(/.test(src));
check("...and its bytes do not go through TextEncoder",
      /Uint8Array\.from\(d, \(c\) => c\.charCodeAt\(0\) & 0xff\)/.test(src));

const ok = results.every(Boolean);
console.log(ok ? "PASS — the strip owns the keyboard and the terminal owns the pointer" : "FAIL");
process.exit(ok ? 0 : 1);
