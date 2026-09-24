import fs from "fs";
const src0 = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
// Find the template that actually contains the keybar, rather than anchoring on whatever
// renderMux's first statement happens to be this week — that anchor has broken twice.
const m = [...src0.matchAll(/renderPreservingTerminal\(`([\s\S]*?)`\);/g)]
  .find((x) => x[1].includes("kbd-bar"));
if (!m) { console.log("FAIL: renderMux template not found"); process.exit(1); }
const evaluated = new Function("state", "escapeHtml", "return `" + m[1] + "`;")(
  { muxError: "", presence: "active" }, String);
// The real decoder, copied from sendKbdBytes.
const decode = (e) => e.replace(/\\x([0-9a-fA-F]{2})/g, (_, h) => String.fromCharCode(parseInt(h, 16)))
                       .replace(/\\t/g, "\t");
const want = { "Ctrl-b": "\x02", "Ctrl-C": "\x03", "Esc": "\x1b", "Tab": "\t", "&uarr;": "\x1b[A",
               "&darr;": "\x1b[B", "&larr;": "\x1b[D", "&rarr;": "\x1b[C", "Enter": "\r" };
let ok = true, n = 0;
for (const r of evaluated.matchAll(/data-bytes="([^"]*)"[^>]*>([^<]*)</g)) {
  // innerHTML normalizes CR (and CRLF) to LF inside attribute values — emulate it, because
  // that normalization is exactly what broke the Enter button when the CR was produced by the
  // template literal instead of by the decoder.
  const attr = r[1].replace(/\r\n?/g, "\n");
  const got = decode(attr), exp = want[r[2]];
  const pass = exp !== undefined && got === exp;
  ok = ok && pass; n++;
  console.log(`${pass ? "ok  " : "FAIL"} ${r[2].padEnd(7)} attr=${JSON.stringify(attr)} -> ${JSON.stringify(got)}`);
}
console.log(ok && n === 9 ? `PASS — all ${n} keybar buttons emit the intended bytes` : "FAIL");
process.exit(ok && n === 9 ? 0 : 1);
