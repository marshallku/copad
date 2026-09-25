// The ordering contract of the direct-input layer, run against the REAL code.
//
// What is being protected: a key reaches the PTY immediately (so comux chords work at all), the
// syllable an IME is still building does not, and NOTHING can overtake that syllable — a chord or
// a tap that switches panes ahead of the text would type it into the wrong pane.
//
// The whole block is lifted out of app.js and executed, rather than re-stating its rules here.
import fs from "fs";
const src = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const m = src.match(/    const IME_COMMIT_MS = [\s\S]*?\n    function wireIme\(\) \{[\s\S]*?\n    \}/);
if (!m) { console.log("FAIL: the input layer was not found in app.js"); process.exit(1); }

const results = [];
const check = (label, cond) => { results.push(!!cond); console.log(`  ${cond ? "ok  " : "FAIL"} ${label}`); };
const dec = (b) => new TextDecoder().decode(b);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// A textarea faithful enough for the parts that matter: listeners can be fired, and `blur()`
/// commits the marked text and raises `compositionend` the way Chrome and WebKit do — which is
/// the assumption the whole mid-composition ordering rests on.
function makeEl() {
  const ls = {};
  return {
    value: "", style: {}, scrollHeight: 20, marked: "", commitOnBlur: false,
    addEventListener(t, f) { (ls[t] ||= []).push(f); },
    fire(t, ev = {}) { for (const f of ls[t] || []) f(ev); },
    focus() {},
    blur() {
      if (!this.commitOnBlur) return;
      this.value += this.marked; this.marked = "";
      this.commitOnBlur = false;
      this.fire("compositionend", {});
    },
  };
}

function env({ readyState = 1 } = {}) {
  const sent = [];
  const el = makeEl();
  const wrap = { notes: [], querySelector: () => null, appendChild(n) { this.notes.push(n); } };
  const host = { addEventListener() {} };
  const state = {
    mode: "mux", ctrlSticky: false, muxNote: "", muxDown: false, term: null,
    ws: { attach: readyState === null ? null : { readyState, send: (b) => sent.push(dec(b)) } },
  };
  const document = {
    getElementById: (id) => (id === "ime" ? el : id === "ime-wrap" ? wrap : id === "term-host" ? host : null),
    createElement: () => ({ set textContent(v) { this._t = v; } }),
  };
  const api = new Function(
    "state", "document", "WebSocket", "TextEncoder", "FocusEvent", "sendMuxResize", "patchMuxChrome",
    "window",
    m[0] + `
    return { onImeKeyDown, onImeBeforeInput, emit, withOrder, keyBytes, flush, resetIme, wireIme,
             write, NON_ASCII, IME_COMMIT_MS };`
  )(state, document, { OPEN: 1 }, TextEncoder, class {}, () => {}, () => {}, {});
  api.wireIme();
  // What `term.onData` does with what `term.paste()` emits: hand it to the real `write`.
  const writeThrough = (t) => api.emit ? api.write(t) : null;
  return { sent, el, state, writeThrough, ...api };
}

// ---- the key table -------------------------------------------------------------------------
{
  const { keyBytes } = env();
  const k = (o) => keyBytes({ key: "a", ctrlKey: false, altKey: false, metaKey: false, ...o });
  check("an arrow key is a CSI sequence", k({ key: "ArrowUp" }) === "\x1b[A");
  check("Backspace is DEL, not BS", k({ key: "Backspace" }) === "\x7f");
  check("Ctrl-C is 0x03", k({ key: "c", ctrlKey: true }) === "\x03");
  check("Ctrl-space is NUL", k({ key: " ", ctrlKey: true }) === "\x00");
  check("Alt-b is an ESC prefix", k({ key: "b", altKey: true }) === "\x1bb");
  check("a plain printable is translated in keydown too", k({ key: "a" }) === "a");
  check("...including Space, which some keyboards report with no text event", k({ key: " " }) === " ");
  check("a Cmd shortcut belongs to the OS", k({ key: "c", metaKey: true }) === null);
  check("a modifier keydown is not a key", k({ key: "Shift" }) === null);
}

// ---- typing is immediate, composing is not -------------------------------------------------
{
  const e = env();
  let prevented = false;
  e.onImeKeyDown({ key: "x", preventDefault: () => { prevented = true; } });
  check("a printable keydown reaches the PTY on its own", e.sent.join("") === "x" && prevented);
  e.sent.length = 0;
  prevented = false;
  e.onImeBeforeInput({ inputType: "insertText", data: "c", preventDefault: () => { prevented = true; } });
  check("an ASCII character goes straight to the PTY", e.sent.join("") === "c" && prevented);
  check("...and does not land in the strip", e.el.value === "");

  prevented = false;
  e.onImeBeforeInput({ inputType: "insertText", data: "한", preventDefault: () => { prevented = true; } });
  check("a Hangul insertion is left in the strip instead", !prevented && e.sent.join("") === "c");
}

