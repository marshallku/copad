// node --test copad-graph-web/web/*.test.js
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const M = require("./model.js");

const file = (id, display, extra = {}) => ({ id, abs: `/r/${display}`, display, reads: 0, edits: 0, writes: 0, searches: 0, first_ms: 0, last_ms: id, ...extra });
const step = (seq, kind, extra = {}) => ({ seq, at_ms: seq * 1000, kind, agent: 0, outcome: "ok", ...extra });

const agent = (extra = {}) => ({
    token: "tok",
    tool: "claude",
    conversation: "c1",
    gen: "g1",
    cwd: "/r",
    counts: { files: 2, reads: 1, edits: 1, writes: 0, searches: 0, runs: 0, prompts: 1, subagents: 1, failed: 0, steps: 4 },
    files: [file(0, "src/a.rs", { reads: 1 }), file(1, "src/b.rs", { edits: 1 })],
    subagents: [{ id: 1, kind: "Explore", description: "look", parent: 0, status: "running" }],
    steps: [step(3, "read", { file: 0 }), step(4, "edit", { file: 1, agent: 1, outcome: "pending" })],
    pending: [4],
    settled: [],
    oldest_seq: 1,
    ...extra,
});

test("an overview folds in and a settlement updates a loaded step", () => {
    let c = M.mergeOverview(null, agent());
    assert.equal(c.steps.size, 2);
    assert.equal(c.steps.get(4).outcome, "pending");
    c = M.mergeOverview(c, agent({ steps: [], pending: [], settled: [{ seq: 4, outcome: "failed" }] }));
    assert.equal(c.steps.get(4).outcome, "failed");
    assert.equal(c.steps.size, 2, "steps the overview no longer carries stay");
});

test("a new gen starts the cache over", () => {
    const c = M.mergeOverview(null, agent());
    const d = M.mergeOverview(c, agent({ gen: "g2", steps: [step(1, "prompt", { label: "hi" })] }));
    assert.notEqual(c, d);
    assert.deepEqual([...d.steps.keys()], [1]);
});

test("history pages merge until the oldest step, and a stale page is ignored", () => {
    const c = M.mergeOverview(null, agent());
    assert.equal(M.wantBefore(c), 3);
    const page = { token: "tok", conversation: "c1", gen: "g1", files: [], subagents: [], steps: [step(1, "prompt"), step(2, "read", { file: 0 })], oldest_seq: 1, next_before: null };
    assert.equal(M.mergePage(c, { ...page, gen: "old" }), false);
    assert.equal(M.mergePage(c, page), true);
    assert.deepEqual(M.sortedSteps(c).map((s) => s.seq), [1, 2, 3, 4]);
    assert.equal(M.wantBefore(c), null);
});

test("a hole between history and a later overview is asked for", () => {
    const c = M.mergeOverview(null, agent({ steps: [step(1, "prompt"), step(2, "read", { file: 0 })] }));
    M.mergeOverview(c, agent({ steps: [step(7, "read", { file: 0 }), step(8, "read", { file: 0 })] }));
    assert.equal(M.wantBefore(c), 7, "steps 3-6 are missing");
    const page = { token: "tok", conversation: "c1", gen: "g1", files: [], subagents: [], oldest_seq: 1 };
    M.mergePage(c, { ...page, steps: [3, 4, 5, 6].map((n) => step(n, "read")) }, 7);
    assert.equal(M.wantBefore(c), null);
    // A hole the server cannot fill is not asked for again.
    M.mergeOverview(c, agent({ steps: [step(12, "read")] }));
    assert.equal(M.wantBefore(c), 12);
    M.mergePage(c, { ...page, steps: [] }, 12);
    assert.equal(M.wantBefore(c), null);
});

test("a pending step whose settlement scrolled out is asked for again", () => {
    const c = M.mergeOverview(null, agent());
    assert.equal(M.staleSeq(c), null, "still pending on the wire");
    M.mergeOverview(c, agent({ steps: [], pending: [], settled: [] }));
    assert.equal(M.staleSeq(c), 4);
});

test("the agent view hangs files off directory hubs, and a subagent off its parent", () => {
    const c = M.mergeOverview(null, agent());
    const v = M.agentView(c, "claude");
    const ids = v.nodes.map((n) => n.id).sort();
    assert.deepEqual(ids, ["a:0", "a:1", "d:src", "f:0", "f:1"]);
    const has = (s, t, k) => v.links.some((l) => l.source === s && l.target === t && l.kind === k);
    assert.ok(has("a:0", "a:1", "spawn"));
    assert.ok(has("d:src", "f:0", "tree") && has("d:src", "f:1", "tree"));
    assert.ok(has("a:0", "d:src", "root"));
    assert.ok(has("a:1", "f:1", "touch"), "the subagent's own file");
    const b = v.nodes.find((n) => n.id === "f:1");
    assert.equal(b.last, "edit");
    assert.equal(b.edited, true);
});

