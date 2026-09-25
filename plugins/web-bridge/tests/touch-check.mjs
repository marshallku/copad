// The two touch paths, which no desktop test could have caught: a key bar whose buttons only
// answered `click` (iOS never generates one when touchstart's default is prevented), and a
// terminal that nothing could scroll.
import fs from "fs";
const src = fs.readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const results = [];
const check = (l, c) => { results.push(!!c); console.log(`  ${c ? "ok  " : "FAIL"} ${l}`); };

const grab = (re) => { const m = src.match(re); if (!m) throw new Error("not found: " + re); return m[0]; };

// --- the regression itself ---
check("no touchstart handler cancels the synthesized click any more",
      !/addEventListener\("touchstart", *\(?e\)? *=> *e\.preventDefault/.test(src));
check("every key bar button is activated through the shared touch+click path",
      /onActivate\(btn, \(\) => \{ sendKbdBytes/.test(src));
check("...including sticky Ctrl, which had its own handler",
      /onActivate\(document\.getElementById\("ctrl"\)/.test(src));

// --- the tap machine, executed ---
{
  const fns = [grab(/    const TAP_SLOP_PX = \d+;/),
               grab(/    const TAP_CLICK_SUPPRESS_MS = \d+;/),
               grab(/    function tapGesture\([\s\S]*?\n    \}/),
               grab(/    function shouldFireClick\([\s\S]*?\n    \}/)].join("\n");
  const { tapGesture, shouldFireClick, TAP_SLOP_PX, TAP_CLICK_SUPPRESS_MS } =
    new Function(fns + "; return { tapGesture, shouldFireClick, TAP_SLOP_PX, TAP_CLICK_SUPPRESS_MS };")();
  const t = (x, y, id = 1) => ({ identifier: id, clientX: x, clientY: y });

  let g = tapGesture(TAP_SLOP_PX);
  g.start(t(100, 200), 1);
  g.move(t(103, 202), 1);
  check("a still finger is a tap", g.end(t(103, 202), 0) === true);

  g = tapGesture(TAP_SLOP_PX);
  g.start(t(100, 200), 1);
  g.move(t(180, 202), 1);          // dragged the bar sideways…
  g.move(t(101, 201), 1);          // …and came back to where it started
  check("a finger that wandered and returned is a scroll, not a tap",
        g.end(t(101, 201), 0) === false);

  g = tapGesture(TAP_SLOP_PX);
  g.start(t(100, 200), 1);
  g.move(t(101, 201), 2);          // a second finger lands, possibly on another element
  check("a second finger anywhere cancels the gesture", g.end(t(101, 201), 0) === false);

  g = tapGesture(TAP_SLOP_PX);
  g.start(t(100, 200), 1);
  check("a lift with another finger still down is not a tap", g.end(t(100, 200), 1) === false);

  g = tapGesture(TAP_SLOP_PX);
  g.start(t(100, 200, 4), 1);
  check("a different finger's lift does not fire it", g.end(t(100, 200, 9), 0) === false);

  check("a click right after a touch fire is suppressed",
        !shouldFireClick(1000, 1000 - (TAP_CLICK_SUPPRESS_MS - 50)));
  check("an ordinary mouse click still fires", shouldFireClick(10000, 0));
}

// --- the drag machine, executed ---
{
  const fns = [grab(/    const TOUCH_AXIS_SLOP_PX = \d+;/),
               grab(/    function wheelSteps\([\s\S]*?\n    \}/),
               grab(/    function scrollGesture\([\s\S]*?\n    \}/)].join("\n");
  const { scrollGesture, wheelSteps, TOUCH_AXIS_SLOP_PX } =
    new Function(fns + "; return { scrollGesture, wheelSteps, TOUCH_AXIS_SLOP_PX };")();
  const t = (x, y, id = 1) => ({ identifier: id, clientX: x, clientY: y });
  const ROW = 17;

  check("a drag shorter than a row emits nothing yet", wheelSteps(9, ROW).steps === 0);
  check("...and keeps the remainder, so slow drags still accumulate", wheelSteps(9, ROW).rest === 9);
  check("a drag of three rows is three lines", wheelSteps(52, ROW).steps === 3);
  check("dragging the other way is negative", wheelSteps(-52, ROW).steps === -3);

  // The defect: the movement that latched the axis used to be thrown away, so a gesture
  // delivered as ONE big touchmove scrolled nothing.
  let g = scrollGesture(TOUCH_AXIS_SLOP_PX);
  g.start(t(200, 400), 1);
  check("one big move scrolls by everything it moved", g.move(t(200, 451), 1, ROW).steps === 3);

  g = scrollGesture(TOUCH_AXIS_SLOP_PX);
  g.start(t(200, 400), 1);
  let total = 0;
  for (let i = 1; i <= 10; i++) { const r = g.move(t(200, 400 + i * 5), 1, ROW); if (r) total += r.steps; }
  check("ten 5px moves add up to the same 2 lines a 50px move would", total === 2);

  g = scrollGesture(TOUCH_AXIS_SLOP_PX);
  g.start(t(200, 400), 1);
  g.move(t(260, 402), 1, ROW);                     // latched horizontal
  check("a pan that later curves downward never starts scrolling",
        g.move(t(262, 500), 1, ROW) === null);

  g = scrollGesture(TOUCH_AXIS_SLOP_PX);
  g.start(t(200, 400), 1);
  g.move(t(200, 460), 2, ROW);                     // a second finger joins
  check("a second finger stops the drag emitting wheels",
        g.move(t(200, 520), 1, ROW) === null);

  g = scrollGesture(TOUCH_AXIS_SLOP_PX);
  g.start(t(200, 400, 2), 1);
  check("another finger's move is not this gesture's", g.move(t(200, 460, 8), 1, ROW) === null);
}

// --- the wheel we dispatch is the one xterm can act on ---
check("the wheel is dispatched in LINE mode, so xterm's own row-height accumulator is not guessed at",
      /deltaMode: 1,/.test(src));
check("...at the touch point, so comux knows which half was scrolled",
      /clientX: t\.clientX, clientY: t\.clientY/.test(src));
check("a move the machine did not claim is left alone — no preventDefault, so the host can pan",
      /const r = drag\.move\(e\.touches\[0\], e\.touches\.length, rowHeightPx\(\)\);\n\s*if \(!r\) return;/.test(src));
check("the terminal's gesture is cancelled on touchcancel, not only on a clean end",
      /host\.addEventListener\("touchcancel", \(\) => drag\.end\(\)\)/.test(src));

const ok = results.every(Boolean);
console.log(ok ? "PASS — the bar answers a tap and the terminal answers a drag" : "FAIL");
process.exit(ok ? 0 : 1);