// ---- a control key never overtakes staged text ----------------------------------------------
{
  const e = env();
  e.el.value = "한";
  e.onImeKeyDown({ key: "Enter", preventDefault() {} });
  check("Enter writes the staged syllable BEFORE the CR", e.sent.join("|") === "한|\r");
  check("...and clears the strip", e.el.value === "");
}

// ---- Enter pressed mid-syllable -------------------------------------------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.marked = "하"; e.el.commitOnBlur = true;
  e.onImeKeyDown({ key: "Enter", isComposing: true, preventDefault() {} });
  check("the CR waits for the IME rather than being sent early", e.sent.length === 0);
  await sleep(5);
  check("after the commit it is syllable-then-CR, in that order", e.sent.join("|") === "하|\r");
}

// ---- a comux chord tapped mid-syllable ------------------------------------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.marked = "하"; e.el.commitOnBlur = true;
  e.emit("\x02n");                                  // Ctrl-b n — switches the comux tab
  check("the chord does not fire while a syllable is open", e.sent.length === 0);
  await sleep(5);
  check("the syllable lands first, so the chord cannot retarget it", e.sent.join("|") === "하|\x02n");
}

// ---- a mouse report is ordered the same way --------------------------------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.marked = "하"; e.el.commitOnBlur = true;
  e.withOrder(() => { e.state.ws.attach.send(new TextEncoder().encode("\x1b[<0;9;3M")); });
  await sleep(5);
  check("a tap on another pane cannot precede the syllable", e.sent.join("|") === "하|\x1b[<0;9;3M");
}

// ---- an IME that never commits ---------------------------------------------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.value = "하";                                 // marked text, and blur() commits nothing
  e.emit("\x02n");
  await sleep(e.IME_COMMIT_MS + 80);
  check("the chord is DISCARDED rather than fired late into another screen", e.sent.length === 0);
  check("...the typed text is kept", e.el.value === "하");
  check("...and the user is told why", e.state.muxNote.length > 0);
  e.onImeKeyDown({ key: "Enter", preventDefault() {} });
  check("...and input still works afterwards", e.sent.join("|") === "하|\r");
}

// ---- a dead socket loses nothing silently ----------------------------------------------------
{
  const e = env({ readyState: 3 });                  // CLOSED
  e.el.value = "커밋해줘";
  e.onImeKeyDown({ key: "Enter", preventDefault() {} });
  check("a closed socket sends nothing", e.sent.length === 0);
  check("...KEEPS what was typed", e.el.value === "커밋해줘");
  check("...and raises the disconnected state", e.state.muxDown === true);
}

// ---- sticky Ctrl reaches the soft keyboard ---------------------------------------------------
{
  const e = env();
  e.state.ctrlSticky = true;
  e.onImeBeforeInput({ inputType: "insertText", data: "c", preventDefault() {} });
  check("Ctrl armed on the bar folds the next typed letter", e.sent.join("") === "\x03");
  check("...and disarms itself", e.state.ctrlSticky === false);
}

// ---- teardown invalidates in-flight work ----------------------------------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.marked = "하"; e.el.commitOnBlur = true;
  e.emit("\x02n");
  e.resetIme();                                      // disposeMuxTerminal happens here
  await sleep(10);
  check("a queued chord cannot drain into the next socket", e.sent.length === 0);
}
{
  const e = env({ readyState: 3 });
  e.el.fire("compositionstart");
  e.el.value = "한";
  e.onImeKeyDown({ key: "x", preventDefault() {} });
  e.resetIme();                                      // the user taps 재연결
  check("reconnecting keeps what was typed while it was down", e.el.value === "한x");
}

// ---- a paste keeps its bracketing ------------------------------------------------------------
{
  const e = env();
  const pasted = [];
  e.state.term = { paste: (t) => pasted.push(t) };
  // A plain textarea hands the text over on `e.data` and leaves dataTransfer null.
  e.onImeBeforeInput({ inputType: "insertFromPaste", data: "echo hello", dataTransfer: null,
                       preventDefault() {} });
  check("a textarea paste is read off e.data, not dataTransfer", pasted.join("") === "echo hello");
  check("...and goes through term.paste so bracketed paste survives", e.sent.length === 0);
  let prevented = false;
  e.onImeBeforeInput({ inputType: "insertFromPaste", data: "", dataTransfer: null,
                       preventDefault: () => { prevented = true; } });
  check("a paste carrying neither is left to land rather than cancelled", !prevented);
}

