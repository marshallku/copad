// Exercise the REAL Enter decision and the REAL submit path against a controllable socket.
import fs from "fs";
const src0 = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const grab = (re) => { const m = src0.match(re); if (!m) throw new Error("not found " + re); return m[0]; };
const code = [
  grab(/    const COMPOSE_CONFIRM_GRACE_MS = \d+;/),
  grab(/    function shouldSubmitOnEnter\(e, now, lastEnd\) \{[\s\S]*?\n    \}/),
  grab(/    function composeEl\(\) \{[\s\S]*?\n    \}/),
  grab(/    function submitCompose\(\) \{[\s\S]*?\n    \}/),
].join("\n");

const results = [];
const check = (label, cond) => { results.push(cond); console.log(`  ${cond ? "ok  " : "FAIL"} ${label}`); };

// --- the Enter decision ---
const env = new Function("return (" + "(" + `() => { ${code}
  return { shouldSubmitOnEnter, COMPOSE_CONFIRM_GRACE_MS };
}` + ")()" + ")")();
const { shouldSubmitOnEnter, COMPOSE_CONFIRM_GRACE_MS } = env;
const key = (o = {}) => ({ key: "Enter", shiftKey: false, isComposing: false, keyCode: 13, ...o });

check("a plain Enter well after composition submits",
      shouldSubmitOnEnter(key(), 10000, 0) === true);
check("Enter while composing does not",
      shouldSubmitOnEnter(key({ isComposing: true }), 10000, 0) === false);
check("keyCode 229 (IME) does not",
      shouldSubmitOnEnter(key({ keyCode: 229 }), 10000, 0) === false);
check("Shift+Enter inserts a newline instead",
      shouldSubmitOnEnter(key({ shiftKey: true }), 10000, 0) === false);
check("the Enter that CONFIRMS a candidate does not submit",
      shouldSubmitOnEnter(key(), 1000, 1000 - (COMPOSE_CONFIRM_GRACE_MS - 10)) === false);
check("...but an Enter after the grace window does",
      shouldSubmitOnEnter(key(), 1000, 1000 - (COMPOSE_CONFIRM_GRACE_MS + 10)) === true);
check("a non-Enter key never submits",
      shouldSubmitOnEnter(key({ key: "a" }), 10000, 0) === false);

// --- the submit path ---
const sent = [];
const makeEnv = (readyState) => {
  const el = { value: "", style: {}, focus() {}, scrollHeight: 20 };
  const state = { ws: { attach: readyState === null ? null : { readyState, send: (b) => sent.push(b) } }, composeNote: "" };
  const document = { getElementById: (id) => (id === "compose" ? el : null) };
  const fn = new Function("state", "document", "WebSocket", "TextEncoder", "renderComposeNote", "autoGrow",
    code + "; return submitCompose;")(state, document, { OPEN: 1 }, TextEncoder,
      () => {}, () => {});
  return { el, state, submitCompose: fn };
};

let e = makeEnv(1);
e.el.value = "계속 진행해봐";
e.submitCompose();
check("a composed Korean line is sent whole, with a trailing CR",
      new TextDecoder().decode(sent[0]) === "계속 진행해봐\r");
check("...and the draft is cleared on success", e.el.value === "");

sent.length = 0;
e = makeEnv(3);                    // CLOSED
e.el.value = "커밋해줘";
e.submitCompose();
check("a closed socket sends nothing", sent.length === 0);
check("...and KEEPS the draft", e.el.value === "커밋해줘");
check("...and says why", e.state.composeNote.length > 0);

const ok = results.every(Boolean);
console.log(ok ? "PASS — Korean composes, confirming Enter is not a submit, drafts survive failure" : "FAIL");
process.exit(ok ? 0 : 1);
