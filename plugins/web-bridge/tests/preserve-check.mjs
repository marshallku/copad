// Reproduce codex's C1: an unrelated async handler calls render() while mux is open.
// Uses the REAL renderPreservingTerminal from the file against a minimal DOM.
import fs from "fs";
const src0 = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const m = src0.match(/    function renderPreservingTerminal\(html\) \{[\s\S]*?\n    \}/);
if (!m) { console.log("FAIL: helper not found"); process.exit(1); }

// Minimal DOM: elements with ids, a root whose innerHTML setter rebuilds children.
class El {
  constructor(id) { this.id = id; this.children = []; this.parent = null; this.marker = null; }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter(c => c !== this); this.parent = null; }
  replaceWith(other) {
    if (!this.parent) return;
    const i = this.parent.children.indexOf(this);
    this.parent.children[i] = other; other.parent = this.parent; this.parent = null;
  }
}
const root = new El("root");
Object.defineProperty(root, "innerHTML", {
  set(v) { root.children = [...v.matchAll(/id="([^"]+)"/g)].map(x => { const e = new El(x[1]); e.parent = root; return e; }); },
});
const find = (id) => root.children.find(c => c.id === id) || null;
const document = { getElementById: find };
const state = { term: null };
const helper = new Function("state", "root", "document", m[0] + "; return renderPreservingTerminal;")(state, root, document);

helper('<div id="term-host"></div><div id="kbd"></div>');
const original = find("term-host");
original.marker = "LIVE-XTERM";     // stand-in for the real xterm DOM
state.term = {};                     // a terminal is now live

// NOTE: the mux view is now built once and patched, so this path is only reached on the
// FIRST build. The helper must still be correct for it.
console.log("a render while a terminal is live...");
helper('<div class="banner">push updated</div><div id="term-host"></div><div id="kbd"></div>');
const after = find("term-host");
const ok = after === original && after.marker === "LIVE-XTERM";
console.log(`  host preserved: ${ok} (marker=${after && after.marker})`);
console.log(ok ? "PASS — the live terminal survives an unrelated render" : "FAIL — terminal DOM was destroyed");
process.exit(ok ? 0 : 1);
