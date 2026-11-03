// The work graph page (decisions #204): what an AI agent in comux has read, edited and searched,
// drawn as a live force-directed graph with a replayable timeline under it. copad loads it in a
// web view beside the terminal (macOS, Linux, iOS) and drives it through window.comuxGraph; it
// hears back through webkit.messageHandlers.comux (or the parent frame): `ready`, `jump` (an
// agent was clicked), `open` (a file was double-clicked), `history` (load older steps).
//
// Everything that comes from comux — paths, labels, prompts — is untrusted program output: it is
// only ever drawn on the canvas or set as textContent, never parsed as markup.
"use strict";

(() => {
    const M = GraphModel;
    const $ = (id) => document.getElementById(id);

    const canvas = $("graph");
    const ctx = canvas.getContext("2d");
    const stage = $("stage");
    const tip = $("tip");
    const picker = $("agent");
    const search = $("search");
    const modeAgent = $("mode-agent");
    const modeAll = $("mode-all");
    const count = $("count");
    const card = $("card");
    const strip = $("timeline");
    const sctx = strip.getContext("2d");
    const playButton = $("play");
    const liveButton = $("live");
    const summary = $("summary");
    const empty = $("empty");

    // ---- the bridge -----------------------------------------------------------------------

    /** Every message carries the host's epoch (`setHost`), so the host can drop one that was
     * queued for a connection it has since left. */
    const post = (message) => {
        message = { ...message, epoch: host.epoch ?? null };
        const w = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.comux;
        if (w) w.postMessage(message);
        else if (window.parent !== window) window.parent.postMessage({ comuxGraph: message }, "*");
    };

    // ---- state ----------------------------------------------------------------------------

    let topic = { agents: [] };
    let agentsInfo = [];
    /** token → the merged cache of that agent's graph. */
    const caches = new Map();
    let mode = "agent";
    let selected = null;
    let host = { local: false };
    let reduced = window.matchMedia ? window.matchMedia("(prefers-reduced-motion: reduce)").matches : false;
    let query = "";

    let view = { nodes: [], links: [], hidden: 0 };
    /** id → { x, y, vx, vy, node, fixed } */
    const points = new Map();
    let shapeKey = "";
    let alpha = 0;
    let transform = { x: 0, y: 0, k: 1 };
    let viewTouched = false;
    let size = { w: 1, h: 1, dpr: 1 };

    let steps = [];
    /** `steps` as the cursor was last placed in. */
    let prevSteps = [];
    let at = [];
    let cursor = -1;
    let cursorSince = 0;
    let live = true;
    let playing = false;
    let playStart = 0;
    let playFrom = 0;
    let selectedFile = null;
    /** Files lit as steps arrive live: file node id → { t, kind, from } */
    const flares = new Map();
    let lastSeqByKey = new Map();
    let hovered = null;
    let inflight = null;
    /** A retry after a failed history request, and whether this connection's comux has no
     * `graph` command at all. */
    let retryTimer = 0;
    let historyUnsupported = false;
    let resets = 0;

    // ---- colours --------------------------------------------------------------------------

    const roles = {
        base: "#1e1e2e", text: "#cdd6f4", subtext: "#bac2de", dim: "#a6adc8", rule: "#585b70", hover: "#45475a",
        accent: "#89b4fa", mauve: "#cba6f7", peach: "#fab387", green: "#a6e3a1", yellow: "#f9e2af", blue: "#89b4fa", red: "#f38ba8",
    };
    let font = "ui-monospace, Menlo, monospace";
    let fontSize = 11;
    const kindColor = (kind) => roles[M.KIND_ROLE[kind] || "dim"] || roles.dim;
    const SUB_ROLES = ["mauve", "green", "peach", "yellow", "blue"];
    const subColor = (id) => roles[SUB_ROLES[(id - 1) % SUB_ROLES.length]];

    function alphaColor(hex, a) {
        const m = /^#?([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex.trim());
        if (!m) return hex;
        return `rgba(${parseInt(m[1], 16)},${parseInt(m[2], 16)},${parseInt(m[3], 16)},${a})`;
    }

    // ---- data in --------------------------------------------------------------------------

    function setData(data) {
        topic = (data && data.graph) || { agents: [] };
        agentsInfo = (data && data.agents) || agentsInfo;
        const now = performance.now();
        const live_ = new Set();
        for (const a of topic.agents || []) {
            live_.add(a.token);
            const before = caches.get(a.token);
            const c = M.mergeOverview(before, a);
            caches.set(a.token, c);
            // New steps since the last value: they play as they arrive.
            const key = c.key;
            const prev = lastSeqByKey.get(key);
            const newest = (a.steps || []).reduce((m, s) => Math.max(m, s.seq), 0);
            if (prev !== undefined) {
                for (const s of a.steps || []) {
                    if (s.seq <= prev || s.file == null) continue;
                    const id = mode === "all" ? `F:${(c.files.get(s.file) || {}).abs}` : `f:${s.file}`;
                    if (mode === "all" || a.token === selected) flares.set(id, { t: now, kind: s.kind, from: mode === "all" ? `t:${a.token}` : `a:${s.agent || 0}` });
                }
            }
            lastSeqByKey.set(key, Math.max(prev || 0, newest));
        }
        for (const t of [...caches.keys()]) if (!live_.has(t)) caches.delete(t);
        if (!selected || !caches.has(selected)) {
            const focused = agentsInfo.find((a) => a.focused && caches.has(a.token));
            selected = focused ? focused.token : (topic.agents[0] || {}).token || null;
        }
        fillPicker();
        rebuild();
        askHistory();
        kick();
    }

    function addHistory(page) {
        if (!page) return;
        clearTimeout(retryTimer);
        const asked = inflight && inflight.token === page.token ? inflight.before : null;
        inflight = null;
        const c = caches.get(page.token);
        if (c && M.mergePage(c, page, asked)) rebuild();
        askHistory();
    }

    /** One request at a time: older pages of the selected agent, then a refresh of a pending
     * step whose settlement scrolled out of the overview. */
    function askHistory() {
        // A host that never answered (a dropped reply) must not stall loading for good.
        if (inflight && performance.now() - inflight.t < 5000) return;
        inflight = null;
        if (historyUnsupported || mode !== "agent" || !selected) return;
        const c = caches.get(selected);
        if (!c) return;
        // Nothing held yet but the agent has steps: the newest page first.
        const before = M.wantBefore(c) ?? (c.steps.size === 0 && (c.counts || {}).steps > 0 ? undefined : null);
        if (before !== null) {
            inflight = { token: selected, before, t: performance.now() };
            post({ type: "history", token: selected, before: before === undefined ? null : before });
            return;
        }
        const stale = M.staleSeq(c);
        if (stale !== null) {
            inflight = { token: selected, before: stale + 1, t: performance.now() };
            post({ type: "history", token: selected, before: stale + 1, limit: 1 });
        }
    }

    /** A history request failed: `unsupported` (this comux has no `graph` command) stops asking
     * for this connection; `not_found`/`no-graph` gives that hole up; anything else is retried
     * after a pause, by a timer, since nothing else may come along to ask again. */
    function historyFailed(f) {
        const asked = inflight;
        inflight = null;
        clearTimeout(retryTimer);
        const code = (f && f.code) || "";
        if (code === "unsupported") {
            historyUnsupported = true;
            return;
        }
        const c = asked ? caches.get(asked.token) : null;
        if ((code === "not_found" || code === "no-graph") && c && asked.before != null) {
            c.exhausted.add(asked.before);
            askHistory();
            return;
        }
        retryTimer = setTimeout(askHistory, 5000);
    }

    /** Everything this page holds belongs to one connection: drop it (another comux, or the
     * same one reconnected). */
    function reset() {
        resets++;
        clearTimeout(retryTimer);
        clearInterval(playTimer);
        caches.clear();
        lastSeqByKey = new Map();
        flares.clear();
        points.clear();
        topic = { agents: [] };
        agentsInfo = [];
        selected = null;
        selectedFile = null;
        inflight = null;
        historyUnsupported = false;
        shapeKey = "";
        viewTouched = false;
        live = true;
        playing = false;
        prevSteps = [];
        fillPicker();
        rebuild();
        drawCard();
        kick();
    }

    function fillPicker() {
        const want = (topic.agents || []).map((a) => a.token);
        const have = [...picker.options].map((o) => o.value);
        const info = new Map(agentsInfo.map((a) => [a.token, a]));
        const label = (t) => {
            const a = info.get(t) || {};
            const g = caches.get(t) || {};
            return `${a.space ? a.space + " · " : ""}${a.title || t} — ${g.tool || a.tool || ""}`;
        };
        if (want.join("\n") !== have.join("\n")) {
            while (picker.firstChild) picker.removeChild(picker.firstChild);
            for (const t of want) {
                const o = document.createElement("option");
                o.value = t;
                picker.appendChild(o);
            }
        }
        for (const o of picker.options) o.textContent = label(o.value);
        picker.value = selected || "";
        picker.hidden = mode !== "agent" || want.length === 0;
    }

    // ---- view -----------------------------------------------------------------------------

    function titleOf(token) {
        const a = agentsInfo.find((x) => x.token === token);
        return a ? a.title || a.tool : (caches.get(token) || {}).tool;
    }

    function rebuild() {
        // Bring a replay up to now on the OLD timeline, and keep how far into its step it is.
        const now = performance.now();
        advance(now);
        const into = playing ? playFrom + (now - playStart) - (at[cursor] || 0) : 0;
        const c = selected ? caches.get(selected) : null;
        if (mode === "agent") {
            view = c ? M.agentView(c, titleOf(selected)) : { nodes: [], links: [], hidden: 0 };
            steps = c ? M.sortedSteps(c) : [];
        } else {
            view = M.allView(topic, agentsInfo);
            steps = [];
        }
        // The cursor is a STEP, not a position: older history landing in front of it, or
        // steps arriving after it, must leave it on the same seq (and a replay going on).
        const held = cursor >= 0 && cursor < prevSteps.length ? prevSteps[cursor].seq : null;
        at = M.timeline(steps);
        if (live || held === null) setCursor(steps.length - 1, false);
        else {
            const i = steps.findIndex((s) => s.seq >= held);
            cursor = i < 0 ? steps.length - 1 : i;
            if (playing) {
                playFrom = (at[cursor] || 0) + Math.max(0, into);
                playStart = now;
            }
            drawCard();
        }
        prevSteps = steps;
        syncPoints();
        empty.hidden = view.nodes.length > 0;
        empty.textContent = mode === "agent" && !c ? "No agent work to show yet." : "Nothing yet.";
        const files = view.nodes.filter((n) => n.kind === "file").length;
        count.textContent = `${files} files${view.hidden ? ` (+${view.hidden})` : ""}`;
        writeSummary(c);
    }

    function writeSummary(c) {
        if (mode === "all") {
            const shared = view.nodes.filter((n) => n.shared).length;
            const n = (topic.agents || []).length;
            summary.textContent = `${n} agent${n === 1 ? "" : "s"}${shared ? ` · ${shared} shared file${shared === 1 ? "" : "s"}` : ""}${topic.omitted_agents ? ` · +${topic.omitted_agents} not shown` : ""}`;
            return;
        }
        if (!c || !c.counts) {
            summary.textContent = "";
            return;
        }
        const k = c.counts;
        const parts = [`${k.files} files`, `${k.reads} reads`, `${k.edits + k.writes} edits`, `${k.searches} searches`];
        if (k.subagents) parts.push(`${k.subagents} subagents`);
        if (k.failed) parts.push(`${k.failed} failed`);
        if (c.dropped) parts.push(`${c.dropped} earlier steps not kept`);
        summary.textContent = parts.join(" · ");
    }

    function syncPoints() {
        const ids = new Set(view.nodes.map((n) => n.id));
        for (const id of [...points.keys()]) if (!ids.has(id)) points.delete(id);
        const neighbour = new Map();
        for (const l of view.links) {
            if (!neighbour.has(l.target)) neighbour.set(l.target, l.source);
            if (!neighbour.has(l.source)) neighbour.set(l.source, l.target);
        }
        let fresh = 0;
        for (const n of view.nodes) {
            const p = points.get(n.id);
            if (p) {
                p.node = n;
                continue;
            }
            fresh++;
            const near = points.get(neighbour.get(n.id));
            const angle = Math.random() * Math.PI * 2;
            const d = near ? 18 + Math.random() * 14 : 40 + Math.random() * 80;
            points.set(n.id, { x: (near ? near.x : 0) + Math.cos(angle) * d, y: (near ? near.y : 0) + Math.sin(angle) * d, vx: 0, vy: 0, node: n, fixed: false });
        }
        const key = view.nodes.map((n) => n.id).sort().join("\n") + "\n\n" + view.links.map((l) => `${l.source}>${l.target}`).sort().join("\n");
        if (key === shapeKey) return;
        const first = shapeKey === "" || fresh === view.nodes.length;
        shapeKey = key;
        alpha = Math.max(alpha, first ? 1 : 0.3);
        if (first) {
            // Lay out out of sight, then frame it: no dots flying apart on first sight.
            const started = performance.now();
            while (alpha > 0.04 && performance.now() - started < 120) tick();
            if (!viewTouched) fit();
        }
    }

    // ---- layout ---------------------------------------------------------------------------

    const CHARGE = { agent: -700, space: -600, sub: -320, dir: -110, file: -70 };
    const REST = { spawn: 110, root: 80, tree: 40, touch: 80 };
    const PULL = { spawn: 0.25, root: 0.2, tree: 0.35, touch: 0.04 };

    function radius(n) {
        switch (n.kind) {
            case "agent":
                return 13;
            case "space":
                return 11;
            case "sub":
                return 9;
            case "dir":
                return 3;
            default:
                return Math.min(3.5 + Math.sqrt(n.touches || 0) * 1.5, 14);
        }
    }

    function tick() {
        const ps = [...points.values()];
        const n = ps.length;
        for (let i = 0; i < n; i++) {
            const a = ps[i];
            const ca = CHARGE[a.node.kind] || -60;
            for (let j = i + 1; j < n; j++) {
                const b = ps[j];
                let dx = b.x - a.x;
                let dy = b.y - a.y;
                let d2 = dx * dx + dy * dy;
                if (d2 > 250000) continue;
                if (d2 < 1) {
                    dx = Math.random() - 0.5;
                    dy = Math.random() - 0.5;
                    d2 = 1;
                }
                const cb = CHARGE[b.node.kind] || -60;
                const f = (Math.sqrt(-ca * -cb) * alpha) / Math.max(d2, 36);
                const d = Math.sqrt(d2);
                const fx = (dx / d) * f;
                const fy = (dy / d) * f;
                a.vx -= fx;
                a.vy -= fy;
                b.vx += fx;
                b.vy += fy;
                const min = radius(a.node) + radius(b.node) + 3;
                if (d < min) {
                    const push = ((min - d) / d) * 0.5;
                    a.vx -= dx * push * 0.5;
                    a.vy -= dy * push * 0.5;
                    b.vx += dx * push * 0.5;
                    b.vy += dy * push * 0.5;
                }
            }
        }
        for (const l of view.links) {
            const a = points.get(l.source);
            const b = points.get(l.target);
            if (!a || !b) continue;
            const dx = b.x - a.x;
            const dy = b.y - a.y;
            const d = Math.sqrt(dx * dx + dy * dy) || 1;
            const k = ((d - (REST[l.kind] || 50)) / d) * (PULL[l.kind] || 0.2) * alpha;
            a.vx += dx * k;
            a.vy += dy * k;
            b.vx -= dx * k;
            b.vy -= dy * k;
        }
        for (const p of ps) {
            const hub = p.node.kind === "agent" || p.node.kind === "space";
            const pull = (hub ? 0.04 : 0.008) * alpha;
            p.vx -= p.x * pull;
            p.vy -= p.y * pull;
            if (p.fixed) {
                p.vx = p.vy = 0;
                continue;
            }
            p.vx *= 0.55;
            p.vy *= 0.55;
            p.x += p.vx;
            p.y += p.vy;
        }
        alpha *= 0.982;
        if (alpha < 0.003) alpha = 0;
    }

    function fit() {
        if (points.size === 0) return;
        let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
        for (const p of points.values()) {
            minX = Math.min(minX, p.x);
            minY = Math.min(minY, p.y);
            maxX = Math.max(maxX, p.x);
            maxY = Math.max(maxY, p.y);
        }
        const pad = 40;
        const w = Math.max(maxX - minX, 1);
        const h = Math.max(maxY - minY, 1);
        const k = Math.min((size.w - pad * 2) / w, (size.h - pad * 2) / h, 2.2);
        transform = { k: Math.max(k, 0.25), x: size.w / 2 - ((minX + maxX) / 2) * Math.max(k, 0.25), y: size.h / 2 - ((minY + maxY) / 2) * Math.max(k, 0.25) };
    }

    // ---- timeline -------------------------------------------------------------------------

    function setCursor(index, fromUser) {
        cursor = Math.max(-1, Math.min(index, steps.length - 1));
        cursorSince = performance.now();
        if (fromUser) {
            live = false;
            playing = false;
            clearInterval(playTimer);
        }
        drawCard();
        kick();
    }

    function drawCard() {
        const c = selected ? caches.get(selected) : null;
        const s = steps[cursor];
        // The timeline is one agent's; the all-sessions view keeps only its summary.
        const one = mode === "agent";
        strip.hidden = !one;
        playButton.hidden = liveButton.hidden = !one;
        while (card.firstChild) card.removeChild(card.firstChild);
        if (mode !== "agent" || !c || !s) {
            card.hidden = mode !== "agent" || !c;
            card.textContent = c && !s ? "No steps yet." : "";
            playButton.disabled = liveButton.disabled = steps.length === 0;
            return;
        }
        card.hidden = false;
        const dot = document.createElement("span");
        dot.className = "dot";
        dot.style.background = s.outcome === "failed" ? roles.red : kindColor(s.kind);
        const what = document.createElement("span");
        what.className = "what";
        what.textContent = M.describe(s, c.files, c.subs);
        const when = document.createElement("span");
        when.className = "when";
        const d = new Date(s.at_ms || 0);
        when.textContent = `#${s.seq} · ${d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}`;
        card.append(dot, what, when);
        playButton.textContent = playing ? "❚❚" : "▶";
        playButton.title = playing ? "Pause" : "Replay from the start";
        liveButton.classList.toggle("on", live);
        playButton.disabled = liveButton.disabled = false;
    }

    function drawStrip() {
        const w = strip.clientWidth;
        const h = strip.clientHeight;
        const dpr = window.devicePixelRatio || 1;
        if (strip.width !== Math.round(w * dpr)) strip.width = Math.round(w * dpr);
        if (strip.height !== Math.round(h * dpr)) strip.height = Math.round(h * dpr);
        sctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        sctx.clearRect(0, 0, w, h);
        const n = steps.length;
        if (n === 0) return;
        const x = (i) => (n === 1 ? w / 2 : 4 + (i / (n - 1)) * (w - 8));
        sctx.fillStyle = alphaColor(roles.rule, 0.5);
        sctx.fillRect(0, h / 2 - 0.5, w, 1);
        for (let i = 0; i < n; i++) {
            const s = steps[i];
            const prompt = s.kind === "prompt";
            const hit = selectedFile !== null && s.file === selectedFile;
            const tall = prompt ? h - 4 : hit ? h - 6 : h * 0.5;
            sctx.globalAlpha = i <= cursor ? 1 : 0.35;
            sctx.fillStyle = s.outcome === "failed" ? roles.red : s.outcome === "pending" ? roles.dim : kindColor(s.kind);
            sctx.fillRect(Math.round(x(i)) - (prompt ? 1 : 0.5), (h - tall) / 2, prompt ? 2 : Math.max(1, Math.min(3, (w - 8) / n)), tall);
        }
        sctx.globalAlpha = 1;
        if (cursor >= 0) {
            sctx.fillStyle = roles.accent;
            sctx.fillRect(Math.round(x(cursor)) - 1, 0, 2, h);
        }
    }

    function scrubTo(clientX) {
        const r = strip.getBoundingClientRect();
        const n = steps.length;
        if (n === 0) return;
        const i = Math.round(((clientX - r.left - 4) / Math.max(r.width - 8, 1)) * (n - 1));
        setCursor(i, true);
    }

    // ---- drawing --------------------------------------------------------------------------

    function resize() {
        const r = stage.getBoundingClientRect();
        const dpr = window.devicePixelRatio || 1;
        const changed = size.w !== r.width || size.h !== r.height;
        size = { w: Math.max(r.width, 1), h: Math.max(r.height, 1), dpr };
        canvas.width = Math.round(size.w * dpr);
        canvas.height = Math.round(size.h * dpr);
        canvas.style.width = `${size.w}px`;
        canvas.style.height = `${size.h}px`;
        if (changed && !viewTouched) fit();
        kick();
    }

    const toScreen = (p) => ({ x: p.x * transform.k + transform.x, y: p.y * transform.k + transform.y });

    function currentFrame(now) {
        if (mode !== "agent" || steps.length === 0) return null;
        return M.frame(steps, cursor, Math.max(0, now - cursorSince), reduced);
    }

    function matches(n) {
        if (!query) return true;
        return (n.label || "").toLowerCase().includes(query) || (n.path || "").toLowerCase().includes(query);
    }

    function draw(now) {
        ctx.setTransform(size.dpr, 0, 0, size.dpr, 0, 0);
        ctx.clearRect(0, 0, size.w, size.h);
        const f = currentFrame(now);
        const replaying = f && cursor < steps.length - 1;
        const visible = (n) => !(replaying && n.kind === "file" && !f.present.has(n.file));
        const pendingFiles = new Set();
        const c = selected ? caches.get(selected) : null;
        if (c && mode === "agent") for (const seq of c.pending || []) {
            const s = c.steps.get(seq);
            if (s && s.file != null) pendingFiles.add(s.file);
        }
        ctx.save();
        ctx.translate(transform.x, transform.y);
        ctx.scale(transform.k, transform.k);
        // Links.
        for (const l of view.links) {
            const a = points.get(l.source);
            const b = points.get(l.target);
            if (!a || !b || !visible(a.node) || !visible(b.node)) continue;
            ctx.beginPath();
            ctx.moveTo(a.x, a.y);
            ctx.lineTo(b.x, b.y);
            ctx.lineWidth = (l.kind === "tree" ? 0.8 : 1.1) / Math.sqrt(transform.k);
            ctx.strokeStyle = alphaColor(roles.rule, l.kind === "touch" ? 0.35 : 0.7);
            ctx.setLineDash(l.kind === "touch" ? [3, 3] : []);
            ctx.stroke();
        }
        ctx.setLineDash([]);
        // Beams: the current step's, and live arrivals'.
        const beam = (from, to, kind, progress) => {
            if (!from || !to) return;
            const x = from.x + (to.x - from.x) * progress;
            const y = from.y + (to.y - from.y) * progress;
            const g = ctx.createLinearGradient(from.x, from.y, x, y);
            g.addColorStop(0, alphaColor(kindColor(kind), 0));
            g.addColorStop(1, alphaColor(kindColor(kind), 0.9));
            ctx.beginPath();
            ctx.moveTo(from.x, from.y);
            ctx.lineTo(x, y);
            ctx.strokeStyle = g;
            ctx.lineWidth = 2.2 / Math.sqrt(transform.k);
            ctx.stroke();
            ctx.beginPath();
            ctx.arc(x, y, 2.6, 0, Math.PI * 2);
            ctx.fillStyle = kindColor(kind);
            ctx.fill();
        };
        if (f) for (const b of f.beams) beam(points.get(`a:${b.from}`), points.get(`f:${b.file}`), b.kind, b.progress);
        const liveLit = new Map();
        if (!reduced && !replaying) {
            for (const [id, fl] of flares) {
                const since = Math.max(0, now - fl.t);
                if (since > M.FLARE) {
                    flares.delete(id);
                    continue;
                }
                liveLit.set(id, { kind: fl.kind, flare: 1 - since / M.FLARE });
                if (since < M.BEAM) beam(points.get(fl.from), points.get(id), fl.kind, since / M.BEAM);
            }
        }
        // Nodes.
        const labels = [];
        ctx.font = `${fontSize}px ${font}`;
        ctx.textAlign = "center";
        ctx.textBaseline = "top";
        for (const p of points.values()) {
            const n = p.node;
            if (!visible(n)) continue;
            const r = radius(n);
            const dimmed = !matches(n);
            const lit = (f && n.kind === "file" && f.lit.get(n.file)) || liveLit.get(n.id);
            let color = roles.dim;
            if (n.kind === "file") color = kindColor(n.last);
            else if (n.kind === "agent") color = roles.accent;
            else if (n.kind === "space") color = roles.mauve;
            else if (n.kind === "sub") color = subColor(n.sub || 1);
            if (lit) color = lit.outcome === "failed" ? roles.red : kindColor(lit.kind);
            ctx.globalAlpha = dimmed ? 0.2 : 1;
            if (lit && (lit.flare > 0 || lit.trail)) {
                const glow = lit.flare > 0 ? lit.flare : lit.trail * 0.4;
                ctx.beginPath();
                ctx.arc(p.x, p.y, r * (1.6 + 1.8 * glow), 0, Math.PI * 2);
                ctx.fillStyle = alphaColor(color, 0.28 * glow);
                ctx.fill();
            }
            ctx.beginPath();
            ctx.arc(p.x, p.y, r * (lit && lit.flare ? 1 + 0.35 * lit.flare : 1), 0, Math.PI * 2);
            const hub = n.kind === "agent" || n.kind === "space";
            // Hubs are rings around the tool's initial, so they never read as one more file.
            ctx.fillStyle = n.kind === "dir" ? alphaColor(roles.dim, 0.7) : hub ? alphaColor(roles.base, 0.92) : color;
            ctx.fill();
            if (hub) {
                ctx.lineWidth = 2.5;
                ctx.strokeStyle = color;
                ctx.stroke();
                ctx.save();
                ctx.fillStyle = color;
                ctx.font = `600 ${Math.round(r * 1.05)}px ${font}`;
                ctx.textBaseline = "middle";
                const glyph = n.kind === "space" ? "▣" : (n.tool || (mode === "agent" ? (caches.get(selected) || {}).tool : "") || "a").charAt(0).toUpperCase();
                ctx.fillText(glyph, p.x, p.y + 0.5);
                ctx.restore();
            }
            if (n.edited && n.kind === "file") {
                ctx.beginPath();
                ctx.arc(p.x, p.y, r + 2, 0, Math.PI * 2);
                ctx.lineWidth = 1.2;
                ctx.strokeStyle = alphaColor(roles.peach, 0.85);
                ctx.stroke();
            }
            if (n.shared) {
                ctx.beginPath();
                ctx.arc(p.x, p.y, r + 4.5, 0, Math.PI * 2);
                ctx.lineWidth = 1.4;
                ctx.strokeStyle = roles.yellow;
                ctx.stroke();
            }
            // In flight: a turning dashed ring (a pending call, a running subagent).
            const spinning = (n.kind === "file" && pendingFiles.has(n.file)) || (n.kind === "sub" && n.status === "running");
            if (spinning) {
                ctx.beginPath();
                ctx.arc(p.x, p.y, r + 3.5, 0, Math.PI * 2);
                ctx.setLineDash([3, 4]);
                ctx.lineDashOffset = reduced ? 0 : -(now / 60) % 7;
                ctx.lineWidth = 1.3;
                ctx.strokeStyle = alphaColor(color, 0.9);
                ctx.stroke();
                ctx.setLineDash([]);
            }
            if (n.kind === "sub" && n.status === "failed") {
                ctx.beginPath();
                ctx.arc(p.x, p.y, r + 3, 0, Math.PI * 2);
                ctx.lineWidth = 1.5;
                ctx.strokeStyle = roles.red;
                ctx.stroke();
            }
            // Labels are placed after every node, most important first, so they never pile up.
            let priority = 0;
            if (n.kind === "agent" || n.kind === "space") priority = 5;
            else if (hovered === n) priority = 6;
            else if (lit && lit.flare > 0) priority = 4;
            else if (n.kind === "sub") priority = 3;
            else if (lit || (query && !dimmed)) priority = 2;
            else if (n.kind === "file") priority = 1 + Math.min(n.touches || 0, 50) / 100;
            else if (n.kind === "dir" && transform.k > 1.4) priority = 0.5;
            if (priority > 0) labels.push({ p, n, r, priority, dimmed });
        }
        ctx.globalAlpha = 1;
        ctx.restore();
        // Screen space: skip a label that would overlap one already drawn.
        labels.sort((a, b) => b.priority - a.priority);
        const placed = [];
        ctx.font = `${fontSize}px ${font}`;
        ctx.textAlign = "center";
        ctx.textBaseline = "top";
        for (const l of labels) {
            const at = toScreen(l.p);
            const w = ctx.measureText(l.n.label).width;
            const box = { x: at.x - w / 2 - 2, y: at.y + l.r * transform.k + 2, w: w + 4, h: fontSize + 2 };
            if (placed.some((b) => box.x < b.x + b.w && b.x < box.x + box.w && box.y < b.y + b.h && b.y < box.y + box.h)) continue;
            placed.push(box);
            ctx.globalAlpha = l.dimmed ? 0.25 : 1;
            ctx.fillStyle = l.n.kind === "dir" ? roles.dim : l.n.kind === "file" ? roles.subtext : roles.text;
            ctx.fillText(l.n.label, at.x, box.y + 1);
        }
        ctx.globalAlpha = 1;
        drawStrip();
        return liveLit.size > 0 || (f && (now - cursorSince < M.FLARE)) || pendingFiles.size > 0 || view.nodes.some((n) => n.kind === "sub" && n.status === "running");
    }

    // ---- the loop -------------------------------------------------------------------------

    let scheduled = false;
    let dragging = null;

    function kick() {
        if (scheduled) return;
        scheduled = true;
        requestAnimationFrame(loop);
    }

    /** Where a replay is at `now`. A function of the clock, not of frames drawn: a hidden or
     * headless view (WebKitGTK under Broadway draws none) still plays, and `state()` agrees. */
    function advance(now) {
        if (!playing) return;
        const t = playFrom + (now - playStart);
        const i = Math.max(M.stepAt(at, t), cursor);
        if (i !== cursor) {
            cursor = i;
            cursorSince = playStart + (at[i] - playFrom);
            drawCard();
        }
        if (i >= steps.length - 1) {
            playing = false;
            live = true;
            clearInterval(playTimer);
            drawCard();
        }
    }

    let playTimer = 0;

    function loop() {
        // One clock for everything: a rAF timestamp is the frame's START and can be earlier
        // than a performance.now() taken since, which would make an elapsed time negative.
        const now = performance.now();
        scheduled = false;
        if (alpha > 0) tick();
        advance(now);
        const animating = draw(now);
        if (alpha > 0 || playing || animating || dragging) kick();
    }

    // ---- interaction ----------------------------------------------------------------------

    function nodeAt(clientX, clientY) {
        const r = canvas.getBoundingClientRect();
        const x = (clientX - r.left - transform.x) / transform.k;
        const y = (clientY - r.top - transform.y) / transform.k;
        let best = null;
        let bestD = Infinity;
        for (const p of points.values()) {
            const d = Math.hypot(p.x - x, p.y - y);
            const reach = radius(p.node) + 5 / transform.k;
            if (d < reach && d < bestD) {
                best = p;
                bestD = d;
            }
        }
        return best;
    }

    function showTip(p, clientX, clientY) {
        while (tip.firstChild) tip.removeChild(tip.firstChild);
        if (!p) {
            tip.hidden = true;
            return;
        }
        const n = p.node;
        const line = (text, cls) => {
            const d = document.createElement("div");
            if (cls) d.className = cls;
            d.textContent = text;
            tip.appendChild(d);
        };
        if (n.kind === "file") {
            line(n.path || n.abs, "strong");
            const c = selected ? caches.get(selected) : null;
            const f = c && n.file != null ? c.files.get(n.file) : null;
            if (f) line(`read ${f.reads} · edited ${f.edits} · wrote ${f.writes} · searched ${f.searches}`);
            else line(`${n.touches} touches${n.shared ? ` · ${n.owners.size} agents` : ""}`);
            if (host.local && n.abs) line("double-click to open", "dim");
        } else if (n.kind === "sub") {
            line(`${n.label} — ${n.status}`, "strong");
            if (n.detail) line(n.detail);
        } else if (n.kind === "agent") {
            line(n.label, "strong");
            if (n.status) line(n.status);
            if (mode === "all") line("click to jump to it", "dim");
        } else {
            line(n.label, "strong");
            if (n.path) line(n.path, "dim");
        }
        tip.hidden = false;
        const r = stage.getBoundingClientRect();
        const x = Math.min(clientX - r.left + 14, r.width - tip.offsetWidth - 6);
        const y = Math.min(clientY - r.top + 14, r.height - tip.offsetHeight - 6);
        tip.style.left = `${Math.max(x, 4)}px`;
        tip.style.top = `${Math.max(y, 4)}px`;
    }

    canvas.addEventListener("pointerdown", (e) => {
        const p = nodeAt(e.clientX, e.clientY);
        canvas.setPointerCapture(e.pointerId);
        dragging = { p, x: e.clientX, y: e.clientY, tx: transform.x, ty: transform.y, moved: false };
        if (p) p.fixed = true;
        kick();
    });
    canvas.addEventListener("pointermove", (e) => {
        if (dragging) {
            const dx = e.clientX - dragging.x;
            const dy = e.clientY - dragging.y;
            if (Math.abs(dx) + Math.abs(dy) > 3) dragging.moved = true;
            if (dragging.p) {
                const r = canvas.getBoundingClientRect();
                dragging.p.x = (e.clientX - r.left - transform.x) / transform.k;
                dragging.p.y = (e.clientY - r.top - transform.y) / transform.k;
                alpha = Math.max(alpha, 0.12);
            } else if (dragging.moved) {
                transform.x = dragging.tx + dx;
                transform.y = dragging.ty + dy;
                viewTouched = true;
            }
            kick();
            return;
        }
        const p = nodeAt(e.clientX, e.clientY);
        if ((p && p.node) !== hovered) {
            hovered = p ? p.node : null;
            canvas.style.cursor = p ? "pointer" : "grab";
            kick();
        }
        showTip(p, e.clientX, e.clientY);
    });
    const endDrag = (e) => {
        if (!dragging) return;
        const d = dragging;
        dragging = null;
        if (d.p) d.p.fixed = false;
        if (!d.moved) click(d.p);
        kick();
        void e;
    };
    canvas.addEventListener("pointerup", endDrag);
    canvas.addEventListener("pointercancel", endDrag);
    canvas.addEventListener("pointerleave", () => {
        hovered = null;
        tip.hidden = true;
        kick();
    });
    canvas.addEventListener("dblclick", (e) => {
        const p = nodeAt(e.clientX, e.clientY);
        if (p && p.node.kind === "file" && host.local && p.node.abs) post({ type: "open", path: p.node.abs });
    });
    canvas.addEventListener("wheel", (e) => {
        e.preventDefault();
        const r = canvas.getBoundingClientRect();
        const mx = e.clientX - r.left;
        const my = e.clientY - r.top;
        const k = Math.min(Math.max(transform.k * Math.exp(-e.deltaY * 0.0015), 0.15), 6);
        transform.x = mx - ((mx - transform.x) * k) / transform.k;
        transform.y = my - ((my - transform.y) * k) / transform.k;
        transform.k = k;
        viewTouched = true;
        kick();
    }, { passive: false });

    function click(p) {
        if (!p) {
            selectedFile = null;
            kick();
            return;
        }
        const n = p.node;
        if (n.kind === "agent" && n.token) post({ type: "jump", token: n.token });
        else if (n.kind === "agent" && mode === "agent" && selected) post({ type: "jump", token: selected });
        else if (n.kind === "file" && mode === "agent") {
            selectedFile = selectedFile === n.file ? null : n.file;
            // Jump the cursor to this file's latest step.
            if (selectedFile !== null) {
                for (let i = steps.length - 1; i >= 0; i--) {
                    if (steps[i].file === selectedFile) {
                        setCursor(i, true);
                        break;
                    }
                }
            }
        }
        kick();
    }

    strip.addEventListener("pointerdown", (e) => {
        strip.setPointerCapture(e.pointerId);
        scrubTo(e.clientX);
        const move = (m) => scrubTo(m.clientX);
        const up = () => {
            strip.removeEventListener("pointermove", move);
            strip.removeEventListener("pointerup", up);
        };
        strip.addEventListener("pointermove", move);
        strip.addEventListener("pointerup", up);
    });
    strip.addEventListener("mousemove", (e) => {
        const r = strip.getBoundingClientRect();
        const n = steps.length;
        const c = selected ? caches.get(selected) : null;
        if (!n || !c) return;
        const i = Math.round(((e.clientX - r.left - 4) / Math.max(r.width - 8, 1)) * (n - 1));
        const s = steps[Math.max(0, Math.min(i, n - 1))];
        strip.title = M.describe(s, c.files, c.subs);
    });

    playButton.addEventListener("click", () => {
        if (playing) {
            playing = false;
            clearInterval(playTimer);
            drawCard();
            return;
        }
        if (steps.length === 0) return;
        const from = cursor >= steps.length - 1 || cursor < 0 ? 0 : cursor;
        playing = true;
        live = false;
        playFrom = at[from] || 0;
        playStart = performance.now();
        cursor = from;
        cursorSince = playStart;
        clearInterval(playTimer);
        playTimer = setInterval(() => advance(performance.now()), 100);
        drawCard();
        kick();
    });
    liveButton.addEventListener("click", () => {
        live = true;
        playing = false;
        setCursor(steps.length - 1, false);
    });

    window.addEventListener("keydown", (e) => {
        if (e.target === search) return;
        if (e.key === " ") {
            e.preventDefault();
            playButton.click();
        } else if (e.key === "ArrowLeft") setCursor(cursor - 1, true);
        else if (e.key === "ArrowRight") setCursor(cursor + 1, true);
        else if (e.key === "End") liveButton.click();
        else if (e.key === "f") {
            viewTouched = false;
            fit();
            kick();
        }
    });

    search.addEventListener("input", () => {
        query = search.value.trim().toLowerCase();
        kick();
    });
    picker.addEventListener("change", () => setAgent(picker.value, true));
    modeAgent.addEventListener("click", () => setMode("agent", true));
    modeAll.addEventListener("click", () => setMode("all", true));

    function setMode(m, fromUser) {
        if (m !== "agent" && m !== "all") return;
        if (m === mode) return;
        mode = m;
        modeAgent.classList.toggle("on", mode === "agent");
        modeAll.classList.toggle("on", mode === "all");
        flares.clear();
        points.clear();
        shapeKey = "";
        viewTouched = false;
        live = true;
        playing = false;
        selectedFile = null;
        fillPicker();
        rebuild();
        drawCard();
        askHistory();
        if (fromUser) post({ type: "mode", mode });
    }

    function setAgent(token, fromUser) {
        if (!token || token === selected) return;
        selected = token;
        points.clear();
        shapeKey = "";
        viewTouched = false;
        live = true;
        playing = false;
        selectedFile = null;
        flares.clear();
        inflight = null;
        clearTimeout(retryTimer);
        fillPicker();
        rebuild();
        askHistory();
        if (fromUser) post({ type: "agent", token });
    }

    function setTheme(t) {
        if (!t) return;
        for (const k of Object.keys(roles)) if (typeof t[k] === "string" && t[k]) roles[k] = t[k];
        if (t.font) font = `"${String(t.font).replace(/["\\]/g, "")}", ui-monospace, monospace`;
        if (t.fontSize) fontSize = Math.max(8, Math.min(+t.fontSize || 11, 20));
        const root = document.documentElement.style;
        for (const k of Object.keys(roles)) root.setProperty(`--${k}`, roles[k]);
        root.setProperty("--font", font);
        root.setProperty("--font-size", `${fontSize}px`);
        drawCard();
        kick();
    }

    function state() {
        advance(performance.now());
        const f = currentFrame(performance.now());
        return {
            ready: true,
            mode,
            agent: selected,
            agents: (topic.agents || []).length,
            nodes: view.nodes.length,
            files: view.nodes.filter((n) => n.kind === "file").length,
            subagents: view.nodes.filter((n) => n.kind === "sub").length,
            shared: view.nodes.filter((n) => n.shared).length,
            links: view.links.length,
            hidden: view.hidden,
            steps: steps.length,
            cursor,
            seq: steps[cursor] ? steps[cursor].seq : null,
            card: card.textContent,
            playing,
            live,
            lit: f ? [...f.lit.keys()] : [],
            summary: summary.textContent,
            settled: alpha === 0,
            resets,
            local: !!host.local,
            epoch: host.epoch ?? null,
            historyUnsupported,
        };
    }

    window.comuxGraph = {
        setData,
        addHistory,
        historyFailed,
        reset,
        setTheme,
        setHost: (h) => {
            host = { ...host, ...(h || {}) };
        },
        setMode: (m) => setMode(m, false),
        setAgent: (t) => setAgent(t, false),
        setReduceMotion: (r) => {
            reduced = !!r;
            kick();
        },
        state,
        // Draw now, without waiting for a frame (a host with no display schedules none): what a
        // capture of the canvas needs.
        render: () => {
            while (alpha > 0) tick();
            draw(performance.now());
        },
    };

    // A parent frame (the web client) drives it the same way.
    window.addEventListener("message", (e) => {
        const m = e.data && e.data.comuxGraphCall;
        if (!m || typeof window.comuxGraph[m.fn] !== "function" || m.fn === "state" || m.fn === "render") return;
        window.comuxGraph[m.fn](m.arg);
    });

    new ResizeObserver(resize).observe(stage);
    resize();
    drawCard();
    post({ type: "ready" });
})();
