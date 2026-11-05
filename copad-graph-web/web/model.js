// The work graph's model, free of the DOM (decisions #204): what the page draws, as plain
// functions of the comux `graph` topic, the history pages the host fetched for one agent, and
// the play time. graph.js draws it; model.test.js checks it under node.
//
// Identity follows the wire: a conversation graph is (token, conversation, gen); within it,
// files and subagents are ids, steps are seqs. A different gen is a different graph.
"use strict";

const GraphModel = (() => {
    const KIND_ROLE = {
        prompt: "text",
        read: "blue",
        edit: "peach",
        write: "green",
        delete: "red",
        search: "mauve",
        run: "yellow",
        web: "subtext",
        spawn: "accent",
        done: "dim",
    };

    /** The most file nodes an agent view draws (the most touched win); the rest are counted. */
    const VIEW_FILES = 300;
    const ALL_FILES = 24;

    /** Shortest and longest a step plays, and the longest a whole replay runs. */
    const STEP_MIN = 140;
    const STEP_MAX = 900;
    const REPLAY_MAX = 60000;
    /** A file's flare settles over this, a beam runs this long, neighbours flicker this long. */
    const FLARE = 1400;
    const BEAM = 520;
    const SPILL = 700;

    const keyOf = (a) => `${a.token}|${a.conversation}|${a.gen}`;

    function emptyCache(agent) {
        return {
            key: keyOf(agent),
            token: agent.token,
            steps: new Map(),
            files: new Map(),
            subs: new Map(),
            oldest: 0,
            dropped: 0,
            /** `before`s that were asked for and brought nothing new. */
            exhausted: new Set(),
        };
    }

    /** Fold an overview agent (from the topic) into `cache`; returns the cache to keep (a fresh
     * one when the graph changed identity). */
    function mergeOverview(cache, agent) {
        const c = cache && cache.key === keyOf(agent) ? cache : emptyCache(agent);
        for (const f of agent.files || []) c.files.set(f.id, f);
        for (const s of agent.subagents || []) c.subs.set(s.id, s);
        for (const s of agent.steps || []) c.steps.set(s.seq, s);
        for (const s of agent.settled || []) {
            const step = c.steps.get(s.seq);
            if (step) c.steps.set(s.seq, { ...step, outcome: s.outcome });
        }
        c.oldest = agent.oldest_seq || c.oldest;
        c.dropped = agent.dropped_before || 0;
        c.counts = agent.counts;
        c.cwd = agent.cwd || "";
        c.tool = agent.tool;
        c.pending = new Set(agent.pending || []);
        c.updated = agent.updated_ms || 0;
        return c;
    }

    function mergePage(cache, page, before) {
        if (!cache || cache.key !== `${page.token}|${page.conversation}|${page.gen}`) return false;
        const had = cache.steps.size;
        for (const f of page.files || []) cache.files.set(f.id, f);
        for (const s of page.subagents || []) cache.subs.set(s.id, s);
        for (const s of page.steps || []) cache.steps.set(s.seq, s);
        cache.oldest = page.oldest_seq || cache.oldest;
        cache.dropped = page.dropped_before || 0;
        if (before != null && cache.steps.size === had) cache.exhausted.add(before);
        return true;
    }

    /** The page to ask for next, as its `before`: the newest hole in the steps held (seqs are
     * contiguous on the server from `oldest_seq` up), else the stretch before the oldest step
     * held. `null` when nothing is missing — or when what is missing was asked for and did not
     * come (`exhausted`), so a server that cannot fill a hole is not asked forever. */
    function wantBefore(cache) {
        if (!cache || cache.steps.size === 0) return null;
        const seqs = [...cache.steps.keys()].sort((a, b) => b - a);
        for (let i = 0; i < seqs.length; i++) {
            const s = seqs[i];
            const below = seqs[i + 1];
            const missing = below === undefined ? s > Math.max(cache.oldest, 1) : below !== s - 1;
            if (missing && !cache.exhausted.has(s)) return s;
        }
        return null;
    }

    /** Loaded steps the host still shows as pending but whose settlement is no longer in the
     * overview: re-ask for the page holding the oldest of them. */
    function staleSeq(cache) {
        let stale = null;
        for (const [seq, s] of cache.steps) {
            if (s.outcome === "pending" && !cache.pending.has(seq) && seq >= cache.oldest) {
                if (stale === null || seq < stale) stale = seq;
            }
        }
        return stale;
    }

    const sortedSteps = (cache) => [...cache.steps.values()].sort((a, b) => a.seq - b.seq);

    const touches = (f) => (f.reads || 0) + (f.edits || 0) + (f.writes || 0) + (f.searches || 0);

    function dirOf(path) {
        const i = path.lastIndexOf("/");
        return i <= 0 ? "" : path.slice(0, i);
    }

    const baseName = (path) => path.slice(path.lastIndexOf("/") + 1) || path;

    /** One agent as nodes and links: the agent in the middle, its subagents around it, the
     * directories its files are in as small hubs, the files sized by how often they were
     * touched. `steps` decides each file's colour (its last kind) and who touched it. */
    function agentView(cache, title) {
        const steps = sortedSteps(cache);
        const nodes = [];
        const links = [];
        const seen = new Set();
        const add = (n) => {
            if (seen.has(n.id)) return;
            seen.add(n.id);
            nodes.push(n);
        };
        add({ id: "a:0", kind: "agent", label: title || cache.tool || "agent", weight: 6 });
        for (const s of [...cache.subs.values()].sort((a, b) => a.id - b.id)) {
            add({ id: `a:${s.id}`, kind: "sub", label: s.kind || "subagent", detail: s.description, status: s.status, sub: s.id, weight: 4 });
            links.push({ source: `a:${s.parent || 0}`, target: `a:${s.id}`, kind: "spawn" });
        }
        const last = new Map();
        const by = new Map();
        for (const s of steps) {
            if (s.file == null) continue;
            last.set(s.file, s.kind);
            if (!by.has(s.file)) by.set(s.file, new Set());
            by.get(s.file).add(s.agent || 0);
        }
        const files = [...cache.files.values()].sort((a, b) => touches(b) - touches(a) || b.last_ms - a.last_ms || a.id - b.id);
        const shown = files.slice(0, VIEW_FILES);
        // A directory the agent searched is a file node already: it is also its tree's hub.
        const byDisplay = new Map(shown.map((f) => [f.display || f.abs, `f:${f.id}`]));
        for (const f of shown) {
            const display = f.display || f.abs;
            let dir = dirOf(display);
            const id = `f:${f.id}`;
            add({ id, kind: "file", label: baseName(display), path: display, abs: f.abs, file: f.id, touches: touches(f), last: last.get(f.id) || "read", edited: (f.edits || 0) + (f.writes || 0) > 0, weight: 1 + Math.sqrt(touches(f)) });
            // Its directory chain, each a hub, up to the agent.
            let child = id;
            while (dir) {
                const asFile = byDisplay.get(dir);
                if (asFile) {
                    // Its own pass links it upward.
                    links.push({ source: asFile, target: child, kind: "tree" });
                    break;
                }
                const did = `d:${dir}`;
                const existed = seen.has(did);
                add({ id: did, kind: "dir", label: baseName(dir) + "/", path: dir, weight: 0.6 });
                links.push({ source: did, target: child, kind: "tree" });
                child = did;
                if (existed) break;
                dir = dirOf(dir);
            }
            if (!dir && !seen.has(`${child}>root`)) {
                seen.add(`${child}>root`);
                links.push({ source: "a:0", target: child, kind: "root" });
            }
            for (const agent of by.get(f.id) || []) {
                if (agent !== 0 && seen.has(`a:${agent}`)) links.push({ source: `a:${agent}`, target: id, kind: "touch" });
            }
        }
        return { nodes, links, hidden: files.length - shown.length };
    }

    /** Every live agent at once: a hub per space, its agents around it, each agent's newest
     * files; a file two agents touched is ONE node linked to both. */
    function allView(graph, agents) {
        const info = new Map((agents || []).map((a) => [a.token, a]));
        const nodes = [];
        const links = [];
        const seen = new Map();
        const add = (n) => {
            if (!seen.has(n.id)) {
                seen.set(n.id, n);
                nodes.push(n);
            }
            return seen.get(n.id);
        };
        for (const a of graph.agents || []) {
            const meta = info.get(a.token) || {};
            const space = a.space_id || meta.space_id || "?";
            add({ id: `s:${space}`, kind: "space", label: meta.space || space, weight: 5 });
            add({ id: `t:${a.token}`, kind: "agent", label: meta.title || a.tool, token: a.token, status: meta.status || "", tool: a.tool, weight: 4 });
            links.push({ source: `s:${space}`, target: `t:${a.token}`, kind: "root" });
            const last = new Map();
            for (const s of a.steps || []) if (s.file != null) last.set(s.file, s.kind);
            const files = [...(a.files || [])].sort((x, y) => y.last_ms - x.last_ms).slice(0, ALL_FILES);
            for (const f of files) {
                const id = `F:${f.abs}`;
                const n = add({ id, kind: "file", label: baseName(f.display || f.abs), path: f.display, abs: f.abs, touches: 0, last: last.get(f.id) || "read", owners: new Set(), weight: 1 });
                n.touches += touches(f);
                n.weight = 1 + Math.sqrt(n.touches);
                n.owners.add(a.token);
                if ((f.edits || 0) + (f.writes || 0) > 0) n.edited = true;
                links.push({ source: `t:${a.token}`, target: id, kind: "touch" });
            }
        }
        for (const n of nodes) if (n.owners) n.shared = n.owners.size > 1;
        return { nodes, links, hidden: 0 };
    }

    /** When each step starts playing in a replay: the real gaps, clamped, then squeezed to fit
     * [`REPLAY_MAX`]. */
    function timeline(steps) {
        const at = [];
        steps.forEach((s, i) => {
            if (i === 0) return at.push(0);
            const gap = Math.min(Math.max((s.at_ms || 0) - (steps[i - 1].at_ms || 0), STEP_MIN), STEP_MAX);
            at.push(at[i - 1] + gap);
        });
        const total = at.length ? at[at.length - 1] : 0;
        if (total > REPLAY_MAX) {
            const k = REPLAY_MAX / total;
            for (let i = 0; i < at.length; i++) at[i] *= k;
        }
        return at;
    }

    function stepAt(at, t) {
        let lo = 0;
        let hi = at.length - 1;
        let found = -1;
        while (lo <= hi) {
            const mid = (lo + hi) >> 1;
            if (at[mid] <= t) {
                found = mid;
                lo = mid + 1;
            } else hi = mid - 1;
        }
        return found;
    }

    /** What shows when the cursor is on step `index` (the steps after it not yet happened) at
     * `since` ms into that step: which files exist yet, the lit ones and their flare, the beam.
     * `reduced` drops the motion and keeps what is lit. */
    function frame(steps, index, since, reduced) {
        since = Math.max(0, since);
        const present = new Set();
        const lit = new Map();
        for (let i = 0; i <= index && i < steps.length; i++) {
            const s = steps[i];
            if (s.file != null) present.add(s.file);
        }
        const beams = [];
        const cur = steps[index];
        if (cur && cur.file != null) {
            lit.set(cur.file, { kind: cur.kind, flare: reduced ? 0 : Math.max(0, 1 - since / FLARE), outcome: cur.outcome });
            if (!reduced && since < BEAM) beams.push({ from: cur.agent || 0, file: cur.file, kind: cur.kind, progress: since / BEAM });
        }
        // The few steps just before stay faintly lit, so a run of reads reads as a trail.
        for (let i = index - 1, k = 1; i >= 0 && k <= 4; i--, k++) {
            const s = steps[i];
            if (s.file != null && !lit.has(s.file)) lit.set(s.file, { kind: s.kind, flare: 0, trail: 1 - k / 5, outcome: s.outcome });
        }
        const spill = !reduced && since < SPILL ? 1 - since / SPILL : 0;
        return { present, lit, beams, spill, step: cur || null };
    }

    function describe(step, files, subs) {
        if (!step) return "";
        const f = step.file != null ? files.get(step.file) : null;
        const target = f ? f.display || f.abs : step.label || "";
        const who = step.agent ? ` · ${(subs.get(step.agent) || {}).kind || "subagent"}` : "";
        const verb = { prompt: "Asked", read: "Read", edit: "Edited", write: "Wrote", delete: "Deleted", search: "Searched", run: "Ran", web: "Fetched", spawn: "Started", done: "Finished turn" }[step.kind] || step.kind;
        const mark = step.outcome === "failed" ? " ✕" : step.outcome === "pending" ? " …" : "";
        return `${verb} ${target}${who}${mark}`.trim();
    }

    return {
        KIND_ROLE,
        VIEW_FILES,
        ALL_FILES,
        FLARE,
        BEAM,
        keyOf,
        emptyCache,
        mergeOverview,
        mergePage,
        wantBefore,
        staleSeq,
        sortedSteps,
        agentView,
        allView,
        timeline,
        stepAt,
        frame,
        describe,
        baseName,
    };
})();

if (typeof module !== "undefined") module.exports = GraphModel;
