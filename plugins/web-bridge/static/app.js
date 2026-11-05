  (() => {
    const root = document.getElementById("root");
    let token = sessionStorage.getItem("copad.token") || "";
// Bearer is the only auth mode. A Tailscale identity header is not trusted: serve attaches it
// to every request it forwards, WebSocket upgrades are not subject to CORS, and
// `/ws/board/attach` hands back a terminal, so any page on an admitted device could drive it
// with no token (docs/mobile-access.md).

    function api(path, opts = {}) {
      opts.headers = Object.assign({}, opts.headers || {});
      if (token) {
        opts.headers["Authorization"] = "Bearer " + token;
      }
      if (opts.body && !opts.headers["Content-Type"]) opts.headers["Content-Type"] = "application/json";
      return fetch(path, opts).then(async (r) => {
        if (r.status === 401) {
          {
            token = ""; sessionStorage.removeItem("copad.token");
            stopBoardPolling();   // no longer authenticated; do not keep hammering 401s
            render();
          }
          throw new Error("unauthorized");
        }
        const body = r.headers.get("content-type")?.includes("json") ? await r.json() : await r.text();
        if (!r.ok) {
          const err = body && body.error ? body.error : { code: String(r.status), message: typeof body === "string" ? body : "" };
          throw err;
        }
        return body;
      });
    }

    const state = {
      mode: "overview",         // "overview" | "attach" | "mux"
      muxError: "",             // why the comux terminal could not open (from the preflight)
      board: null,              // GET /api/board result; null = never loaded
      boardStale: false,        // a fetch failed — what is shown is the LAST good read
      boardTimer: null,
      boardEpoch: 0,
      boardLoop: 0,
      boardInFlight: false,
      boardNote: "",            // why a row could not be opened (stale token, no server)
      jumpBusy: null,           // a jump is in flight; a second row tap would race it
      muxNote: "",              // transient input-layer note, shown under the IME strip
      muxDown: false,           // the attach socket closed; the banner offers a reconnect
      muxChecking: false,       // a preflight is in flight (drives the banner)
      muxEpoch: 0,              // bumped on every entry/teardown; stale preflights are dropped
      muxResizeOff: null,       // detaches the mux resize listeners on leave
      presence: null,
      tmuxPanes: [],
      events: [],
      noGui: false,
      pilot: null,              // /api/pilot/status result: {pilot:{goals,active,gate,counts}, csd, tmx, errors}
      pilotError: "",           // set when pilot.status RPC fails (plugin not running)
      pilotAdd: false,
      activePane: null,         // pane_id while in attach mode
      ws: { events: null, overview: null, attach: null },
      term: null,
      fit: null,
      ctrlSticky: false,
      // push.status: "unsupported" (no serviceWorker/PushManager), "denied", "off" (supported, not
      // subscribed), "on" (server holds a Subscription). kinds: empty = every kind.
      push: {
        status: "unsupported",
        kinds: ["notification", "stop", "codex-turn"],
        subId: null,
        error: "",
      },
    };

    // PushManager.subscribe wants the VAPID public key as a Uint8Array, not url-safe base64.
    function urlB64ToUint8Array(b64) {
      const pad = "=".repeat((4 - b64.length % 4) % 4);
      const norm = (b64 + pad).replace(/-/g, "+").replace(/_/g, "/");
      const raw = atob(norm);
      const out = new Uint8Array(raw.length);
      for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
      return out;
    }

    // Subprotocol list for WebSocket constructors; the upgrade auth reads `bearer.<token>`.
    function wsProtocols() {
      return token ? [`bearer.${token}`] : undefined;
    }
    function canReconnect() {
      return !!token;
    }

    function escapeHtml(s) {
      return String(s == null ? "" : s).replace(/[&<>"']/g, c => ({ "&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;" }[c]));
    }
    function stripAnsi(s) { return String(s).replace(/\x1b\[[0-9;]*[a-zA-Z]/g, ""); }
    function fmtTs(ms) { if (!ms) return ""; return new Date(ms).toTimeString().slice(0,8); }
    // Relative age like "5s"/"3m"/"2h"/"1d" from epoch millis, "" when missing.
    function humanAgeMs(ms) {
      if (!ms) return "";
      const secs = Math.max(0, Math.floor((Date.now() - ms) / 1000));
      return humanAgeSecs(secs);
    }
    function humanAgeFromEpochSec(tsSec) {
      if (!tsSec) return "";
      const secs = Math.max(0, Math.floor(Date.now() / 1000 - tsSec));
      return humanAgeSecs(secs);
    }
    function humanAgeSecs(secs) {
      if (secs < 60) return `${secs}s`;
      if (secs < 3600) return `${Math.floor(secs / 60)}m`;
      if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
      return `${Math.floor(secs / 86400)}d`;
    }
    function summarize(e) {
      const p = e.data || e.payload || {};
      if (typeof p === "string") return p;
      const keys = Object.keys(p);
      if (keys.length === 0) return "";
      return keys.slice(0,2).map(k => `${k}=${JSON.stringify(p[k]).slice(0,50)}`).join(" ");
    }

    function renderSetup() {
      root.innerHTML = `
        <header><h1>copad <span class="dim">— remote bridge</span></h1></header>
        <div class="setup">
          <p>Paste the bearer token from <code>$COPAD_WEB_BRIDGE_TOKEN</code>. Stored in sessionStorage; cleared on tab close.</p>
          <input id="tok" type="password" placeholder="bearer token (≥32 chars)" autocomplete="off">
          <button id="connect">connect</button>
        </div>
      `;
      const inp = document.getElementById("tok");
      const btn = document.getElementById("connect");
      inp.focus();
      const go = () => {
        const t = inp.value.trim();
        if (t.length < 32) { inp.style.borderColor = "var(--accent-err)"; return; }
        token = t; sessionStorage.setItem("copad.token", t);
        bootFromHash();
        bootstrap();
      };
      btn.addEventListener("click", go);
      inp.addEventListener("keydown", e => { if (e.key === "Enter") go(); });
    }

    // Stable sort key, lower = higher on the page: blocked/awaiting-decision, working, ready,
    // then plain shell (tmux's own order). The harness `blocked` flag overrides everything.
    function paneSortRank(p) {
      const a = p.agent;
      if (a && a.flags && a.flags.blocked) return 0;
      const status = a && a.status;
      if (status === "awaiting-decision") return 0;
      if (status === "working") return 1;
      if (status === "ready") return 2;
      if (status === "idle" && a && (a.kind === "claude" || a.kind === "codex")) return 2;
      return 4;
    }
    function statusToPillClass(s) {
      if (s === "working") return "busy";
      if (s === "awaiting-decision") return "waiting";
      if (s === "ready" || s === "idle") return "idle";
      return "";
    }
    function paneSorted(panes) {
      return panes.slice().sort((a, b) => paneSortRank(a) - paneSortRank(b));
    }

    // Status comes from comux and is cached and inferred: only Claude's is read from a session
    // file, the rest is matched against screen text, and `idle` doubles as "no recognized UI".
    // So the partition must be exhaustive and keep the raw label, or `ready` agents get hidden.
    const BOARD_ATTENTION = new Set(["blocked"]);
    const BOARD_ACTIVE = new Set(["working"]);

    // Group key. An older comux sends no `space_id`, and two sessions may share a display name,
    // so a name-based key is namespaced and marked ambiguous rather than merged with an id key.
    function boardGroupKey(a) {
      return a.space_id ? `id:${a.space_id}` : `name:${a.space || "?"}`;
    }

    // Name the tool only where it differs from the fleet's dominant one, so `codex` on a row means
    // something and a phone does not spend a column repeating `claude`.
    function dominantTool(agents) {
      const c = new Map();
      for (const a of agents) c.set(a.tool, (c.get(a.tool) || 0) + 1);
      let best = null, n = 0;
      for (const [t, k] of c) if (k > n) { best = t; n = k; }
      return c.size <= 1 ? best : null;
    }

    function boardRow(a, withSpace, hideTool) {
      const cls = BOARD_ATTENTION.has(a.status) ? "s-attn"
                : BOARD_ACTIVE.has(a.status) ? "s-work" : "s-idle";
      const where = withSpace
        ? `<span class="row-space">${escapeHtml(a.space || "?")}</span>`
        : "";
      // `detail` is what the agent was last seen doing. Absent means "no reading", not "idle", and
      // an older comux does not send it, so it is left out rather than shown as a placeholder.
      const detail = a.detail
        ? `<div class="row-detail">${escapeHtml(a.detail)}</div>` : "";
      const tool = hideTool ? "" : `<span class="row-tool">${escapeHtml(a.tool || "")}</span>`;
      // `blocked` and `working` are encoded by band, `ready` by a plain dot. Anything else prints
      // its raw label: comux's `idle` also means "no recognized UI", and must not read as ready.
      const encoded = BOARD_ATTENTION.has(a.status) || BOARD_ACTIVE.has(a.status)
        || a.status === "ready";
      const status = encoded ? "" : `<span class="row-status">${escapeHtml(a.status || "?")}</span>`;
      return `
        <div class="agent-row" data-token="${escapeHtml(a.token || "")}">
          <span class="dot ${cls}"></span>
          <div class="row-main">
            <div class="row-title">${where}<span class="row-name">${escapeHtml(a.title || "?")}</span></div>
            ${detail}
          </div>
          ${status}${tool}<i class="row-age">${escapeHtml(humanSecs(a.for_secs))}</i>
        </div>`;
    }

    function humanSecs(n) {
      if (typeof n !== "number" || n < 0) return "";
      if (n < 90) return `${n}s`;
      if (n < 5400) return `${Math.round(n / 60)}m`;
      if (n < 172800) return `${Math.round(n / 3600)}h`;
      return `${Math.round(n / 86400)}d`;
    }

    function renderBoard() {
      const b = state.board;
      if (!b) {
        return `<div class="board-empty">${
          state.boardStale ? "브리지에 연결할 수 없음 — 재시도 중" : "불러오는 중\u2026"}</div>`;
      }

      const errs = (b.errors || []).map(e =>
        `<div class="banner">fleet: ${escapeHtml(e.what)} — ${escapeHtml(e.message || e.code)}</div>`).join("")
        + (state.boardNote ? `<div class="banner">${escapeHtml(state.boardNote)}</div>` : "");
      // Computed before the early return, or a transport failure on a partial response would show
      // the retained session count as current.
      const stale = state.boardStale
        ? `<div class="banner">마지막으로 읽은 내용 — 브리지가 응답하지 않음</div>` : "";

      // null means "could not read"; [] means "read, and empty". Rendering both as an empty fleet
      // would print a false "idle 0".
      if (b.agents === null) {
        return `${errs}${stale}<div class="board-empty">에이전트 목록을 읽지 못했습니다${
          b.sessions ? ` — 마지막 읽기 기준 세션 ${b.sessions.length}개` : ""}</div>`;
      }
      const agents = b.agents;
      const only = dominantTool(agents);   // null when the fleet is mixed
      const attention = agents.filter(a => BOARD_ATTENTION.has(a.status));
      const active = agents.filter(a => BOARD_ACTIVE.has(a.status));
      const rest = agents.filter(a => !BOARD_ATTENTION.has(a.status) && !BOARD_ACTIVE.has(a.status));

      // Groups are always expanded; there is no toggle.
      const groups = new Map();
      for (const a of rest) {
        const k = boardGroupKey(a);
        if (!groups.has(k)) groups.set(k, { name: a.space || "?", ambiguous: !a.space_id, rows: [] });
        groups.get(k).rows.push(a);
      }

      const band = (title, rows, cls) => rows.length === 0 ? "" : `
        <section class="band ${cls}">
          <h2 class="band-head">${title}<span class="count">${rows.length}</span></h2>
          ${rows.map(a => boardRow(a, true, !!only)).join("")}
        </section>`;

      // Ambiguity is a property of the server (an older comux sends no session id), so mark a group
      // "by name" only when some groups are keyed by id and some are not.
      const someById = [...groups.values()].some(g => !g.ambiguous);
      const allByName = groups.size > 0 && [...groups.values()].every(g => g.ambiguous);
      const idle = rest.length === 0 ? "" : `
        <section class="band idle">
          <h2 class="band-head">대기<span class="count">${rest.length}</span></h2>
          ${[...groups.values()].map(g => `
            <div class="group">
              <h3 class="group-head">${escapeHtml(g.name)}${
                g.ambiguous && someById ? '<em title="이 세션은 id가 없어 이름으로 묶었습니다">이름 기준</em>' : ""
              }<span class="count">${g.rows.length}</span></h3>
              ${g.rows.map(a => boardRow(a, false, !!only)).join("")}
            </div>`).join("")}
        </section>`;

      const sessions = b.sessions ? b.sessions.length : null;
      const summary = `
        <div class="board-summary">
          ${attention.length ? `<span class="sum s-attn"><b>${attention.length}</b>대기 중</span>` : ""}
          <span class="sum s-work"><b>${active.length}</b>작업 중</span>
          <span class="sum s-idle"><b>${rest.length}</b>유휴</span>
          ${sessions === null ? "" : `<span class="sum"><b>${sessions}</b>세션</span>`}
          ${only ? `<span class="sum sum-quiet">${escapeHtml(only)}</span>` : ""}
        </div>`;

      return `${errs}${stale}${summary}
        ${band("응답 필요", attention, "attention")}
        ${band("작업 중", active, "working")}
        ${idle}
        <p class="board-note">상태는 comux의 추론값이고, 시간은 그 상태를 유지한 기간입니다.${
          allByName ? " 이 comux는 세션 id를 보내지 않아 이름으로 묶었습니다 — <code>comux server restart</code> 후 정확해집니다." : ""
        }</p>`;
    }

    // One request at a time, the next scheduled after the previous completes, stale responses
    // dropped by epoch. /api/board can spend two sequential subprocess deadlines server-side.
    async function loadBoard() {
      // Single-flight: startBoardPolling runs on return from the terminal and on every foreground,
      // so without this a waking phone could stack several fetches.
      if (state.boardInFlight) return;
      state.boardInFlight = true;
      const epoch = ++state.boardEpoch;
      let deadline = null;
      try {
        // `api()` has no timeout. A request that never settles (lost signal) would leave
        // `boardInFlight` set forever and skip every later refresh. It is aborted rather than
        // raced, since an abandoned request would overlap the next poll.
        const ctl = new AbortController();
        deadline = setTimeout(() => ctl.abort(), 15000);
        const b = await api("/api/board", { signal: ctl.signal });
        if (state.boardEpoch !== epoch) return;
        state.board = b;
        state.boardStale = false;
      } catch {
        if (state.boardEpoch !== epoch) return;
        state.boardStale = true;
      } finally {
        if (deadline) clearTimeout(deadline);
        state.boardInFlight = false;
      }
      if (state.mode === "overview") render();
    }

    // Same condition `render()` uses to pick the app or the token page. On the setup screen a
    // foreground event would start polling, every 401 clears the token and re-renders, and the
    // field being pasted into is wiped every five seconds.
    function isAuthed() {
      return !!token;
    }

    function startBoardPolling() {
      if (!isAuthed()) { stopBoardPolling(); return; }
      // Only the timer is reset, not the request epoch: invalidating an in-flight request on every
      // foreground would discard a good response about to land. Invalidation belongs to
      // `stopBoardPolling`.
      clearBoardTimer();
      // Only the current generation may schedule. Otherwise a restart during an in-flight request
      // forks the loop into two polling chains.
      const loop = ++state.boardLoop;
      const tick = async () => {
        if (!isAuthed() || state.mode !== "overview" || state.boardLoop !== loop) return;
        await loadBoard();
        if (!isAuthed() || state.mode !== "overview" || state.boardLoop !== loop) return;
        state.boardTimer = setTimeout(tick, 5000);
      };
      tick();
    }

    function clearBoardTimer() {
      if (state.boardTimer) { clearTimeout(state.boardTimer); state.boardTimer = null; }
    }

    function stopBoardPolling() {
      clearBoardTimer();
      state.boardLoop++;    // no existing tick may schedule again
      state.boardEpoch++;   // leaving: drop any in-flight response
    }

    function renderOverview() {
      const pushChip = (() => {
        if (state.push.status === "unsupported") return `<button class="chip" id="push-toggle" disabled>push: n/a</button>`;
        if (state.push.status === "denied") return `<button class="chip warn" id="push-toggle">push: denied</button>`;
        if (state.push.status === "on") return `<button class="chip ok" id="push-toggle">push: on</button>`;
        return `<button class="chip" id="push-toggle">push: off</button>`;
      })();
      root.innerHTML = `
        <header>
          <h1>copad <span class="dim">— remote bridge</span></h1>
          ${pushChip}
          <button class="chip" id="open-mux">terminal</button>
          <button class="chip ${state.presence === "away" ? "warn" : "ok"}" id="presence-toggle">${state.presence || "…"}</button>
        </header>
        ${state.noGui ? `<div class="banner" style="margin: 0.5rem 0.6rem;">copad GUI not running — the fleet board, terminal, presence and events still work</div>` : ""}
        <main class="overview">
          ${state.push.status === "on" ? `
            <div class="push-settings">
              <span>push kinds:</span>
              ${["notification", "stop", "codex-turn"].map(k => `
                <label><input type="checkbox" data-push-kind="${escapeHtml(k)}" ${state.push.kinds.includes(k) ? "checked" : ""}> ${escapeHtml(k)}</label>
              `).join("")}
              <span style="color:var(--fg-dim);">(empty = all kinds)</span>
            </div>
          ` : ""}
          ${state.push.error ? `<div class="push-settings"><span class="err">push: ${escapeHtml(state.push.error)}</span></div>` : ""}
          ${renderPilot()}
          ${renderBoard()}
          <div class="section-title">recent events</div>
          <div class="events-strip">${
            state.events.length === 0
              ? `<div class="empty" style="border:none; padding:0.7rem;">waiting…</div>`
              : state.events.slice().reverse().slice(0, 20).map(e => `
                  <div class="event-row" data-kind="${escapeHtml(e.type || e.kind || "?")}">
                    <span class="event-kind">${escapeHtml(e.type || e.kind || "?")}</span>
                    <span>${escapeHtml(summarize(e))}</span>
                    <span style="margin-left:auto; color:var(--fg-dim); font-size:0.7rem;">${fmtTs(e.timestamp_ms)}</span>
                  </div>`).join("")
          }</div>
        </main>
      `;
      document.getElementById("open-mux").addEventListener("click", () => enterMux(true));
      document.getElementById("presence-toggle").addEventListener("click", togglePresence);
      const pushBtn = document.getElementById("push-toggle");
      if (pushBtn && !pushBtn.disabled) pushBtn.addEventListener("click", togglePush);
      document.querySelectorAll(".push-settings input[data-push-kind]").forEach(el => {
        el.addEventListener("change", () => updatePushKinds(el.dataset.pushKind, el.checked));
      });
      document.querySelectorAll(".agent-row").forEach(el => {
        el.addEventListener("click", () => openAgent(el, el.dataset.token || ""));
      });
      document.querySelectorAll(".card[data-pane]").forEach(el => {
        el.addEventListener("click", () => enterAttach(el.dataset.pane));
      });
      wirePilot();
    }

    function renderAttach() {
      const pane = state.tmuxPanes.find(p => p.pane_id === state.activePane);
      const title = pane ? `${pane.session} : ${pane.window_name || pane.window_index} (${pane.pane_id})` : state.activePane;
      root.innerHTML = `
        <header>
          <button class="chip back" id="back">← overview</button>
          <div class="card-meta" style="flex:1; text-align:center; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;">${escapeHtml(title)}</div>
          <button class="chip ${state.presence === "away" ? "warn" : "ok"}" id="presence-toggle">${state.presence || "…"}</button>
        </header>
        <main class="attach">
          <div id="term-host"></div>
          <div class="kbd-bar">
            <button data-bytes="\\x1b">Esc</button>
            <button data-bytes="\\t">Tab</button>
            <button id="ctrl">Ctrl</button>
            <button data-bytes="/">/</button>
            <button data-bytes="|">|</button>
            <button data-bytes="\\x1b[A">↑</button>
            <button data-bytes="\\x1b[B">↓</button>
            <button data-bytes="\\x1b[D">←</button>
            <button data-bytes="\\x1b[C">→</button>
            <button data-bytes="\\x03">^C</button>
            <button data-bytes="\\x04">^D</button>
            <button data-bytes="\\x1a">^Z</button>
          </div>
        </main>
      `;
      document.getElementById("back").addEventListener("click", leaveAttach);
      document.getElementById("presence-toggle").addEventListener("click", togglePresence);
      wireKbdBar();
      openTerminal();
    }

// Key bar buttons act on `touchend`, not `click`: on iOS, preventDefault on `touchstart`
// suppresses the synthesized mousedown/mouseup/click, so a click handler never runs. So
// `touchstart` stays passive and preventDefault happens on `touchend`, which keeps the input
// focused (no dropped keyboard) and cancels the synthesized click. Only a touch that stayed put
// counts, since each bar is a horizontally scrollable row. A device that emits a click anyway is
// covered by the suppression window.

    const TAP_SLOP_PX = 10;
    const TAP_CLICK_SUPPRESS_MS = 700;

    // Tap detection as a small machine so it is testable without a browser. A gesture is owned by
    // the identifier it started with and dies when a second finger joins (checked on every move,
    // since that finger may land elsewhere) or when it travels past the slop during the move, so a
    // finger that wanders down the bar and back is a scroll, not a key.
    function tapGesture(slop) {
      let id = null, sx = 0, sy = 0, moved = false;
      return {
        start(t, count) {
          if (count !== 1) { id = null; return; }
          id = t.identifier; sx = t.clientX; sy = t.clientY; moved = false;
        },
        move(t, count) {
          if (id === null) return;
          if (count !== 1) { id = null; return; }
          if (t.identifier !== id) return;
          if (Math.abs(t.clientX - sx) > slop || Math.abs(t.clientY - sy) > slop) moved = true;
        },
        end(t, remaining) {
          const fire = id !== null && !moved && remaining === 0 && !!t && t.identifier === id;
          id = null;
          return fire;
        },
        cancel() { id = null; },
      };
    }

    // Both a touch fire and a synthesized click would send the byte twice, e.g. a double Escape.
    function shouldFireClick(now, lastTouch) {
      return now - lastTouch >= TAP_CLICK_SUPPRESS_MS;
    }

    function onActivate(el, run) {
      if (!el) return;
      // Preventing a synthesized mousedown keeps focus and, unlike touchstart, does not cancel the
      // click.
      el.addEventListener("mousedown", (e) => e.preventDefault());
      // Per element, not shared: tapping one key then clicking another on a hybrid device (iPad +
      // trackpad) within the window must not swallow the second.
      let lastTouchFire = 0;
      const tap = tapGesture(TAP_SLOP_PX);
      el.addEventListener("touchstart", (e) => tap.start(e.touches[0], e.touches.length), { passive: true });
      el.addEventListener("touchmove", (e) => tap.move(e.touches[0], e.touches.length), { passive: true });
      el.addEventListener("touchend", (e) => {
        if (!tap.end(e.changedTouches[0], e.touches.length)) return;
        e.preventDefault();
        lastTouchFire = Date.now();
        run();
      }, { passive: false });
      el.addEventListener("touchcancel", () => tap.cancel());
      el.addEventListener("click", () => {
        if (!shouldFireClick(Date.now(), lastTouchFire)) return;
        run();
      });
    }

    function wireKbdBar() {
      // The two views own input differently: mux has the IME strip, attach lets xterm read its
      // keyboard.
      const refocus = () => {
        try {
          if (state.mode === "mux") imeEl()?.focus({ preventScroll: true });
          else state.term?.focus();
        } catch {}
      };
      document.querySelectorAll(".kbd-bar button[data-bytes]").forEach(btn => {
        onActivate(btn, () => { sendKbdBytes(btn.dataset.bytes); refocus(); });
      });
      onActivate(document.getElementById("ctrl"), () => {
        state.ctrlSticky = !state.ctrlSticky;
        document.getElementById("ctrl")?.classList.toggle("sticky", state.ctrlSticky);
        refocus();
      });
    }

    function shortId(id) { return (id || "").replace(/^g-/, "").slice(0, 8); }

    function renderPilot() {
      const wrap = state.pilot && state.pilot.pilot ? state.pilot.pilot : null;
      const goals = wrap && Array.isArray(wrap.goals) ? wrap.goals : [];
      const counts = (wrap && wrap.counts) || {};
      const countStr = Object.keys(counts).length
        ? Object.entries(counts).map(([k, v]) => `${escapeHtml(k)} ${v}`).join(" · ")
        : "";
      const projects = (state.pilot && Array.isArray(state.pilot.projects)) ? state.pilot.projects : [];
      const projectOpts = `<option value="">— pick a project —</option>` + projects.map(p => {
        const path = p.subpath ? `${p.path}/${p.subpath}` : p.path;
        return `<option value="${escapeHtml(path)}">${escapeHtml(p.name)} · ${escapeHtml(p.path)}</option>`;
      }).join("");
      const addForm = `
        <div class="pilot-add">
          ${state.pilotAdd ? `
            ${projects.length ? `<select id="pilot-cwd-select">${projectOpts}</select>` : ""}
            <input id="pilot-cwd" placeholder="cwd (existing dir)" value="${escapeHtml(state.lastCwd || "")}">
            <textarea id="pilot-instruction" placeholder="goal instruction"></textarea>
            <div class="pilot-add-row">
              <select id="pilot-posture" title="how much the agent may do unattended before a gate pauses the queue">
                <option value="trust">trust (default — folder trust only)</option>
                <option value="auto-accept">auto-accept (auto-accept edits)</option>
                <option value="bypass">bypass (skip permission prompts)</option>
                <option value="yolo">yolo (no guardrails)</option>
                <option value="default">default (no flags — gates on everything)</option>
              </select>
              <button class="chip" id="pilot-add-submit">enqueue</button>
              <button class="chip" id="pilot-add-cancel">cancel</button>
            </div>
          ` : `<button class="chip" id="pilot-add-open">+ add goal</button>`}
        </div>`;
      if (state.pilotError) {
        return `<div class="section-title">pilot</div>
          <div class="banner" style="margin:0 0 0.4rem;">pilot: ${escapeHtml(state.pilotError)}</div>${addForm}`;
      }
      const rows = goals.length === 0
        ? `<div class="empty" style="border:none; padding:0.6rem;">no goals queued</div>`
        : goals.map(g => {
          const live = g.live && g.live.status ? ` · ${escapeHtml(g.live.status)}` : "";
          const isActive = wrap.active === g.id;
          const terminal = ["done", "stalled", "failed", "cancelled"].includes(g.status);
          let gateBlock = "";
          if (g.status === "awaiting_gate" && g.gate) {
            const gk = g.gate.kind;
            const prompt = g.gate.prompt || (gk === "plan" ? "Plan ready for approval" : "Gate open");
            const controls = gk === "answer"
              ? `<input class="pilot-answer-input" data-id="${escapeHtml(g.id)}" placeholder="your answer">
                 <button class="chip pilot-answer" data-id="${escapeHtml(g.id)}">send</button>`
              : `<button class="chip ok pilot-approve" data-id="${escapeHtml(g.id)}">approve</button>`;
            gateBlock = `<div class="pilot-gate">
              <span class="pill warn">${escapeHtml(gk)}</span>
              <div class="pilot-gate-prompt">${escapeHtml(prompt)}</div>
              <div class="pilot-gate-controls">${controls}</div>
            </div>`;
          }
          const statusClass = g.status === "running" ? "busy"
            : g.status === "done" ? "ok"
            : (g.status === "failed" || g.status === "stalled") ? "err" : "";
          return `<div class="pilot-goal${isActive ? " active" : ""}">
            <div class="pilot-goal-head">
              <span class="pill ${statusClass}">${escapeHtml(g.status)}${live}</span>
              <span class="pilot-goal-id">${escapeHtml(shortId(g.id))}</span>
              <span class="pilot-goal-instr">${escapeHtml(g.instruction || "")}</span>
              ${terminal ? "" : `<button class="chip pilot-cancel" data-id="${escapeHtml(g.id)}">cancel</button>`}
            </div>
            ${gateBlock}
          </div>`;
        }).join("");
      const sidecarErr = state.pilot && state.pilot.errors && Object.keys(state.pilot.errors).length
        ? `<div class="pilot-sidecar-err">${Object.entries(state.pilot.errors).map(([k, v]) => `${escapeHtml(k)}: ${escapeHtml(String(v))}`).join(" / ")}</div>`
        : "";
      return `
        <div class="section-title">pilot ${countStr ? `<span style="text-transform:none; color:var(--fg-dim);">${countStr}</span>` : ""}</div>
        <div class="pilot-strip">${rows}</div>
        ${sidecarErr}
        ${addForm}`;
    }

    async function loadPilot() {
      try {
        state.pilot = await api("/api/pilot/status");
        state.pilotError = "";
      } catch (e) {
        state.pilot = null;
        state.pilotError = (e && e.message) || "unavailable";
      }
      if (state.mode === "overview") render();
    }

    function wirePilot() {
      const open = document.getElementById("pilot-add-open");
      if (open) open.addEventListener("click", () => { state.pilotAdd = true; render(); });
      const cancelAdd = document.getElementById("pilot-add-cancel");
      if (cancelAdd) cancelAdd.addEventListener("click", () => { state.pilotAdd = false; render(); });
      const submit = document.getElementById("pilot-add-submit");
      const cwdSelect = document.getElementById("pilot-cwd-select");
      if (cwdSelect) cwdSelect.addEventListener("change", () => {
        if (cwdSelect.value) document.getElementById("pilot-cwd").value = cwdSelect.value;
      });
      if (submit) submit.addEventListener("click", async () => {
        const cwd = document.getElementById("pilot-cwd").value.trim() || (cwdSelect ? cwdSelect.value : "");
        const instruction = document.getElementById("pilot-instruction").value.trim();
        const posture = document.getElementById("pilot-posture").value;
        if (!cwd || !instruction) return;
        state.lastCwd = cwd;
        try {
          await api("/api/pilot/goals", { method: "POST", body: JSON.stringify({ cwd, instruction, posture }) });
          state.pilotAdd = false;
          await loadPilot();
        } catch (e) { alert("add failed: " + (e.message || e)); }
      });
      document.querySelectorAll(".pilot-answer").forEach(el => el.addEventListener("click", async () => {
        const id = el.dataset.id;
        const input = document.querySelector(`.pilot-answer-input[data-id="${CSS.escape(id)}"]`);
        const text = input ? input.value.trim() : "";
        if (!text) return;
        try { await api(`/api/pilot/goals/${encodeURIComponent(id)}/answer`, { method: "POST", body: JSON.stringify({ text }) }); await loadPilot(); }
        catch (e) { alert("answer failed: " + (e.message || e)); }
      }));
      document.querySelectorAll(".pilot-approve").forEach(el => el.addEventListener("click", async () => {
        const id = el.dataset.id;
        try { await api(`/api/pilot/goals/${encodeURIComponent(id)}/approve`, { method: "POST", body: JSON.stringify({}) }); await loadPilot(); }
        catch (e) { alert("approve failed: " + (e.message || e)); }
      }));
      document.querySelectorAll(".pilot-cancel").forEach(el => el.addEventListener("click", async () => {
        const id = el.dataset.id;
        try { await api(`/api/pilot/goals/${encodeURIComponent(id)}/cancel`, { method: "POST" }); await loadPilot(); }
        catch (e) { alert("cancel failed: " + (e.message || e)); }
      }));
    }

    function render() {
      if (!token) { renderSetup(); return; }
      if (state.mode === "mux") { renderMux(); return; }
      if (state.mode === "attach") { renderAttach(); return; }
      renderOverview();
    }

    /* ==== comux terminal (the mobile terminal) ==== */

    // The grid is whatever the phone fits, with no column floor. Flooring at 80 (to stop the phone
    // reflowing the desktop) left horizontal scrolling on a ~40 column screen, and 80 is comux's
    // `sidebar_min_cols` default, which keeps its sidebar visible and unreadable. Fitting hides
    // the sidebar by itself (comux checks `cols >= sidebar_min_cols`) without touching `Ctrl-b s`,
    // which is shared state. The fleet board is the phone's sidebar.
    //
    // Cost: the composed frame and the PTYs follow the narrowest client, so if the phone detaches
    // last the geometry stays narrow until something attaches (decisions #65/#66).
    //
    // The size must be the browser's logical grid, not only the PTY: comux probes the terminal's
    // size on focus and sends back what it gets, so a PTY-only clamp is overwritten.
    const MUX_MIN_COLS = 20;   // not a layout choice — just never send a degenerate size
    const MUX_MIN_ROWS = 8;

    function muxGrid(fit, term) {
      if (fit) fit.fit();
      const cols = Math.max(term.cols, MUX_MIN_COLS);
      const rows = Math.max(term.rows, MUX_MIN_ROWS);
      if (cols !== term.cols || rows !== term.rows) term.resize(cols, rows);
      return { cols, rows };
    }

    // Every size change goes through here so xterm and the PTY cannot drift apart.
    function sendMuxResize() {
      if (!state.term) return;
      const { cols, rows } = muxGrid(state.fit, state.term);
      const ws = state.ws.attach;
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "resize", rows, cols }));
      }
    }

    // Replace the view's chrome while keeping a live terminal's DOM. Any async handler finishing
    // while mux is open calls `render()`; a plain `root.innerHTML = ...` would drop xterm's host
    // element, and `openMuxTerminal` would then return early on its idempotence guard, leaving a
    // blank screen whose toolbar types into an invisible terminal.
    function renderPreservingTerminal(html) {
      // The compose bar is kept too: a native IME composition lives in the element, and copying its
      // string into a fresh element would drop the half-built syllable.
      const keep = ["term-host", "ime-wrap"]
        .map((id) => document.getElementById(id))
        .filter(Boolean);
      for (const el of keep) el.remove();
      root.innerHTML = html;
      for (const el of keep) document.getElementById(el.id)?.replaceWith(el);
    }

    // The mux view is built once and patched thereafter. `el.remove()` disconnects a focused
    // textarea, which resets focus and ends the native IME composition, and re-inserting the node
    // cannot resurrect a half-built syllable. Patching also stops listeners accumulating per
    // render.
    function renderMux() {
      if (document.getElementById("mux-shell")) {
        patchMuxChrome();
        return;
      }
      renderPreservingTerminal(`
        <div id="mux-shell">
        <header>
          <button class="chip back" id="back">&larr; overview</button>
          <div class="card-meta" style="flex:1; text-align:center;">comux</div>
          <button class="chip" id="presence-toggle"></button>
        </header>
        <main class="attach">
          <div id="mux-banner"></div>
          <div id="term-host" class="mux"></div>
          <div id="ime-wrap" class="ime-wrap">
            <textarea id="ime" rows="1" enterkeyhint="enter" autocapitalize="off" autocomplete="off"
                      autocorrect="off" spellcheck="false" aria-label="입력"
                      placeholder="한글 조합 영역 · 그 외 키는 바로 전송"></textarea>
          </div>
          <div class="kbd-bar">
            <button data-bytes="\\x02">Ctrl-b</button>
            <button data-bytes="\\x03">Ctrl-C</button>
            <button data-bytes="\\x1b">Esc</button>
            <button data-bytes="\\t">Tab</button>
            <button id="ctrl">Ctrl</button>
            <button data-bytes="\\x1b[A">&uarr;</button>
            <button data-bytes="\\x1b[B">&darr;</button>
            <button data-bytes="\\x1b[D">&larr;</button>
            <button data-bytes="\\x1b[C">&rarr;</button>
            <button data-bytes="\\x0d">Enter</button>
          </div>
          <!-- comux chords. The prefix and the key travel as ONE payload, so a soft keyboard is
               never asked to hold a modifier across two taps — which is what made these
               unreachable before. Kept to the verbs worth a chip on a phone. -->
          <div class="kbd-bar chords">
            <button data-bytes="\\x02c">탭+</button>
            <button data-bytes="\\x02p">&lsaquo;탭</button>
            <button data-bytes="\\x02n">탭&rsaquo;</button>
            <button data-bytes="\\x02s">사이드바</button>
            <button data-bytes="\\x02!">&#9873;막힌곳</button>
            <button data-bytes="\\x06">찾기</button>
            <button data-bytes="\\x02[">스크롤</button>
            <button data-bytes="\\x02d">분리</button>
          </div>
        </main>
        </div>`);
      document.getElementById("back").addEventListener("click", leaveMux);
      document.getElementById("presence-toggle").addEventListener("click", togglePresence);
      wireKbdBar();
      wireIme();
      patchMuxChrome();
    }

    // Update the mux chrome without touching the DOM the terminal and the IME live in.
    function patchMuxChrome() {
      const pres = document.getElementById("presence-toggle");
      if (pres) {
        pres.textContent = state.presence || "\u2026";
        pres.className = "chip " + (state.presence === "away" ? "warn" : "ok");
      }
      const banner = document.getElementById("mux-banner");
      if (banner) {
        const msg = state.muxChecking ? "checking comux\u2026"
                  : state.muxError || (state.muxDown ? "연결이 끊겼습니다" : "");
        const again = state.muxDown && !state.muxChecking
          ? ` <button class="chip ok" id="mux-reconnect">재연결</button>` : "";
        banner.innerHTML = msg ? `<div class="banner">${escapeHtml(msg)}${again}</div>` : "";
        document.getElementById("mux-reconnect")?.addEventListener("click", () => enterMux(false));
      }
      renderImeNote();
      if (!state.muxChecking && !state.muxError) openMuxTerminal();
    }

    // Drop the terminal, socket and listeners without changing the mode. Re-entering mux needs this
    // too: `render()` replaces the host element and `openMuxTerminal` would no-op on its
    // idempotence guard, leaving a blank screen whose toolbar types into the old socket.
    function disposeMuxTerminal() {
      // Invalidate the input layer first: a settle timer or queued chord closes over
      // `state.ws.attach`, which the next entry replaces, so a late drain would type into another
      // socket.
      resetIme();
      state.muxDown = false;
      state.muxNote = "";
      try { state.muxResizeOff?.(); } catch {}
      state.muxResizeOff = null;
      if (state.ws.attach) { try { state.ws.attach.close(); } catch {} state.ws.attach = null; }
      if (state.term) { try { state.term.dispose(); } catch {} state.term = null; }
      state.fit = null;
    }

    // Tear down mux without touching history (mirror of `teardownAttach`), so popstate owns the
    // transition. Bumping the epoch lets Back-then-Forward during a preflight open a fresh one.
    function teardownMux() {
      disposeMuxTerminal();
      state.muxEpoch++;
      state.muxChecking = false;
      state.mode = "overview";
      state.muxError = "";
      startBoardPolling();      // refresh immediately on return, then resume the cadence
    }

// ==== direct input ====
// A key reaches the PTY the moment it is pressed, so `Ctrl-b` then `c` is a comux chord. The
// exception is IME composition: Korean typed into xterm's own helper textarea arrives
// decomposed on iOS (that textarea is `opacity: 0; z-index: -5`; the cause is suspected, not
// proved). A normal, visible, sized textarea composes correctly (proved on device).
//
// So the one focusable element is a one-row strip under the terminal, almost always empty.
// Printable text, Enter, Backspace, arrows, Ctrl chords and pastes are written straight to the
// PTY; only the syllable the IME is still building lives in the strip, and is written when the
// IME commits.
//
// Ordering: nothing may reach the PTY ahead of text the IME has already shown, and nothing that
// can retarget the input (a chord, a tap on another pane) may run before that text lands.
// `withOrder` owns this with three states and one queue:
//   idle       nothing pending: flush the strip, then act.
//   composing  the IME owns the strip: queue the action and force a commit (blur + refocus,
//              the only synchronous commit the DOM offers).
//   settling   the IME finished but the DOM has not settled; reading the strip now is the early
//              read xterm itself defers. The action queues and drains after the read.

    const IME_COMMIT_MS = 200;
    const IME_MAX_H = 72;           // the strip grows to ~3 rows, then scrolls

    let imeState = "idle";          // "idle" | "composing" | "settling"
    let imeComp = 0;                // bumped per composition; the watchdog checks it
    let imeQueue = [];              // {run, text} deferred behind a composition, in arrival order
    let imeGen = 0;                 // bumped on dispose; a stale timer checks it and returns
    let imeTimer = null;
    let imeInAction = false;
    let imeWriteFailed = false;     // a write failed inside the action currently running

    function imeEl() { return document.getElementById("ime"); }

    // The only ws.send for input. Returns false rather than dropping the keystroke silently: on a
    // phone this socket dies whenever the screen sleeps.
    function write(s) {
      if (!s) return true;
      return writeBytes(new TextEncoder().encode(s));
    }

    function writeBytes(bytes) {
      const ws = state.ws.attach;
      const fail = () => { imeWriteFailed = true; showDisconnected(); return false; };
      if (!ws || ws.readyState !== WebSocket.OPEN) return fail();
      try { ws.send(bytes); } catch { return fail(); }
      return true;
    }

    // Staged text -> PTY. Cleared only after a successful write, synchronously, so a racing flush
    // cannot send the same syllable twice.
    function flush() {
      const el = imeEl();
      if (!el || !el.value) return true;
      if (!write(el.value)) return false;
      el.value = "";
      autoGrowIme(el);
      return true;
    }

    // `text` is the printable payload the action would have written, so a dropped action does not
    // lose it: the control part is discarded and the characters go back into the strip.
    function withOrder(action, text) {
      if (imeInAction) { action(); return; }      // already inside an ordered drain
      if (imeState !== "idle") {
        imeQueue.push({ run: action, text });
        if (imeState === "composing") forceCommit();
        return;
      }
      // A failed send cancels what came after it but never the characters: older queued text goes
      // back first, then this action's own, in typed order.
      if (!flush()) { dropQueue(); if (text) stageText(text); return; }
      runAction(action);
    }

    // Run one action and report whether its writes got out. An action returns `false` when it
    // could not send; that is the only way a failure inside `term.paste()` reaches the caller.
    function runAction(action) {
      imeInAction = true;
      imeWriteFailed = false;
      try {
        const ok = action();
        return ok !== false && !imeWriteFailed;
      } finally {
        imeInAction = false;
      }
    }

    const emit = (bytes) => withOrder(() => write(bytes));

    // Discard the queue but keep the characters: a chord or mouse report replayed later would act
    // on a screen that has moved on, while text goes back to the strip.
    function dropQueue() {
      const q = imeQueue;
      imeQueue = [];
      const kept = q.map((a) => a.text || "").join("");
      if (kept) stageText(kept);
    }

    // Ask the IME to commit now. `blur()` is the only synchronous commit the DOM offers (Chrome and
    // WebKit raise `compositionend` inside the call); the refocus keeps the phone keyboard up.
    // If an engine defers it anyway, the watchdog discards the queued action instead of running it
    // late, since a chord fired after an unknown delay acts on a different screen. Staged text is
    // kept.
    function forceCommit() {
      const el = imeEl();
      if (!el) return;
      el.blur();
      el.focus({ preventScroll: true });
      const gen = imeGen;
      const comp = imeComp;
      // Armed once per pending queue, never restarted: re-arming per action turns a 200 ms promise
      // into "200 ms after you stop tapping", and the first action is the one that goes stale.
      if (imeTimer) return;
      imeTimer = setTimeout(() => {
        imeTimer = null;
        if (gen !== imeGen || !imeQueue.length) return;
        dropQueue();
        showImeNote("조합이 끝나지 않아 키를 보내지 못했습니다");
        // Only take the state back if nothing newer took over. A composition started after this
        // deadline owns the strip but does not inherit this queue. Clearing the timer on
        // `compositionstart` once let a missed chord fire minutes later.
        if (comp === imeComp) imeState = "idle";
      }, IME_COMMIT_MS);
    }

    // One task after `compositionend`, when the DOM has settled. Idempotent.
    // Known limit: if a newer composition both starts and commits before this runs, the queue
    // drains after it (`한` + queued `x` + commit `글` reaches the PTY as `한글x`). That needs two
    // keystrokes inside one task, and a human types each key in its own.
    function settle(gen) {
      if (gen !== imeGen) return;                // torn down: this strip and socket are gone
      if (imeState === "composing") return;      // a newer composition owns the strip; it settles
      clearTimeout(imeTimer);
      imeTimer = null;
      imeState = "idle";
      // A queued action is replayed once, and dropped if the socket died meanwhile: unsent text is
      // worth keeping, a chord or mouse report replayed after a reconnect is not.
      if (!flush()) { dropQueue(); return; }
      const q = imeQueue;
      imeQueue = [];
      // Stop at the first failure so later input is not reordered around it; its characters go back
      // to the strip in order.
      for (let i = 0; i < q.length; i++) {
        if (runAction(q[i].run)) continue;
        const rest = q.slice(i + 1).map((a) => a.text || "").join("");
        if (rest) stageText(rest);
        return;
      }
    }

    // Tear the input layer down (dispose, and the reconnect button). Queued characters go back in
    // the strip, since reconnecting is the user trying to recover their typing.
    function resetIme() {
      imeGen++;
      clearTimeout(imeTimer);
      imeTimer = null;
      dropQueue();
      imeState = "idle";
      imeInAction = false;
    }

    // A keydown -> the bytes a terminal would send, or null when it is not ours (a cmd shortcut
    // belongs to the OS; a dead key to the IME). A plain printable character is translated here and
    // preventing that keydown suppresses the `beforeinput` that would follow, so the paths cannot
    // both fire. Some keyboards report keydown with no input events (a Space arrived with no text).
    function keyBytes(e) {
      if (e.metaKey) return null;
      switch (e.key) {
        case "Tab": return "\t";
        case "Escape": return "\x1b";
        case "Backspace": return "\x7f";
        case "Delete": return "\x1b[3~";
        case "ArrowUp": return "\x1b[A";
        case "ArrowDown": return "\x1b[B";
        case "ArrowRight": return "\x1b[C";
        case "ArrowLeft": return "\x1b[D";
        case "Home": return "\x1b[H";
        case "End": return "\x1b[F";
        case "PageUp": return "\x1b[5~";
        case "PageDown": return "\x1b[6~";
      }
      if (typeof e.key !== "string" || e.key.length !== 1) return null;   // F-keys, modifiers, Dead
      if (e.ctrlKey) {
        const c = e.key.toUpperCase().charCodeAt(0);
        if (c === 63) return "\x7f";                                      // Ctrl-? = DEL
        if (c === 32 || (c >= 64 && c <= 95)) return String.fromCharCode(c & 0x1f);
        return null;
      }
      if (e.altKey) return "\x1b" + e.key;                                // Meta = ESC prefix
      return e.key;                                                       // printable
    }

    // Anything the IME might still be composing stays in the strip until a boundary: the fallback
    // for a WebKit that inserts syllables without composition events. Every non-ASCII insertion,
    // not just Hangul, since the same holds for any IME.
    const NON_ASCII = /[^\x20-\x7e]/;

    function clearSticky() {
      state.ctrlSticky = false;
      document.getElementById("ctrl")?.classList.remove("sticky");
    }

    // The keyboard's exit. The sticky-Ctrl fold lives here and not in `write`, so a mouse report
    // or bracketed-paste byte is never folded into a control character. A printable character that
    // cannot be sent is kept in the strip; a control byte is not, since a stray Enter replayed
    // after a reconnect would act on a screen that has moved on.
    function sendKey(text) {
      if (state.ctrlSticky && text.length === 1 && text >= " " && text <= "~") {
        text = String.fromCharCode(text.charCodeAt(0) & 0x1f);
        clearSticky();
      }
      // Length is not the test: dictation and autocorrect deliver whole words.
      if (!isPrintable(text)) { emit(text); return; }
      // Goes through `withOrder` even with the socket gone: a character typed while a syllable is
      // pending belongs after it, and staging directly would put it in front.
      withOrder(() => {
        if (write(text)) return true;
        stageText(text);
        return false;
      }, text);
    }

    const isPrintable = (t) => t.length > 0 && !/[\x00-\x1f\x7f]/.test(t);

    function socketOpen() {
      const ws = state.ws.attach;
      return !!ws && ws.readyState === WebSocket.OPEN;
    }

    function stageText(text) {
      const el = imeEl();
      if (!el) return;
      el.value += text;
      autoGrowIme(el);
    }

    function onImeKeyDown(e) {
      if (state.muxNote) showImeNote("");
      // Enter first, and always prevented: letting the default run while the IME owns the key would
      // leave a newline in the strip, and preventing it guarantees no `insertLineBreak` follows.
      if (e.key === "Enter") {
        e.preventDefault();
        emit("\r");
        return;
      }
      if (e.isComposing || e.keyCode === 229) return;      // the IME owns this key
      const el = imeEl();
      if ((e.key === "Backspace" || e.key === "Delete") && el && el.value) return;
      const bytes = keyBytes(e);
      if (bytes == null) return;                           // not ours: beforeinput may still own it
      e.preventDefault();
      sendKey(bytes);
    }

    function onImeBeforeInput(e) {
      if (e.isComposing || e.inputType === "insertCompositionText") return;   // the IME owns it
      if (state.muxNote) showImeNote("");
      const el = imeEl();
      switch (e.inputType) {
        case "insertText":
        case "insertReplacementText": {
          // Only reachable when keydown did not already send this text (it prevents its own
          // default):
          // dictation, autocorrect, an emoji pick, or a soft keyboard with no usable keydown.
          const data = e.data || "";
          if (!data) return;
          if (NON_ASCII.test(data)) return;                // let it stage; a boundary writes it
          e.preventDefault();
          sendKey(data);
          return;
        }
        case "insertFromPaste": {
          // A textarea hands pasted text over on `e.data` with `dataTransfer` null; a
          // contenteditable does the opposite. If neither carries it, do not cancel: the text lands
          // in the strip and
          // goes out
          // at the next boundary, losing the bracketing but not the paste.
          const text = e.data || e.dataTransfer?.getData("text") || "";
          if (!text) return;
          e.preventDefault();
          // term.paste, not write: it adds the bracketed-paste delimiters when the app asked for
          // them, so a multi-line paste is editable instead of running line by line. It reports
          // nothing back, so the socket is checked before handing over, and `text` rides along for
          // retention.
          withOrder(() => {
            if (socketOpen()) {
              try { state.term?.paste(text); } catch { imeWriteFailed = true; }
              if (!imeWriteFailed) return true;
            } else {
              showDisconnected();
            }
            stageText(text);
            return false;
          }, text);
          return;
        }
        case "insertLineBreak":
        case "insertParagraph":
          // Only reachable when no keydown was delivered for that Return (keydown prevents every
          // Enter).
          e.preventDefault();
          emit("\r");
          return;
        case "deleteContentBackward":
          if (el && el.value) return;                      // editing the staged syllable
          e.preventDefault();
          emit("\x7f");
          return;
        case "deleteWordBackward":
          if (el && el.value) return;
          e.preventDefault();
          emit("\x17");
          return;
        default:
          return;
      }
    }

    // xterm draws a live cursor only from focus/blur on its own textarea, which never gets focus
    // again, so mirror ours. Coupled to xterm 5.5.0's handler (index.html pins the version); the
    // other effect is the `ESC [I` focus report an app must ask for.
    function mirrorXtermFocus(type) {
      const ta = state.term?.textarea;
      if (!ta) return;
      try { ta.dispatchEvent(new FocusEvent(type)); } catch {}
    }

    // Grow the strip to its content. The regrid is driven by the observer in `wireIme`, not here,
    // because a note appearing also moves the terminal's floor (comux's status bar once slid under
    // the fold).
    function autoGrowIme(el) {
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, IME_MAX_H)}px`;
      if (!window.ResizeObserver) sendMuxResize();
    }

    function showImeNote(msg) {
      state.muxNote = msg;
      renderImeNote();
    }

    function renderImeNote() {
      const wrap = document.getElementById("ime-wrap");
      if (!wrap) return;
      let note = wrap.querySelector(".ime-note");
      if (!state.muxNote) { note?.remove(); return; }
      if (!note) {
        note = document.createElement("div");
        note.className = "ime-note";
        wrap.appendChild(note);
      }
      note.textContent = state.muxNote;
    }

    // The socket is gone. Shown in the banner with a way out, since the `[disconnected]` line xterm
    // prints scrolls away and leaves a terminal that looks alive and types into nothing.
    function showDisconnected() {
      if (state.muxDown) return;
      state.muxDown = true;
      patchMuxChrome();
    }

// ==== touch scrolling ====
// comux paints the whole screen with cursor addressing, so xterm's scrollback stays empty and
// scrolling is comux's job, driven only by wheel events. A one-finger drag produces none on a
// touch device, so a vertical drag is translated into wheel events dispatched at the touch
// point and xterm encodes them with whatever protocol the app negotiated. Encoding SGR by hand
// would duplicate that negotiation and bypass the classifier that keeps a mouse report ordered
// behind a pending composition.

    const TOUCH_AXIS_SLOP_PX = 8;   // movement before the gesture commits to an axis
    const TOUCH_LINE_FALLBACK = 17; // px per line when the grid cannot be measured yet

    function rowHeightPx() {
      const rows = document.querySelector("#term-host .xterm-rows");
      const h = rows?.firstElementChild?.getBoundingClientRect().height;
      return h && h > 1 ? h : TOUCH_LINE_FALLBACK;
    }

    function wheelSteps(dy, rowH) {
      const steps = Math.trunc(dy / rowH);
      return { steps, rest: dy - steps * rowH };
    }

    // A drag -> lines machine. Same ownership rules as `tapGesture`, plus an axis that latches once
    // past the slop so a curved drag does not switch between scrolling and panning. `move` returns
    // the whole-line steps, or `null` when the move is not ours (wrong or second finger, under
    // slop, horizontal).
    function scrollGesture(slop) {
      let id = null, sx = 0, sy = 0, lastY = 0, rest = 0, axis = "";
      return {
        start(t, count) {
          if (count !== 1) { id = null; return; }
          id = t.identifier; sx = t.clientX; sy = t.clientY; lastY = t.clientY;
          rest = 0; axis = "";
        },
        move(t, count, rowH) {
          if (id === null || !t || t.identifier !== id) return null;
          if (count !== 1) { id = null; return null; }
          if (!axis) {
            const dx = Math.abs(t.clientX - sx), dy = Math.abs(t.clientY - sy);
            if (Math.max(dx, dy) < slop) return null;
            axis = dy > dx ? "y" : "x";
              // `lastY` is deliberately not reset: the movement that latched the axis is real, and
              // discarding it made a gesture delivered as one big `touchmove` scroll nothing.
          }
          if (axis !== "y") return null;
          const { steps, rest: left } = wheelSteps(t.clientY - lastY + rest, rowH);
          rest = left; lastY = t.clientY;
          return { steps };
        },
        end() { id = null; axis = ""; rest = 0; },
      };
    }

    function wireTouchScroll(host) {
      if (!host) return;
      const drag = scrollGesture(TOUCH_AXIS_SLOP_PX);
      host.addEventListener("touchstart", (e) => drag.start(e.touches[0], e.touches.length), { passive: true });
      host.addEventListener("touchmove", (e) => {
        const r = drag.move(e.touches[0], e.touches.length, rowHeightPx());
        if (!r) return;
        e.preventDefault();                                  // ours: stop iOS rubber-banding
        if (!r.steps) return;
        const t = e.touches[0];
        const target = document.elementFromPoint(t.clientX, t.clientY) || host;
        for (let i = 0; i < Math.abs(r.steps); i++) {
          target.dispatchEvent(new WheelEvent("wheel", {
            deltaY: r.steps > 0 ? -1 : 1,
            deltaMode: 1,                                    // DOM_DELTA_LINE: exactly one row
            clientX: t.clientX, clientY: t.clientY,
            bubbles: true, cancelable: true,
          }));
        }
      }, { passive: false });
      host.addEventListener("touchend", () => drag.end());
      host.addEventListener("touchcancel", () => drag.end());
    }

    function wireIme() {
      const el = imeEl();
      if (!el) return;
      el.addEventListener("keydown", onImeKeyDown);
      el.addEventListener("beforeinput", onImeBeforeInput);
      el.addEventListener("compositionstart", () => {
        imeComp++;
        imeState = "composing";
      });
      el.addEventListener("compositionend", () => {
        imeState = "settling";
        const gen = imeGen;
        setTimeout(() => settle(gen), 0);
      });
      el.addEventListener("input", () => autoGrowIme(el));
      // The strip shares the column with the terminal, so a height change takes rows from the grid,
      // and the window and visual-viewport resize listeners do not fire for the box next door.
      if (window.ResizeObserver) {
        const wrap = document.getElementById("ime-wrap");
        if (wrap) new ResizeObserver(() => sendMuxResize()).observe(wrap);
      }
      el.addEventListener("focus", () => mirrorXtermFocus("focus"));
      el.addEventListener("blur", () => mirrorXtermFocus("blur"));
      // A focus landing anywhere inside the terminal comes back to the strip. Capture phase so it
      // runs first, but without stopPropagation/preventDefault: stopping the event once made the
      // terminal untappable (no focus, no selection, no mouse report), killing comux's clickable
      // chrome.
      const host = document.getElementById("term-host");
      host?.addEventListener("focusin", (ev) => {
        if (ev.target === el) return;
        el.focus({ preventScroll: true });
      }, true);
      wireTouchScroll(host);
      renderImeNote();
    }

    function leaveMux() {
      if (history.state && history.state.mode === "mux") {
        history.back();
      } else {
        teardownMux();
        render();
      }
    }

    // A WebSocket upgrade cannot tell the page why it failed (a refused attach looks like a dropped
    // connection), so ask over HTTP first and show the real reason.
    async function muxPreflight() {
      try {
        const j = await api("/api/board/attach-preflight");
        return j.ok ? "" : (j.message || j.code || "comux is unavailable");
      } catch (e) {
        return `could not reach the bridge: ${e}`;
      }
    }

    // The one way into mux, for the button and history navigation alike. popstate entry must
    // preflight too, or Back-then-Forward with comux stopped shows a bare "[disconnected]".
    async function enterMux(pushHistory) {
      // Epoch identity, not a boolean: a second entry (double tap, Back-then-Forward mid-request)
      // supersedes the earlier one, and only the current epoch's response may land. A boolean could
      // be left set by a mid-flight teardown and swallow the next entry.
      disposeMuxTerminal();
      const epoch = ++state.muxEpoch;
      state.muxError = "";
      state.muxDown = false;
      state.muxChecking = true;
      state.mode = "mux";
      if (pushHistory) {
        // Same contract as attach: popstate owns terminal-mode transitions, so back leaves the
        // terminal.
        history.pushState({ mode: "mux" }, "", "#mux");
      }
      stopBoardPolling();
      render();                 // shell + "checking" banner; no socket opened yet
      const err = await muxPreflight();
      if (state.muxEpoch !== epoch) return;   // superseded by a later entry or a teardown
      state.muxChecking = false;
      state.muxError = err;
      if (state.mode === "mux") render();
    }

    // Open the terminal on the agent that was tapped. The jump moves the desktop's view too (comux
    // composes one frame for all attached clients), which is what "go to this agent" means here; a
    // per-client view needs the per-pane semantic grid (#65/#66). One jump at a time: two taps are
    // two server-side mutations, the first can land last, and no response guard can undo a jump
    // that already happened.
    async function openAgent(el, token) {
      // Without a pane token there is nothing stable to address (`terminal` ids restart at `term0`
      // each incarnation), so open the terminal without moving anything.
      if (!token) { enterMux(true); return; }
      if (state.jumpBusy) return;
      state.jumpBusy = token;
      el?.classList.add("pending");
      // Navigation can happen under an in-flight jump, and a late response must not reopen the
      // terminal and push a second history entry. `muxEpoch` is bumped by every entry and teardown.
      const epoch = state.muxEpoch;
      const superseded = () => state.muxEpoch !== epoch || state.mode !== "overview";
      try {
        await api("/api/board/jump", { method: "POST", body: JSON.stringify({ token }) });
        state.boardNote = "";
      } catch (e) {
        // A token goes stale on `comux server restart`; opening the focused session anyway would
        // look like a wrong jump.
        state.jumpBusy = null;
        el?.classList.remove("pending");
        if (superseded()) return;
        state.boardNote = e && e.message
          ? `그 pane으로 이동하지 못했습니다 — ${e.message}`
          : "그 pane으로 이동하지 못했습니다";
        render();
        return;
      }
      state.jumpBusy = null;
      el?.classList.remove("pending");
      if (superseded()) return;
      enterMux(true);
    }

    // Paint with WebGL when possible (decisions #166): the DOM renderer re-lays-out a span per cell
    // run each frame. iOS drops WebGL contexts when backgrounded, so a lost context disposes the
    // addon and xterm falls back to the DOM renderer instead of going blank.
    function useGpuRenderer(term) {
      const Webgl = window.WebglAddon?.WebglAddon;
      if (!Webgl) return false;
      try {
        const gl = new Webgl();
        gl.onContextLoss(() => gl.dispose());
        term.loadAddon(gl);
        return true;
      } catch (_) {
        return false; // no WebGL2: the DOM renderer stays
      }
    }

    function openMuxTerminal() {
      if (state.term || state.ws.attach) return;
      const host = document.getElementById("term-host");
      const term = new window.Terminal({
        fontSize: 13,
        fontFamily: '"CopadTerminal", "CopadSymbols", ui-monospace, "JetBrains Mono", "SF Mono", monospace',
        cursorBlink: true,
        scrollback: 5000,
        convertEol: false,
        theme: { background: "#11111b", foreground: "#cdd6f4" },
      });
      const FitAddon = window.FitAddon?.FitAddon;
      const fit = FitAddon ? new FitAddon() : null;
      if (fit) term.loadAddon(fit);
      term.open(host);
      useGpuRenderer(term);
      state.term = term;
      state.fit = fit;
      // xterm's mousedown calls `this.textarea.focus()` unconditionally; redirecting it opens the
      // keyboard for our strip within the same gesture while the mousedown still produces a mouse
      // report, and xterm's key/input/composition handlers never run. The `focusin` bounce in
      // `wireIme` is the backstop for paths that also call `select()`.
      if (term.textarea) {
        term.textarea.focus = () => imeEl()?.focus({ preventScroll: true });
      }
      muxGrid(fit, term);
      // repairFontMetrics refits to the raw viewport and would send a size below the floor;
      // re-apply the floor and send the corrected size.
      whenFontReady(term.options.fontSize, () => { repairFontMetrics({ sendResize: false }); sendMuxResize(); });
      // The symbol font changes no metrics (xterm measures the first family) but its glyphs only
      // appear once loaded, so nudge a repaint.
      whenFontReady(term.options.fontSize, () => term.refresh(0, term.rows - 1), "CopadSymbols");

      const proto = location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${proto}://${location.host}/ws/board/attach`, wsProtocols());
      ws.binaryType = "arraybuffer";
      state.ws.attach = ws;

      ws.onopen = () => {
        state.muxDown = false;
        patchMuxChrome();
        sendMuxResize();
      };
      ws.onmessage = (msg) => {
        if (msg.data instanceof ArrayBuffer) term.write(new Uint8Array(msg.data));
        else term.write(String(msg.data));
      };
      ws.onclose = () => {
        if (state.mode !== "mux") return;
        term.writeln("\r\n\x1b[33m[disconnected]\x1b[0m");
        showDisconnected();
      };
      ws.onerror = () => { try { ws.close(); } catch {} };

      // `onData` carries mouse reports, the paste we ask for, and protocol replies (DA/DSR, focus
      // reports). Only a mouse report can retarget the next keystroke, so only it is ordered behind
      // a pending composition; delaying a reply could time out comux's terminal probe.
      term.onData((d) => {
        if (isMouseReport(d)) withOrder(() => write(d));
        else write(d);
      });
      // A default-encoding (non-SGR) mouse report arrives here, one byte per char. Never through
      // TextEncoder, which would expand a byte above 0x7f into two.
      term.onBinary((d) => {
        const bytes = Uint8Array.from(d, (c) => c.charCodeAt(0) & 0xff);
        withOrder(() => writeBytes(bytes));
      });

      const onResize = () => {
        if (state.mode !== "mux") return;
        sendMuxResize();
      };
      window.addEventListener("resize", onResize);
      window.visualViewport?.addEventListener("resize", onResize);
      // Otherwise every visit leaves a live listener that refits the current terminal and sends a
      // duplicate resize.
      state.muxResizeOff = () => {
        window.removeEventListener("resize", onResize);
        window.visualViewport?.removeEventListener("resize", onResize);
      };
    }

    function openTerminal() {
      // Idempotent: unrelated re-renders must not spawn a second WS/xterm. leaveAttach is the only
      // teardown.
      if (state.term || state.ws.attach) return;
      const host = document.getElementById("term-host");
      const term = new window.Terminal({
        fontSize: 13,
        fontFamily: '"CopadTerminal", "CopadSymbols", ui-monospace, "JetBrains Mono", "SF Mono", monospace',
        cursorBlink: true,
        scrollback: 5000,
        convertEol: false,
        theme: { background: "#11111b", foreground: "#cdd6f4" },
      });
      const FitAddon = window.FitAddon?.FitAddon;
      const fit = FitAddon ? new FitAddon() : null;
      if (fit) term.loadAddon(fit);

      term.open(host);
      useGpuRenderer(term);
      if (fit) fit.fit();
      state.term = term;
      state.fit = fit;

      // xterm measures the glyph cell once at open(), so a webfont landing later leaves every
      // column misplaced. Not awaited before open(): `font-display: swap` already paints the
      // fallback, a wait would blank the terminal for a ~2 MB download, and an await would open a
      // window between the idempotence guard and `state.term` being set in which a re-render spawns
      // a second xterm +
      // WS.
      whenFontReady(term.options.fontSize, repairFontMetrics);

      const proto = location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${proto}://${location.host}/ws/tmux/attach/${encodeURIComponent(state.activePane)}`, wsProtocols());
      ws.binaryType = "arraybuffer";
      state.ws.attach = ws;

      ws.onopen = () => {
        if (fit) {
          fit.fit();
          ws.send(JSON.stringify({ type: "resize", rows: term.rows, cols: term.cols }));
        }
      };
      ws.onmessage = (msg) => {
        if (msg.data instanceof ArrayBuffer) term.write(new Uint8Array(msg.data));
        else term.write(String(msg.data));
      };
      ws.onclose = () => {
        if (state.mode === "attach") {
          term.writeln("\r\n\x1b[33m[disconnected]\x1b[0m");
        }
      };
      ws.onerror = () => { try { ws.close(); } catch {} };

      term.onData((data) => {
        if (state.ctrlSticky && data.length === 1 && data >= " " && data <= "~") {
          const code = data.charCodeAt(0);
          const ctrlByte = code & 0x1f;
          data = String.fromCharCode(ctrlByte);
          state.ctrlSticky = false;
          const el = document.getElementById("ctrl");
          if (el) el.classList.remove("sticky");
        }
        if (ws.readyState === WebSocket.OPEN) ws.send(new TextEncoder().encode(data));
      });

      const onResize = () => {
        if (!state.fit || !state.term || !state.ws.attach) return;
        try {
          state.fit.fit();
          if (state.ws.attach.readyState === WebSocket.OPEN) {
            state.ws.attach.send(JSON.stringify({ type: "resize", rows: state.term.rows, cols: state.term.cols }));
          }
        } catch {}
      };
      window.addEventListener("resize", onResize);

      // iOS's on-screen keyboard does not fire window.resize: it shrinks the visual viewport while
      // the layout viewport stays put, so visualViewport is the only signal. `scroll` matters too
      // because iOS pans the visual viewport to keep the focused element visible.
      const vv = window.visualViewport;
      let vvRaf = 0;
      const onViewport = () => {
        // Coalesce the burst during the keyboard animation to one refit per frame.
        if (vvRaf) return;
        vvRaf = requestAnimationFrame(() => {
          vvRaf = 0;
          setAppHeight();
          onResize();
        });
      };
      if (vv) {
        vv.addEventListener("resize", onViewport);
        // passive: a non-passive scroll listener blocks the compositor during the keyboard
        // animation.
        vv.addEventListener("scroll", onViewport, { passive: true });
      }
      state._removeResize = () => {
        window.removeEventListener("resize", onResize);
        if (vv) {
          vv.removeEventListener("resize", onViewport);
          vv.removeEventListener("scroll", onViewport);
        }
        if (vvRaf) { cancelAnimationFrame(vvRaf); vvRaf = 0; }
      };
    }

    function sendKbdBytes(escape) {
      const ws = state.ws.attach;
      if (!ws || ws.readyState !== WebSocket.OPEN) return;
      let bytes = escape.replace(/\\x([0-9a-fA-F]{2})/g, (_, h) => String.fromCharCode(parseInt(h, 16)))
                        .replace(/\\t/g, "\t");
      // A tap mid-syllable must not overtake it: in mux the payload goes through the ordering
      // owner, so `Ctrl-b n` cannot switch panes ahead of the text the IME holds.
      // Sticky Ctrl: if armed and the next tap is one printable byte, fold it with the 0x1f mask.
      if (state.ctrlSticky && bytes.length === 1 && bytes >= " " && bytes <= "~") {
        bytes = String.fromCharCode(bytes.charCodeAt(0) & 0x1f);
        clearSticky();
      }
      if (state.mode === "mux") emit(bytes);
      else ws.send(new TextEncoder().encode(bytes));
    }

    // Which `onData` payloads can retarget the input. SGR reports are `ESC [ < ...M/m`, default
    // encoding ones `ESC [ M ...`; neither collides with an application reply (`ESC [ ? ...c`,
    // `ESC [ ...R`). xterm 5.5 exposes no provenance on that channel, so this is version-specific.
    function isMouseReport(d) {
      return typeof d === "string" && (d.startsWith("\x1b[<") || d.startsWith("\x1b[M"));
    }

    function openEventStream() {
      if (state.ws.events) { try { state.ws.events.close(); } catch {} }
      const proto = location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${proto}://${location.host}/ws/events`, wsProtocols());
      state.ws.events = ws;
      ws.onmessage = (msg) => {
        try {
          const ev = JSON.parse(msg.data);
          state.events.push(ev);
          if (state.events.length > 50) state.events.shift();
          if ((ev.type || ev.kind) === "presence.changed") {
            const cur = ev.data?.current || ev.payload?.current;
            if (cur) state.presence = cur;
          }
          // A pilot.* bus event means the queue changed; refresh the cockpit.
          if ((ev.type || ev.kind || "").startsWith("pilot.")) loadPilot();
          if (state.mode === "overview") render();
        } catch {}
      };
      ws.onclose = () => { state.ws.events = null; if (canReconnect()) setTimeout(openEventStream, 2000); };
      ws.onerror = () => { try { ws.close(); } catch {} };
    }

    async function initPush() {
      // No Push API on iOS Safari before 16.4, and a service worker needs a secure context.
      if (!("serviceWorker" in navigator) || !("PushManager" in window)) {
        state.push.status = "unsupported";
        return;
      }
      try {
        await navigator.serviceWorker.register("/sw.js");
        const reg = await navigator.serviceWorker.ready;
        const existing = await reg.pushManager.getSubscription();
        if (existing) {
          state.push.status = "on";
          // Kinds preference is sticky in local storage; the server is the source of truth on next
          // mutate.
          try {
            const k = JSON.parse(localStorage.getItem("copad.push.kinds") || "null");
            if (Array.isArray(k)) state.push.kinds = k;
          } catch {}
        } else {
          state.push.status = Notification.permission === "denied" ? "denied" : "off";
        }
      } catch (e) {
        state.push.status = "off";
        state.push.error = String(e && e.message || e);
      }
    }

    async function togglePush() {
      if (state.push.status === "unsupported") return;
      if (state.push.status === "on") return disablePush();
      return enablePush();
    }

    async function enablePush() {
      try {
        const perm = await Notification.requestPermission();
        if (perm !== "granted") {
          state.push.status = "denied";
          state.push.error = "permission " + perm;
          render();
          return;
        }
        const { public_key } = await api("/api/push/vapid-public");
        const reg = await navigator.serviceWorker.ready;
        const sub = await reg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: urlB64ToUint8Array(public_key),
        });
        const subJson = sub.toJSON();
        const r = await api("/api/push/subscribe", {
          method: "POST",
          body: JSON.stringify({
            endpoint: subJson.endpoint,
            keys: subJson.keys,
            kinds: state.push.kinds,
          }),
        });
        state.push.status = "on";
        state.push.subId = r.id;
        state.push.error = "";
        localStorage.setItem("copad.push.kinds", JSON.stringify(state.push.kinds));
        render();
      } catch (e) {
        state.push.error = "subscribe failed: " + String(e && e.message || e);
        render();
      }
    }

    async function disablePush() {
      try {
        const reg = await navigator.serviceWorker.ready;
        const sub = await reg.pushManager.getSubscription();
        if (sub) {
          // Best-effort server prune by sha256(endpoint); the browser-side unsubscribe is
          // authoritative.
          try {
            const enc = new TextEncoder().encode(sub.endpoint);
            const digest = await crypto.subtle.digest("SHA-256", enc);
            const hex = Array.from(new Uint8Array(digest)).map(b => b.toString(16).padStart(2, "0")).join("");
            await api(`/api/push/subscribe/${hex}`, { method: "DELETE" });
          } catch {}
          await sub.unsubscribe();
        }
        state.push.status = "off";
        state.push.subId = null;
        state.push.error = "";
        render();
      } catch (e) {
        state.push.error = "unsubscribe failed: " + String(e && e.message || e);
        render();
      }
    }

    async function updatePushKinds(kind, on) {
      const cur = new Set(state.push.kinds);
      if (on) cur.add(kind); else cur.delete(kind);
      state.push.kinds = Array.from(cur);
      localStorage.setItem("copad.push.kinds", JSON.stringify(state.push.kinds));
      // Re-subscribe with new kinds; the server dedupes by sha256(endpoint), so this is idempotent.
      if (state.push.status !== "on") return;
      try {
        const reg = await navigator.serviceWorker.ready;
        const sub = await reg.pushManager.getSubscription();
        if (!sub) return;
        const subJson = sub.toJSON();
        await api("/api/push/subscribe", {
          method: "POST",
          body: JSON.stringify({
            endpoint: subJson.endpoint,
            keys: subJson.keys,
            kinds: state.push.kinds,
          }),
        });
      } catch (e) {
        state.push.error = "kinds update failed: " + String(e && e.message || e);
        render();
      }
    }

    // Run `cb` once the terminal webfont is usable; never if it cannot load (e.g. /font/terminal
    // 404s), leaving the fallback stack.
    function whenFontReady(px, cb, family = "CopadTerminal") {
      if (!document.fonts?.load) return;
      const spec = `${px || 13}px "${family}"`;
      document.fonts
        .load(spec)
        .then(() => {
          // load() resolves even when nothing matched; check() proves the face is there.
          if (document.fonts.check(spec)) cb();
        })
        .catch(() => {});
    }

    // Re-measure after a webfont that landed after term.open(). Reassigning fontFamily is what
    // makes xterm drop its cached glyph metrics; then refit and tell the PTY, since rows/cols
    // changed.
    function repairFontMetrics(opts = {}) {
      const { sendResize = true } = opts;
      const term = state.term;
      if (!term) return;
      try {
        // Must assign a different value: xterm's option setter short-circuits on an unchanged
        // value, so round-trip through a distinct-but-equivalent stack.
        const ff = term.options.fontFamily;
        term.options.fontFamily = "monospace";
        term.options.fontFamily = ff;
        state.fit?.fit();
        const ws = state.ws.attach;
        if (sendResize && ws && ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: "resize", rows: term.rows, cols: term.cols }));
        }
      } catch {}
    }

    // Publish the visual viewport height as --app-h. Full-height views use it instead of
    // 100vh/100dvh, because only the visual viewport shrinks for the on-screen keyboard.
    function setAppHeight() {
      const vv = window.visualViewport;
      const h = Math.round(vv ? vv.height : window.innerHeight);
      if (h > 0) document.documentElement.style.setProperty("--app-h", h + "px");
    }

    // Global, since the overview/pilot views raise the keyboard too. The attach view registers its
    // own handler as well; both call setAppHeight so neither depends on registration order.
    (function trackViewport() {
      setAppHeight();
      const vv = window.visualViewport;
      if (vv) {
        vv.addEventListener("resize", setAppHeight);
        vv.addEventListener("scroll", setAppHeight, { passive: true });
      }
      window.addEventListener("orientationchange", setAppHeight);
      window.addEventListener("resize", setAppHeight);
    })();

    async function bootstrap() {
      state.presence = "…";
      try {
        const p = await api("/api/presence");
        state.presence = p.state;
        state.noGui = false;
      } catch (e) {
        if (e && e.code === "unauthorized") return;
        state.presence = "?";
      }
      try {
        // Called for its failure mode: a `no_gui` rejection is how the page learns the copad GUI
        // is not running.
        await api("/api/panes");
      } catch (e) {
        if (e && e.code === "no_gui") state.noGui = true;
      }
      await initPush();
      render();
      openEventStream();
      // `/ws/tmux/overview` is deliberately not opened: it shells out to tmux every 5 s and pushes
      // empty snapshots on a comux machine. The endpoint stays for installs that still run tmux.
      startBoardPolling();
      // Initial load plus a slow poll as a backstop; pilot.* events drive the responsive refresh.
      loadPilot();
      if (!state.pilotPoll) {
        state.pilotPoll = setInterval(() => { if (state.mode === "overview") loadPilot(); }, 8000);
      }
    }

    async function togglePresence() {
      const next = state.presence === "away" ? "active" : "away";
      try {
        const r = await api("/api/presence", { method: "POST", body: JSON.stringify({ state: next }) });
        state.presence = r.current || next;
        // Both terminal views patch in place: a full render() tears out xterm's host element, and
        // openTerminal/openMuxTerminal then no-op on their idempotence guard because `state.term`
        // still exists, leaving a live socket attached to a terminal with no DOM.
        if (state.mode === "attach" || state.mode === "mux") {
          const el = document.getElementById("presence-toggle");
          if (el) {
            el.textContent = state.presence;
            el.className = "chip " + (state.presence === "away" ? "warn" : "ok");
          }
        } else {
          render();
        }
      } catch (e) {
        alert(`presence: ${e.code || ""} ${e.message || ""}`);
      }
    }

    function enterAttach(paneId) {
      state.activePane = paneId;
      state.mode = "attach";
      // Push a history entry so the platform back gesture detaches instead of leaving the page.
      // popstate is the single source of truth for transitions, so URL and `state.mode` never
      // diverge.
      history.pushState({ mode: "attach", pane: paneId }, "", `#pane/${encodeURIComponent(paneId)}`);
      render();
    }

    // Tear down attach resources without touching history; the caller controls the history entry.
    function teardownAttach() {
      if (state.ws.attach) { try { state.ws.attach.close(); } catch {} state.ws.attach = null; }
      if (state.term) { try { state.term.dispose(); } catch {} state.term = null; }
      state.fit = null;
      if (state._removeResize) { state._removeResize(); state._removeResize = null; }
      state.activePane = null;
      state.mode = "overview";
      // The board's poll loop exits when the mode is not `overview`, and a `#pane/<id>` deep link
      // boots straight into attach, so restart it here.
      startBoardPolling();
    }

    function leaveAttach() {
      if (history.state && history.state.mode === "attach") {
        history.back();
      } else {
        teardownAttach();
        render();
      }
    }

    // Leave whichever terminal mode is live. Both own an xterm + socket and `render()` drops the
    // host element, so switching without teardown leaves a blank screen typing into the old socket.
    function teardownAnyTerminal() {
      if (state.mode === "attach") teardownAttach();
      else if (state.mode === "mux") teardownMux();
    }

    // Mobile browsers freeze timers in a background tab, so refresh when the page comes back.
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible" && state.mode === "overview" && isAuthed()) {
        startBoardPolling();
      }
    });

    window.addEventListener("popstate", (e) => {
      const target = (e.state && e.state.mode) || "overview";
      if (target === "attach" && e.state.pane) {
        if (state.mode !== "attach" || state.activePane !== e.state.pane) {
          teardownAnyTerminal();
          state.activePane = e.state.pane;
          state.mode = "attach";
          render();
        }
      } else if (target === "mux") {
        if (state.mode !== "mux") {
          teardownAnyTerminal();
          enterMux(false);
        }
      } else if (state.mode !== "overview") {
        teardownAnyTerminal();
        render();
      }
    });

    function bootFromHash() {
      const m = location.hash.match(/^#pane\/(.+)$/);
      if (m) {
        const paneId = decodeURIComponent(m[1]);
        history.replaceState({ mode: "overview" }, "", location.pathname + location.search);
        history.pushState({ mode: "attach", pane: paneId }, "", `#pane/${encodeURIComponent(paneId)}`);
        state.activePane = paneId;
        state.mode = "attach";
      } else {
        history.replaceState({ mode: "overview" }, "", location.pathname + location.search + location.hash);
      }
    }

    // Bearer only: with a cached token, boot; without one, show the setup page.
    (() => {
      if (token) {
        bootFromHash(); bootstrap();
      } else {
        render();
      }
    })();
  })();