// ---- a dead socket keeps what was typed -------------------------------------------------------
{
  const e = env({ readyState: 3 });
  e.onImeKeyDown({ key: "x", preventDefault() {} });
  e.onImeBeforeInput({ inputType: "insertText", data: "y", preventDefault() {} });
  check("printable keys typed while disconnected are kept, not dropped", e.el.value === "xy");
  e.onImeKeyDown({ key: "ArrowUp", preventDefault() {} });
  check("...but a control key is not queued up to fire at a later screen", e.el.value === "xy");
}

// ---- a later composition cannot resurrect an expired chord -------------------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.value = "하";                                  // never commits
  e.emit("\x02n");
  await sleep(40);
  e.el.fire("compositionstart");                      // a NEW syllable begins, mid-deadline
  e.el.marked = "글"; e.el.commitOnBlur = true;
  await sleep(e.IME_COMMIT_MS + 80);
  e.el.blur();                                        // the new one commits, much later
  await sleep(10);
  check("the expired chord does not ride out on a later commit",
        e.sent.every((x) => !x.includes("\x02n")));
}

// ---- the deadline belongs to the first queued action, not to the last -------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.value = "하";                                  // never commits
  e.emit("\x02n");
  await sleep(140);
  e.emit("\x02p");                                   // a second tap must not extend the first's wait
  await sleep(90);                                    // 230ms since the first: past the deadline
  check("a later tap cannot postpone the first one's deadline", e.sent.length === 0);
  check("...and both are dropped together", e.state.muxNote.length > 0);
}

// ---- text queued behind a composition survives a disconnect -----------------------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.value = "한";
  e.onImeKeyDown({ key: "x", preventDefault() {} });   // queued behind the syllable
  e.state.ws.attach.readyState = 3;                    // the phone sleeps mid-syllable
  e.el.commitOnBlur = true; e.el.blur();
  await sleep(10);
  check("a character queued behind a syllable is not lost with the queue", e.el.value === "한x");
}

// ---- dictation delivers whole words -----------------------------------------------------------
{
  const e = env({ readyState: 3 });
  e.onImeBeforeInput({ inputType: "insertText", data: "hello", preventDefault() {} });
  check("a multi-character insertion is kept too, not just a single key", e.el.value === "hello");
}

// ---- nothing typed is lost, wherever the send fails --------------------------------------------
{
  const e = env();
  e.el.value = "한";
  e.state.ws.attach.send = () => { throw new Error("socket died mid-send"); };
  e.onImeKeyDown({ key: "x", preventDefault() {} });
  check("a character typed onto a failing socket is kept, after the text it followed",
        e.el.value === "한x");
}
{
  const e = env({ readyState: 3 });
  e.el.value = "한";
  e.state.term = { paste: () => { throw new Error("should not be reached"); } };
  e.onImeBeforeInput({ inputType: "insertFromPaste", data: "hello", dataTransfer: null,
                       preventDefault() {} });
  check("a paste onto a dead socket is kept too", e.el.value === "한hello");
}
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.value = "한";
  e.onImeKeyDown({ key: "x", preventDefault() {} });   // queued behind the syllable
  e.state.ws.attach.readyState = 3;
  e.onImeKeyDown({ key: "y", preventDefault() {} });   // typed after it, while disconnected
  e.el.commitOnBlur = true; e.el.blur();
  await sleep(10);
  check("disconnected typing keeps the order it was typed in", e.el.value === "한xy");
}

// ---- a failure inside the drain stops it, instead of reordering what follows -------------------
{
  const e = env();
  e.el.fire("compositionstart");
  e.el.value = "한";
  e.onImeKeyDown({ key: "x", preventDefault() {} });
  e.onImeKeyDown({ key: "y", preventDefault() {} });
  const ws = e.state.ws.attach;
  const realSend = ws.send;
  let n = 0;
  ws.send = (b) => { n++; if (n === 2) throw new Error("send failed"); realSend(b); };
  e.el.commitOnBlur = true; e.el.blur();
  await sleep(10);
  check("the PTY never receives the later key ahead of the one that failed",
        e.sent.join("") === "한");
  check("...and both are back in the strip, in order", e.el.value === "xy");
}

// ---- a paste that fails mid-write is kept ------------------------------------------------------
{
  const e = env();
  // term.paste writes through onData, which reports nothing back — the only signal is the failed
  // write itself.
  e.state.term = { paste: (t) => { e.state.ws.attach.send = () => { throw new Error("died"); };
                                   e.writeThrough(t); } };
  e.onImeBeforeInput({ inputType: "insertFromPaste", data: "hello", dataTransfer: null,
                       preventDefault() {} });
  check("a paste whose socket dies mid-write is kept", e.el.value === "hello");
}

const ok = results.every(Boolean);
console.log(ok ? "PASS — keys are immediate, a syllable is never overtaken, nothing is lost quietly" : "FAIL");
process.exit(ok ? 0 : 1);