test("a searched directory is its files' hub, not a second node", () => {
    const files = [file(0, "src/a.rs", { reads: 1 }), file(1, "src", { searches: 1 })];
    const c = M.mergeOverview(null, agent({ files, steps: [], subagents: [] }));
    const v = M.agentView(c, "x");
    assert.ok(!v.nodes.some((n) => n.id === "d:src"));
    assert.ok(v.links.some((l) => l.source === "f:1" && l.target === "f:0" && l.kind === "tree"));
    assert.ok(v.links.some((l) => l.source === "a:0" && l.target === "f:1" && l.kind === "root"));
});

test("the agent view keeps the most touched files and counts the rest", () => {
    const files = Array.from({ length: M.VIEW_FILES + 5 }, (_, i) => file(i, `f${i}.rs`, { reads: i }));
    const c = M.mergeOverview(null, agent({ files, steps: [], subagents: [] }));
    const v = M.agentView(c, "x");
    assert.equal(v.hidden, 5);
    assert.ok(!v.nodes.some((n) => n.id === "f:0"), "the least touched is left out");
});

test("the all view shares a file two agents touched", () => {
    const a = agent();
    const b = agent({ token: "tok2", space_id: "s2", files: [file(7, "src/a.rs", { edits: 2 })], steps: [], subagents: [] });
    const v = M.allView({ agents: [{ ...a, space_id: "s1" }, b] }, [{ token: "tok", space: "work", title: "tab 1" }]);
    const shared = v.nodes.filter((n) => n.kind === "file" && n.shared);
    assert.deepEqual(shared.map((n) => n.abs), ["/r/src/a.rs"]);
    assert.equal(shared[0].touches, 3);
    assert.ok(v.nodes.some((n) => n.id === "s:s1" && n.label === "work"));
    assert.ok(v.links.some((l) => l.source === "t:tok2" && l.target === "F:/r/src/a.rs"));
});

test("the timeline clamps real gaps and fits a long replay", () => {
    const at = M.timeline([step(1, "read"), step(2, "read", { at_ms: 1010 }), step(3, "read", { at_ms: 99999 })]);
    assert.deepEqual(at, [0, 140, 1040]);
    const many = Array.from({ length: 1000 }, (_, i) => step(i + 1, "read", { at_ms: i * 10000 }));
    const long = M.timeline(many);
    assert.ok(long[long.length - 1] <= 60000 + 1e-6);
    assert.equal(M.stepAt(at, -1), -1);
    assert.equal(M.stepAt(at, 500), 1);
    assert.equal(M.stepAt(at, 5000), 2);
});

test("a frame lights the current file with a beam, the trail behind it, and nothing ahead", () => {
    const steps = [step(1, "read", { file: 0 }), step(2, "edit", { file: 1, agent: 1 }), step(3, "read", { file: 2 })];
    const f = M.frame(steps, 1, 100, false);
    assert.deepEqual([...f.present].sort(), [0, 1]);
    assert.ok(f.lit.get(1).flare > 0.9);
    assert.equal(f.lit.get(0).flare, 0);
    assert.deepEqual(f.beams.map((b) => [b.from, b.file]), [[1, 1]]);
    const r = M.frame(steps, 1, 100, true);
    assert.equal(r.beams.length, 0);
    assert.equal(r.lit.get(1).flare, 0);
});

test("a step describes itself", () => {
    const c = M.mergeOverview(null, agent());
    assert.equal(M.describe(c.steps.get(4), c.files, c.subs), "Edited src/b.rs · Explore …");
    assert.equal(M.describe(step(9, "run", { label: "cargo test", outcome: "failed" }), c.files, c.subs), "Ran cargo test ✕");
});

test("graph.js never builds markup from data", () => {
    const fs = require("node:fs");
    const src = fs.readFileSync(require("node:path").join(__dirname, "graph.js"), "utf8");
    for (const bad of ["innerHTML", "outerHTML", "insertAdjacentHTML", "eval(", "new Function", "document.write"]) {
        assert.ok(!src.includes(bad), `graph.js uses ${bad}`);
    }
});
