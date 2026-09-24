  (() => {
    const root = document.getElementById("root");
    let token = sessionStorage.getItem("copad.token") || "";
    // Auth mode is decided by /api/whoami on boot. tailscale-mode
    // skips the token setup page entirely + omits the Authorization
    // header from /api/* calls + the bearer.<token> subprotocol from
    // /ws/* upgrades (the middleware trusts Tailscale-User-Login).
    // Bearer is the only auth mode. The Tailscale identity-header path was removed: serve
    // attaches that header to every request it forwards, WebSocket upgrades are not subject to
    // CORS, and `/ws/board/attach` hands back a terminal — so any page visited on an admitted
    // device could have driven it with no token. See docs/mobile-access.md.

    function api(path, opts = {}) {
      opts.headers = Object.assign({}, opts.headers || {});
      // Only send the Bearer header in bearer mode — in tailscale
      // mode we have no token and the server would 401 on an empty
      // bearer anyway.
      if (token) {
        opts.headers["Authorization"] = "Bearer " + token;
      }
      if (opts.body && !opts.headers["Content-Type"]) opts.headers["Content-Type"] = "application/json";
      return fetch(path, opts).then(async (r) => {
        if (r.status === 401) {
          // Only the bearer path treats 401 as "token went bad,
          // reprompt". In tailscale mode a 401 means the proxy lost
          // the header — surface it as a generic error rather than
          // dropping the user back to a setup page that doesn't
          // apply to them.
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
      rawInput: false,          // keystrokes go straight to the PTY (no IME composition)
      composeNote: "",          // why a send did not go out; the draft is kept
      muxChecking: false,       // a preflight is in flight (drives the banner)
      muxEpoch: 0,              // bumped on every entry/teardown; stale preflights are dropped
      muxResizeOff: null,       // detaches the mux resize listeners on leave
      presence: null,
      tmuxPanes: [],
      events: [],
      noGui: false,
      pilot: null,              // /api/pilot/status result: {pilot:{goals,active,gate,counts}, csd, tmx, errors}
      pilotError: "",           // set when pilot.status RPC fails (plugin not running)
      pilotAdd: false,          // add-goal form expanded?
      activePane: null,         // pane_id while in attach mode
      ws: { events: null, overview: null, attach: null },
      term: null,               // xterm.js Terminal instance
      fit: null,                // FitAddon
      ctrlSticky: false,
      // PWA push notification state. `status`:
      //   "unsupported" — browser missing serviceWorker / PushManager
      //   "denied" — Notification.permission === "denied"
      //   "off" — supported but user hasn't subscribed
      //   "on" — subscribed; server has a stored Subscription
      // kinds is the user-selected filter (empty = receive every kind);
      // we default to a sensible "useful Claude/Codex" set.
      push: {
        status: "unsupported",
        kinds: ["notification", "stop", "codex-turn"],
        subId: null,
        error: "",
      },
    };

    // URL-safe base64 → Uint8Array. PushManager.subscribe wants the
    // VAPID public key as a Uint8Array, but we transport it as the
    // url-safe base64 string the spec uses.
    function urlB64ToUint8Array(b64) {
      const pad = "=".repeat((4 - b64.length % 4) % 4);
      const norm = (b64 + pad).replace(/-/g, "+").replace(/_/g, "/");
      const raw = atob(norm);
      const out = new Uint8Array(raw.length);
      for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
      return out;
    }

    // Subprotocol list for WebSocket constructors. The server's
    // upgrade handshake auth checks the `bearer.<token>` subprotocol
    // when a token is present; in tailscale mode there's no token, so
    // we omit the second argument and let the Tailscale-User-Login
    // header (injected on the upgrade request like any HTTP request)
    // satisfy the middleware.
    function wsProtocols() {
      return token ? [`bearer.${token}`] : undefined;
    }
    // True when the SPA still has a way to authenticate the next
    // reconnect attempt — tailscale mode is always-eligible since
    // the proxy keeps injecting the header; bearer mode needs the
    // cached token. Used by overview/events WS onclose handlers so
    // tailscale sessions don't silently stop reconnecting.
    function canReconnect() {
      return !!token;
    }

    function escapeHtml(s) {
      return String(s == null ? "" : s).replace(/[&<>"']/g, c => ({ "&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;" }[c]));
    }
    function stripAnsi(s) { return String(s).replace(/\x1b\[[0-9;]*[a-zA-Z]/g, ""); }
    function fmtTs(ms) { if (!ms) return ""; return new Date(ms).toTimeString().slice(0,8); }
    // Relative-age string matching tmx's human_age / attention-picker.sh.
    // `ms` is epoch-millis; `tsSec` is epoch-seconds. Returns "5s"/"3m"/
    // "2h"/"1d" or "" when missing so callers can chain conditionally.
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
        // honor `#pane/<id>` deep-links that arrived through the
        // pre-auth login flow — without this, a phone shortcut to a
        // specific pane drops you on overview after the first login.
        bootFromHash();
        bootstrap();
      };
      btn.addEventListener("click", go);
      inp.addEventListener("keydown", e => { if (e.key === "Enter") go(); });
    }

    // Stable sort key — lower = higher on the page. Order: waiting
    // (user-blocking) > busy (actively running) > idle (parked) > no
    // agent (plain shell). Tier 4 panes preserve tmux's own order; tier
    // 0-2 are tie-broken by newer updated_at_ms so the most recently
    // active session of a tier floats up.
    // tmx 1.x status vocabulary (verified against live snapshot):
    //   working           — mid-turn (was: busy)
    //   awaiting-decision — permission / selection dialog (was: waiting)
    //   ready             — at chat prompt
    //   idle              — plain shell / no signal
    // `blocked` flag overrides everything — it means the harness has
    // stop-blocked the session, the user must look.
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
    // Map tmx status → existing pill CSS class. Keeps the warn/err/ok
    // palette consistent without renaming the underlying styles.
    function statusToPillClass(s) {
      if (s === "working") return "busy";
      if (s === "awaiting-decision") return "waiting";
      if (s === "ready" || s === "idle") return "idle";
      return "";
    }
    function paneSorted(panes) {
      // tmx no longer surfaces a per-agent updated_at, so within a
      // tier we fall back to tmux's natural order (stable sort).
      return panes.slice().sort((a, b) => paneSortRank(a) - paneSortRank(b));
    }

    /* ==== the comux fleet board ====
     *
     * This replaces the tmx-fed `attention` strip and `codex jobs` cards that used to sit in
     * the overview. Their only producer was the /ws/tmux/overview stream, which this build no
     * longer opens, so they could only ever render empty here; the board answers the same
     * question (which agent needs me) from comux, which is what actually runs on this machine.
     */

    // Status comes from comux and is CACHED AND INFERRED: only Claude's is read from a session
    // file, everything else is matched against screen text, and `idle` doubles as "no
    // recognized UI" — i.e. an unresolved reading. So the partition below must be EXHAUSTIVE
    // and must preserve the raw label. An earlier draft listed blocked/working/idle only,
    // which would have hidden `ready` — 26 of the 27 agents on this machine.
    const BOARD_ATTENTION = new Set(["blocked"]);
    const BOARD_ACTIVE = new Set(["working"]);

    // Group key. `space_id` is the stable session id, but an older comux does not send it, and
    // two sessions may legitimately share a display name — so a name-based key is NAMESPACED
    // and marked ambiguous rather than silently merged with an id-based one.
    function boardGroupKey(a) {
      return a.space_id ? `id:${a.space_id}` : `name:${a.space || "?"}`;
    }

    /// The tool worth naming is the one that is NOT what everything else is.
    ///
    /// Every agent here is `claude`, so printing it on all 27 rows is 27 repetitions of one
    /// fact and it costs a column on a phone. Name the tool only where it differs from the
    /// fleet's dominant one — then seeing `codex` on a row actually means something.
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
      // `detail` is what the agent was last seen DOING, straight from its own log. Absent means
      // "no reading", never "doing nothing" — an older comux does not send it at all — so it is
      // rendered when present and simply left out otherwise, never as a placeholder.
      const detail = a.detail
        ? `<div class="row-detail">${escapeHtml(a.detail)}</div>` : "";
      const tool = hideTool ? "" : `<span class="row-tool">${escapeHtml(a.tool || "")}</span>`;
      // The layout encodes three statuses positionally: `blocked` is the attention band,
      // `working` is the working band, `ready` is a plain dot in the idle list. Anything else
      // gets its raw label printed, because collapsing it into the same grey dot would hide a
      // real distinction — comux documents `idle` as ALSO meaning "no recognized UI", i.e. an
      // unresolved reading, and an unknown status must not silently read as ready.
      const encoded = BOARD_ATTENTION.has(a.status) || BOARD_ACTIVE.has(a.status)
        || a.status === "ready";
      const status = encoded ? "" : `<span class="row-status">${escapeHtml(a.status || "?")}</span>`;
      return `
        <div class="agent-row" data-token="${escapeHtml(a.token || a.terminal || "")}">
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
        `<div class="banner">fleet: ${escapeHtml(e.what)} — ${escapeHtml(e.message || e.code)}</div>`).join("");
      // Computed BEFORE the early return: without it, a transport failure on top of an
      // already-partial response would present the retained session count as current.
      const stale = state.boardStale
        ? `<div class="banner">마지막으로 읽은 내용 — 브리지가 응답하지 않음</div>` : "";

      // null means "could not read"; [] means "read, and empty". Rendering them the same would
      // report a failed read as an empty fleet — and the agents list IS the board, so
      // substituting [] would print a confident "idle 0" that is simply false.
      if (b.agents === null) {
        return `${errs}${stale}<div class="board-empty">에이전트 목록을 읽지 못했습니다${
          b.sessions ? ` — 마지막 읽기 기준 세션 ${b.sessions.length}개` : ""}</div>`;
      }
      const agents = b.agents;
      const only = dominantTool(agents);   // null when the fleet is mixed
      const attention = agents.filter(a => BOARD_ATTENTION.has(a.status));
      const active = agents.filter(a => BOARD_ACTIVE.has(a.status));
      const rest = agents.filter(a => !BOARD_ATTENTION.has(a.status) && !BOARD_ACTIVE.has(a.status));

      // Groups are ALWAYS expanded. There is no toggle: on a phone the question is "what is
      // going on", and an answer you have to unfold one session at a time is not an answer.
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

      // Ambiguous grouping is a property of the SERVER, not of one session: an older comux
      // sends no session id at all, so marking every group "by name" would repeat one global
      // fact ten times. Per-group only when some groups are keyed by id and some are not.
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

    // One outstanding request at a time, the next scheduled only after the previous completes,
    // and stale responses dropped by epoch. A fixed interval would stack requests whenever the
    // bridge is slow — and /api/board can spend two sequential subprocess deadlines.
    async function loadBoard() {
      // One request at a time, not one per caller. `startBoardPolling` is invoked on return
      // from the terminal AND on every foreground, so without this a phone waking up could put
      // several fetches in flight at once — and each /api/board can spend two sequential
      // subprocess deadlines server-side.
      if (state.boardInFlight) return;
      state.boardInFlight = true;
      const epoch = ++state.boardEpoch;
      let deadline = null;
      try {
        // `api()` has no timeout of its own. A request that never settles — a phone that lost
        // signal mid-flight is the ordinary case — would otherwise leave `boardInFlight` set
        // forever, silently skipping every later refresh while the board showed data that was
        // never marked stale. The fetch is ABORTED rather than merely raced: abandoning it
        // would leave the old request outstanding while the next poll started another, which
        // is the overlap the single-flight guard exists to prevent.
        const ctl = new AbortController();
        deadline = setTimeout(() => ctl.abort(), 15000);
        const b = await api("/api/board", { signal: ctl.signal });
        if (state.boardEpoch !== epoch) return;
        state.board = b;
        state.boardStale = false;
      } catch {
        if (state.boardEpoch !== epoch) return;
        // A request that never returns is invisible to the API's own `errors[]`, so transport
        // failure marks what is on screen as stale rather than blanking it.
        state.boardStale = true;
      } finally {
        if (deadline) clearTimeout(deadline);
        state.boardInFlight = false;
      }
      if (state.mode === "overview") render();
    }

    // The same condition `render()` uses to decide between the app and the token page. Polling
    // must respect it: on the setup screen `state.mode` is still "overview", so a foreground
    // event would start the loop, every /api/board would 401, and `api()`'s 401 handler clears
    // the token and re-renders — wiping the field the user is in the middle of pasting into,
    // every five seconds.
    function isAuthed() {
      return !!token;
    }

    function startBoardPolling() {
      if (!isAuthed()) { stopBoardPolling(); return; }
      // Only the TIMER is reset here, deliberately not the request epoch. `startBoardPolling`
      // runs on every foreground and on every return from the terminal, and invalidating an
      // in-flight request there would throw away a good response that is about to land and
      // then wait a full interval for its replacement. Invalidation belongs to leaving
      // (`stopBoardPolling`).
      clearBoardTimer();
      // Each loop carries a generation, and only the CURRENT generation may schedule. Without
      // it a restart during an in-flight request forks the loop: the new tick returns early on
      // the single-flight guard and schedules, then the original tick completes and schedules
      // too — two chains from then on, each polling.
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
      // The tmux-pane cards and copad-panel cards that used to be built here are gone with
      // the tmux data model; `renderBoard()` below is the overview's content now. Their
      // computation was removed rather than left unrendered — it walked state that nothing
      // populates any more and ran on every poll.
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
      // Opening the terminal is the only action a row can offer today: comux's v1 transport
      // composes ONE frame for all clients, so the phone cannot show a single pane, and
      // `jump` would move the desktop user's view. Targeted open waits on the per-pane
      // protocol (decisions #65/#66).
      document.querySelectorAll(".agent-row").forEach(el => {
        el.addEventListener("click", () => enterMux(true));
      });
      document.querySelectorAll(".card[data-pane]").forEach(el => {
        el.addEventListener("click", () => enterAttach(el.dataset.pane));
      });
      wirePilot();
    }

    // Resolve a "session:window_idx" tmux_target (or bare session name)
    // to a concrete pane_id from the current snapshot. Preference order:
    // active pane in the exact window → first pane in the window → any
    // pane in the session. Null when nothing matches (stale entry whose
    // session has since died).

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
      // preventDefault on mousedown/touchstart stops the button from
      // stealing focus away from xterm's hidden textarea. Without
      // this, every toolbar tap blurs the terminal and the user has
      // to click the terminal again before the next key works — the
      // sticky-Ctrl combo becomes "Ctrl, click terminal, type letter"
      // instead of "Ctrl, letter". After the click fires we still
      // call term.focus() as a belt-and-braces refocus in case any
      // platform quirk drained the textarea.
      wireKbdBar();
      openTerminal();
    }

    // Shared by both terminal views. The preventDefault on mousedown/touchstart is what keeps
    // focus in xterm's textarea, so the on-screen keyboard never closes mid-chord and a Ctrl
    // press reads as "Ctrl+letter" rather than "Ctrl, letter".
    function wireKbdBar() {
      // Mode-aware: in compose mode this MUST go back to the compose bar. Sending it to
      // xterm's textarea — which is what every keybar tap used to do, including the new Ctrl-C
      // — hands the iOS keyboard back to xterm and the next Korean word decomposes again. The
      // toggle would still read 조합 while the bug was back.
      const refocus = () => {
        try {
          if (state.mode === "mux" && !state.rawInput) composeEl()?.focus();
          else state.term?.focus();
        } catch {}
      };
      document.querySelectorAll(".kbd-bar button").forEach(btn => {
        btn.addEventListener("mousedown", e => e.preventDefault());
        btn.addEventListener("touchstart", e => e.preventDefault(), { passive: false });
      });
      document.querySelectorAll(".kbd-bar button[data-bytes]").forEach(btn => {
        btn.addEventListener("click", () => { sendKbdBytes(btn.dataset.bytes); refocus(); });
      });
      document.getElementById("ctrl")?.addEventListener("click", () => {
        state.ctrlSticky = !state.ctrlSticky;
        document.getElementById("ctrl").classList.toggle("sticky", state.ctrlSticky);
        refocus();
      });
    }

    // --- Phase 24.6 pilot orchestration cockpit ---

    function shortId(id) { return (id || "").replace(/^g-/, "").slice(0, 8); }

    // Build the pilot strip for the overview. Renders the goal queue with
    // per-goal status pills, the active goal's gate (with answer/approve
    // controls), a cancel affordance, and an add-goal form.
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
        // pilot plugin not running / no daemon → show a banner, keep polling.
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
      // Tailscale mode never needs a token — go straight to the app.
      // Bearer mode needs one; otherwise show the setup prompt.
      if (!token) { renderSetup(); return; }
      if (state.mode === "mux") { renderMux(); return; }
      if (state.mode === "attach") { renderAttach(); return; }
      renderOverview();
    }

    /* ==== comux terminal (the mobile terminal) ==== */

    // The grid is whatever the phone actually fits. There is deliberately NO column floor.
    //
    // An earlier build floored it at 80 to stop the phone reflowing the desktop, since comux
    // composes one frame sized to the smallest attached client. That was the wrong trade twice
    // over. A phone fits roughly 40 columns, so an 80-column grid meant horizontal scrolling
    // through content that did not fit — and 80 is exactly comux's `sidebar_min_cols` default,
    // the one value at which its sidebar stays VISIBLE, so 24 of those 80 columns went to a
    // sidebar the phone does not need. The result on a real device was a screen with nothing
    // readable on it.
    //
    // Fitting the viewport hides that sidebar for free — comux checks `cols >= sidebar_min_cols`
    // itself — without touching `Ctrl-b s`, which is shared state and would hide it on the
    // desktop too. The fleet board is the phone's sidebar.
    //
    // The cost is real and larger than "while attached": the composed frame and the PTYs follow
    // the narrowest client, and if the phone is the LAST one to detach the geometry stays
    // narrow until something attaches again. Accepted — the premise of this product is that
    // nobody is at the desktop. The fix is the per-pane semantic grid (decisions #65/#66).
    //
    // The sizing must be the BROWSER's logical grid, not just the PTY: comux probes the terminal
    // for its true size on focus and sends back what it gets, so a PTY-only clamp is overwritten.
    const MUX_MIN_COLS = 20;   // not a layout choice — just never send a degenerate size
    const MUX_MIN_ROWS = 8;

    function muxGrid(fit, term) {
      if (fit) fit.fit();
      const cols = Math.max(term.cols, MUX_MIN_COLS);
      const rows = Math.max(term.rows, MUX_MIN_ROWS);
      if (cols !== term.cols || rows !== term.rows) term.resize(cols, rows);
      return { cols, rows };
    }

    // Apply the floor and tell the PTY the SAME grid the browser is drawing. Every path that
    // can change the size goes through here, so xterm and the PTY cannot drift apart.
    function sendMuxResize() {
      if (!state.term) return;
      const { cols, rows } = muxGrid(state.fit, state.term);
      const ws = state.ws.attach;
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "resize", rows, cols }));
      }
    }

    // Replace the view's chrome while KEEPING a live terminal's DOM.
    //
    // Any async handler that finishes while mux is open calls `render()` — the push
    // subscribe/unsubscribe completions do, and so would anything added later. A plain
    // `root.innerHTML = …` throws away xterm's host element, and `openMuxTerminal` then
    // returns early on its idempotence guard because the Terminal and socket objects still
    // exist: a blank screen whose toolbar still types into an invisible terminal. Detaching
    // the live host first and putting it back into the freshly rendered slot preserves the
    // terminal, its scrollback and its socket, so no caller has to know mux is open.
    function renderPreservingTerminal(html) {
      // The compose bar is preserved for a second reason beyond the terminal's: it may be in
      // the middle of a native IME composition, and a composition lives in the ELEMENT. Copying
      // its string value into a fresh element would drop the half-built syllable and leave the
      // keyboard in a state the page cannot see.
      const keep = ["term-host", "compose-wrap"]
        .map((id) => document.getElementById(id))
        .filter(Boolean);
      for (const el of keep) el.remove();
      root.innerHTML = html;
      for (const el of keep) document.getElementById(el.id)?.replaceWith(el);
    }

    // The mux view is BUILT ONCE and patched thereafter — it is never re-rendered.
    //
    // Preserving the compose element across a re-render is not enough, and that was the bug:
    // `el.remove()` disconnects a focused textarea, which resets focus and ends the native IME
    // composition. Re-inserting the same node and calling `.focus()` cannot resurrect a
    // half-built syllable. So nothing removes it: later renders only patch the chrome. This
    // also stops the event listeners accumulating, one set per render, on the preserved nodes.
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
          <button class="chip" id="raw-toggle"></button>
          <button class="chip" id="presence-toggle"></button>
        </header>
        <main class="attach">
          <div id="mux-banner"></div>
          <div id="term-host" class="mux"></div>
          <div id="compose-wrap" class="compose-wrap">
            <textarea id="compose" rows="1" enterkeyhint="send" autocapitalize="off"
                      autocorrect="off" spellcheck="false"
                      placeholder="한글 입력 · Enter 전송"></textarea>
            <button id="compose-send" class="chip ok">보내기</button>
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
        </main>
        </div>`);
      document.getElementById("back").addEventListener("click", leaveMux);
      document.getElementById("presence-toggle").addEventListener("click", togglePresence);
      document.getElementById("raw-toggle").addEventListener("click", toggleRawInput);
      wireKbdBar();
      wireCompose();
      patchMuxChrome();
    }

    /// Update everything about the mux view that can change, WITHOUT touching the DOM the
    /// terminal and the IME live in.
    function patchMuxChrome() {
      const raw = document.getElementById("raw-toggle");
      if (raw) {
        raw.textContent = state.rawInput ? "직접" : "조합";
        raw.classList.toggle("active", state.rawInput);
      }
      const pres = document.getElementById("presence-toggle");
      if (pres) {
        pres.textContent = state.presence || "\u2026";
        pres.className = "chip " + (state.presence === "away" ? "warn" : "ok");
      }
      const banner = document.getElementById("mux-banner");
      if (banner) {
        const msg = state.muxChecking ? "checking comux\u2026" : state.muxError;
        banner.innerHTML = msg ? `<div class="banner">${escapeHtml(msg)}</div>` : "";
      }
      renderComposeNote();
      // Only once the preflight has answered, and only if it said yes.
      if (!state.muxChecking && !state.muxError) openMuxTerminal();
    }

    // Drop the terminal + socket + listeners WITHOUT changing the mode. Split out because
    // re-entering mux must also do this: `render()` replaces the host element, and
    // `openMuxTerminal` would then no-op on its idempotence guard, leaving a blank screen
    // whose toolbar still types into the old socket.
    function disposeMuxTerminal() {
      try { state.muxResizeOff?.(); } catch {}
      state.muxResizeOff = null;
      if (state.ws.attach) { try { state.ws.attach.close(); } catch {} state.ws.attach = null; }
      if (state.term) { try { state.term.dispose(); } catch {} state.term = null; }
      state.fit = null;
    }

    // Tear down mux WITHOUT touching history — the mirror of `teardownAttach`, so popstate
    // owns the transition and neither path double-pops. Bumping the epoch is what lets a
    // Back-then-Forward while a preflight is still in flight open a fresh one instead of
    // being swallowed by a guard the old request never released.
    function teardownMux() {
      disposeMuxTerminal();
      state.muxEpoch++;
      state.muxChecking = false;
      state.mode = "overview";
      state.muxError = "";
      startBoardPolling();      // refresh immediately on return, then resume the cadence
    }

    /* ==== composed text input ====
     *
     * Typing Korean straight into xterm produces DECOMPOSED jamo on iOS — `잘 접근되는데`
     * arrives as `ㅈㅏㄹ ㅈㅓㅂㄱㅡㄴㄷㅗㅣㄴㅡㄴㄷㅔ`. Reported from the device. On a desktop
     * the OS composes a syllable before the terminal ever sees it, which is why nothing I ran
     * locally could catch it.
     *
     * The mechanism is attributed to WebKit not driving composition inside the hidden textarea
     * xterm owns and keeps moving, but I have NOT verified that claim at the source — what is
     * verified is the symptom and that composing outside xterm fixes it. So: a real textarea
     * the IME owns, and only finished text reaches the PTY.
     *
     * A textarea, not an `<input>`: an input silently strips newlines, so a pasted two-line
     * command would become a different single-line command with nothing to see.
     */

    // Enter is ambiguous during Korean input: WebKit fires `compositionend` BEFORE the keydown
    // of the Enter that CONFIRMED the candidate, so `isComposing` is already false by then and
    // submitting on it would swallow that Enter and send a syllable early. Treat an Enter that
    // lands immediately after a composition ended as the confirming one.
    const COMPOSE_CONFIRM_GRACE_MS = 50;
    let lastCompositionEnd = 0;

    // Should this keydown send the draft?
    //
    // Pulled out as a pure function because it is the whole correctness of Korean input and it
    // is otherwise unreachable from a test. Three separate ways an Enter is NOT a submission:
    //   * Shift+Enter — the user is inserting a newline.
    //   * `isComposing` / keyCode 229 — the IME is mid-syllable.
    //   * an Enter that arrives within the grace window after `compositionend` — WebKit fires
    //     compositionend BEFORE the keydown of the Enter that CONFIRMED the candidate, so
    //     `isComposing` is already false and this is the only thing separating "confirm the
    //     syllable" from "send the line".
    function shouldSubmitOnEnter(e, now, lastEnd) {
      if (e.key !== "Enter" || e.shiftKey) return false;
      if (e.isComposing || e.keyCode === 229) return false;
      return now - lastEnd >= COMPOSE_CONFIRM_GRACE_MS;
    }

    function composeEl() {
      return document.getElementById("compose");
    }

    function focusForMode() {
      if (state.rawInput) {
        try { state.term?.focus(); } catch {}
      } else {
        composeEl()?.focus();
      }
    }

    function toggleRawInput() {
      // Never flushes the draft — switching modes is navigation, not submission.
      state.rawInput = !state.rawInput;
      render();
      focusForMode();
    }

    function submitCompose() {
      const el = composeEl();
      if (!el) return;
      const text = el.value;
      if (!text) return;
      const ws = state.ws.attach;
      // Keep the draft on any failure. `sendKbdBytes` silently returns when the socket is not
      // open, which is fine for one keystroke and would be a lost prompt here.
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        state.composeNote = "연결되지 않음 — 입력은 유지됩니다";
        renderComposeNote();
        return;
      }
      try {
        // Literally what typing would do: the text, then Enter. Newlines inside the text are
        // Enter presses at the remote too — a two-line paste submits twice. The socket has no
        // application ack, so a send that leaves here is QUEUED, not delivered; nothing is
        // replayed automatically, because replaying an ambiguous send could run a command twice.
        ws.send(new TextEncoder().encode(text + "\r"));
      } catch {
        state.composeNote = "전송 실패 — 입력은 유지됩니다";
        renderComposeNote();
        return;
      }
      el.value = "";
      autoGrow(el);
      state.composeNote = "";
      renderComposeNote();
      el.focus();
    }

    function renderComposeNote() {
      const wrap = document.getElementById("compose-wrap");
      if (!wrap) return;
      let note = wrap.querySelector(".compose-note");
      if (!state.composeNote) { note?.remove(); return; }
      if (!note) {
        note = document.createElement("div");
        note.className = "compose-note";
        wrap.appendChild(note);
      }
      note.textContent = state.composeNote;
    }

    /// Resize the draft box to its content, and tell the terminal when that moved it.
    ///
    /// The compose bar and the terminal share the column: growing the draft shrinks the
    /// terminal's box. Nothing else notices — the resize listeners watch the window and the
    /// visual viewport, neither of which fires for a textarea growing — so without this the
    /// grid keeps its old row count and its bottom lines sit outside the visible area.
    function autoGrow(el) {
      const before = el.style.height;
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 96)}px`;
      if (el.style.height !== before) sendMuxResize();
    }

    function wireCompose() {
      const el = composeEl();
      if (!el) return;
      el.addEventListener("compositionend", () => { lastCompositionEnd = Date.now(); });
      el.addEventListener("input", () => autoGrow(el));
      el.addEventListener("keydown", (e) => {
        if (!shouldSubmitOnEnter(e, Date.now(), lastCompositionEnd)) return;
        e.preventDefault();
        submitCompose();
      });
      document.getElementById("compose-send")?.addEventListener("click", (e) => {
        e.preventDefault();
        submitCompose();
      });
      // xterm grabs focus whenever the terminal body is touched, and once its textarea has it
      // the iOS keyboard belongs to xterm again and Korean stops composing. In compose mode the
      // terminal is for READING.
      //
      // CAPTURE phase AND `stopPropagation`, both load-bearing.
      //
      // Capture, because xterm's handler is on a DESCENDANT: a bubbling listener would run
      // after it had already focused the textarea, interrupting the IME session even though
      // focus is handed straight back. And `preventDefault` alone is not enough either —
      // xterm 5.5's mousedown handler calls `focus()` unconditionally and never looks at
      // `defaultPrevented` — so the event must not reach it at all.
      //
      // The cost, accepted: with the event stopped, xterm's own drag-selection does not run in
      // compose mode. Switch to 직접 to select terminal text with the pointer.
      //
      // Deliberately NOT touchstart: stopping that would kill scrolling the output, and iOS
      // synthesises the mousedown for a tap anyway.
      const host = document.getElementById("term-host");
      const keepFocus = (ev) => {
        if (state.rawInput) return;
        ev.stopPropagation();
        ev.preventDefault();
        el.focus();
      };
      host?.addEventListener("mousedown", keepFocus, true);
      host?.addEventListener("contextmenu", keepFocus, true);
      // Backstop for any focus path not enumerated above: if anything inside the terminal takes
      // focus while composing, bounce it straight back.
      host?.addEventListener("focusin", (ev) => {
        if (state.rawInput || ev.target === el) return;
        el.focus();
      }, true);
      renderComposeNote();
      focusForMode();
    }

    function leaveMux() {
      if (history.state && history.state.mode === "mux") {
        history.back();
      } else {
        teardownMux();
        render();
      }
    }

    // A WebSocket upgrade cannot tell the page WHY it failed — a refused attach looks exactly
    // like a dropped connection. So ask over HTTP first and show the real reason.
    // A WebSocket upgrade cannot tell the page WHY it failed — a refused attach looks exactly
    // like a dropped connection. So ask over HTTP first and show the real reason.
    async function muxPreflight() {
      try {
        const j = await api("/api/board/attach-preflight");
        return j.ok ? "" : (j.message || j.code || "comux is unavailable");
      } catch (e) {
        return `could not reach the bridge: ${e}`;
      }
    }

    // The ONE way into mux, used by the button and by history navigation alike. Entering
    // through popstate has to preflight too: otherwise Back-then-Forward with comux stopped
    // would skip straight to the socket and show a bare "[disconnected]", which is precisely
    // the diagnostic the preflight exists to replace.
    async function enterMux(pushHistory) {
      // Identity, not a boolean: a second entry (double tap, or Back-then-Forward while the
      // first request is still out) supersedes the earlier one, and only the CURRENT epoch's
      // response is allowed to land. A boolean guard could be left set by a teardown that
      // happened mid-flight, which silently swallowed the next entry.
      disposeMuxTerminal();
      const epoch = ++state.muxEpoch;
      state.muxError = "";
      state.muxChecking = true;
      state.mode = "mux";
      if (pushHistory) {
        // Same contract as attach: popstate is the single source of truth for terminal-mode
        // transitions, so the platform back gesture leaves the terminal, not the page.
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
      state.term = term;
      state.fit = fit;
      muxGrid(fit, term);
      // repairFontMetrics refits to the raw viewport and sends THAT to the PTY, which would
      // push the grid below the floor and leave xterm and the PTY disagreeing. Re-apply the
      // floor and send the corrected size instead.
      whenFontReady(term.options.fontSize, () => { repairFontMetrics({ sendResize: false }); sendMuxResize(); });
      // The symbol font lands independently and changes no metrics (xterm measures the FIRST
      // family), but the glyphs it carries only appear once it has. Nudge a repaint.
      whenFontReady(term.options.fontSize, () => term.refresh(0, term.rows - 1), "CopadSymbols");

      const proto = location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(`${proto}://${location.host}/ws/board/attach`, wsProtocols());
      ws.binaryType = "arraybuffer";
      state.ws.attach = ws;

      ws.onopen = () => sendMuxResize();
      ws.onmessage = (msg) => {
        if (msg.data instanceof ArrayBuffer) term.write(new Uint8Array(msg.data));
        else term.write(String(msg.data));
      };
      ws.onclose = () => {
        if (state.mode === "mux") term.writeln("\r\n\x1b[33m[disconnected]\x1b[0m");
      };
      ws.onerror = () => { try { ws.close(); } catch {} };

      term.onData((data) => {
        if (state.ctrlSticky && data.length === 1 && data >= " " && data <= "~") {
          data = String.fromCharCode(data.charCodeAt(0) & 0x1f);
          state.ctrlSticky = false;
          document.getElementById("ctrl")?.classList.remove("sticky");
        }
        if (ws.readyState === WebSocket.OPEN) ws.send(new TextEncoder().encode(data));
      });

      const onResize = () => {
        if (state.mode !== "mux") return;
        sendMuxResize();
      };
      window.addEventListener("resize", onResize);
      window.visualViewport?.addEventListener("resize", onResize);
      // Without this every visit leaves another live listener behind, and each one refits the
      // CURRENT terminal and sends its own duplicate resize.
      state.muxResizeOff = () => {
        window.removeEventListener("resize", onResize);
        window.visualViewport?.removeEventListener("resize", onResize);
      };
    }

    /* ==== xterm.js attach mode ==== */
    function openTerminal() {
      // Idempotent: re-renders triggered by unrelated state changes
      // (e.g. presence toggle in attach mode) must not spawn a fresh
      // WS / xterm while the previous one is still live. leaveAttach
      // is the only legitimate teardown.
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
      if (fit) fit.fit();
      state.term = term;
      state.fit = fit;

      // xterm measures the glyph cell ONCE at open(), so whenever the webfont lands
      // after that the grid keeps the fallback's cell size and every column sits
      // wrong. Deliberately NOT awaited before open(): `font-display: swap` already
      // paints the fallback immediately, so waiting would only buy a blank terminal
      // for the length of a ~2 MB cellular download — and an await here would open a
      // window between the idempotence guard above and `state.term` being set, in
      // which a re-render could spawn a second xterm + WS. Resolves instantly when
      // the font is already cached (the common case after the first load).
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

      // Resize on viewport change.
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

      // The on-screen keyboard does NOT fire window.resize on iOS — it shrinks the
      // *visual* viewport while the layout viewport (and 100vh/100dvh) stay put. So
      // without this the terminal keeps its full height behind the keyboard and the
      // PTY never learns its new size. visualViewport is the only signal that sees
      // the keyboard; `scroll` matters too because iOS pans the visual viewport to
      // keep the focused element visible.
      const vv = window.visualViewport;
      let vvRaf = 0;
      const onViewport = () => {
        // visualViewport fires a burst during the keyboard animation; coalesce to
        // one refit per frame so we don't spam PTY resizes mid-animation.
        if (vvRaf) return;
        vvRaf = requestAnimationFrame(() => {
          vvRaf = 0;
          setAppHeight();
          onResize();
        });
      };
      if (vv) {
        vv.addEventListener("resize", onViewport);
        // passive: the handler never preventDefaults, and a non-passive scroll
        // listener blocks the compositor during the keyboard animation.
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
      // unescape sequences typed in the data-bytes attribute
      let bytes = escape.replace(/\\x([0-9a-fA-F]{2})/g, (_, h) => String.fromCharCode(parseInt(h, 16)))
                        .replace(/\\t/g, "\t");
      // Sticky-Ctrl path: if toolbar Ctrl is armed and the next
      // toolbar tap is a single printable byte, fold it via the
      // 0x1f mask (Ctrl-A = 0x01, Ctrl-/ = 0x1f, etc.). Without
      // this `Ctrl` then `/` sends a literal slash — the intent's
      // sticky-modifier contract is broken on the toolbar path.
      if (state.ctrlSticky && bytes.length === 1 && bytes >= " " && bytes <= "~") {
        bytes = String.fromCharCode(bytes.charCodeAt(0) & 0x1f);
        state.ctrlSticky = false;
        const el = document.getElementById("ctrl");
        if (el) el.classList.remove("sticky");
      }
      ws.send(new TextEncoder().encode(bytes));
    }

    /* ==== overview WS ==== */

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
          // A pilot.* bus event means the queue changed — refresh the
          // cockpit immediately (loadPilot re-renders on completion).
          if ((ev.type || ev.kind || "").startsWith("pilot.")) loadPilot();
          if (state.mode === "overview") render();
        } catch {}
      };
      ws.onclose = () => { state.ws.events = null; if (canReconnect()) setTimeout(openEventStream, 2000); };
      ws.onerror = () => { try { ws.close(); } catch {} };
    }

    /* ==== push notifications ==== */
    async function initPush() {
      // iOS Safari pre-16.4 has no Push API. Android Chrome / FF /
      // Safari 16.4+ / desktop are fine. Plain HTTP (other than
      // localhost) is also blocked — service worker requires secure
      // context.
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
          // Load kinds preference from local storage (sticky across
          // reloads). Server is source of truth on next mutate.
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
          // Best-effort server prune by sha256(endpoint) id. The
          // browser-side unsubscribe is the auth-of-record though.
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
      // Re-subscribe with new kinds (server dedupes by sha256(endpoint)
      // id, so this is idempotent rather than creating a second row).
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

    /* ==== actions ==== */
    /// Run `cb` once the terminal webfont is actually usable. Never runs it if the
    /// font can't load (e.g. /font/terminal 404s because the workstation has no such
    /// font installed) — the fallback stack is then simply what you get, as before.
    function whenFontReady(px, cb, family = "CopadTerminal") {
      if (!document.fonts?.load) return;
      const spec = `${px || 13}px "${family}"`;
      document.fonts
        .load(spec)
        .then(() => {
          // load() resolves even when nothing matched; check() is what proves the
          // face is really there.
          if (document.fonts.check(spec)) cb();
        })
        .catch(() => {});
    }

    /// Re-measure after a webfont that landed AFTER term.open().
    ///
    /// Reassigning fontFamily is what forces xterm to drop its cached glyph metrics;
    /// without it the grid keeps the fallback's cell size and the columns stay
    /// misaligned even though the right glyphs are now available. Then refit and tell
    /// the PTY, since the cell size (and therefore rows/cols) just changed.
    function repairFontMetrics(opts = {}) {
      const { sendResize = true } = opts;
      const term = state.term;
      if (!term) return;
      try {
        // Must assign a DIFFERENT value: xterm's option setter short-circuits when the
        // value is unchanged, so `x = x` is a no-op and the cached glyph metrics stay.
        // Round-tripping through a distinct-but-equivalent stack is what actually
        // triggers the re-measure.
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

    /// Publish the VISUAL viewport height as --app-h.
    ///
    /// Every full-height view keys off this instead of 100vh/100dvh, because the
    /// layout viewport does not shrink for the on-screen keyboard — only the visual
    /// viewport does. Without it the phone keyboard covers the terminal (and the
    /// pilot composer) with no way to see what you're typing at.
    function setAppHeight() {
      const vv = window.visualViewport;
      const h = Math.round(vv ? vv.height : window.innerHeight);
      if (h > 0) document.documentElement.style.setProperty("--app-h", h + "px");
    }

    // Global measurement: the overview/pilot views raise the keyboard too (goal
    // composer). The attach view registers its own handler as well so it can refit
    // the terminal + resize the PTY; both call setAppHeight so neither depends on
    // listener registration order.
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
        // Called for its failure mode, not its payload: a `no_gui` rejection is how the page
        // learns the copad GUI is not running. The pane list itself is no longer rendered.
        await api("/api/panes");
      } catch (e) {
        if (e && e.code === "no_gui") state.noGui = true;
      }
      await initPush();
      render();
      openEventStream();
      // `/ws/tmux/overview` is deliberately NOT opened any more. It shells out to tmux
      // server-side every 5 s and pushes a snapshot that triggers a render — pure cost on a
      // machine whose multiplexer is comux, where every one of those snapshots is empty. The
      // endpoint itself is left in place for installs that still run tmux.
      startBoardPolling();
      // Pilot cockpit: initial load + a slow poll as a backstop (pilot.*
      // bus events on the event stream drive the responsive refresh; this
      // catches state the events miss and the first paint).
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
        // In attach mode, patch the chip in-place rather than calling
        // render() — full rerender would re-trigger openTerminal and,
        // even with its idempotent guard, would needlessly thrash the
        // xterm host element. Overview mode goes through render() as
        // normal so the chip + downstream UI stay consistent.
        // Both terminal views patch in place: a full render() tears out xterm's host element,
        // and openTerminal/openMuxTerminal then no-op on their idempotence guard because
        // `state.term` still exists — leaving a live socket attached to a terminal with no DOM.
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
      // Push a history entry so the platform back button (Android
      // gesture / browser ←) detaches instead of leaving the page.
      // popstate is the single source of truth for transitions —
      // both this push and the in-app `← overview` button route
      // through it, so the URL and `state.mode` never diverge.
      history.pushState({ mode: "attach", pane: paneId }, "", `#pane/${encodeURIComponent(paneId)}`);
      render();
    }

    // Tear down attach resources without touching history — used both
    // by user-initiated back and by popstate handler. Caller controls
    // whether to push/pop the history entry to avoid double-entries.
    function teardownAttach() {
      if (state.ws.attach) { try { state.ws.attach.close(); } catch {} state.ws.attach = null; }
      if (state.term) { try { state.term.dispose(); } catch {} state.term = null; }
      state.fit = null;
      if (state._removeResize) { state._removeResize(); state._removeResize = null; }
      state.activePane = null;
      state.mode = "overview";
      // The board's poll loop exits whenever the mode is not `overview`, and a `#pane/<id>`
      // deep link puts the page straight into attach at boot — so without this the fleet would
      // sit on "loading…" until something else happened to restart it.
      startBoardPolling();
    }

    function leaveAttach() {
      // Delegate to platform back so popstate fires + URL pops in
      // the same path as the OS back gesture. popstate handler
      // does the actual teardown + render.
      if (history.state && history.state.mode === "attach") {
        history.back();
      } else {
        teardownAttach();
        render();
      }
    }

    // Leave whichever terminal mode is live. Both own an xterm + socket, and `render()` drops
    // the host element, so switching between them without tearing down first leaves a blank
    // screen whose toolbar still types into the old socket.
    function teardownAnyTerminal() {
      if (state.mode === "attach") teardownAttach();
      else if (state.mode === "mux") teardownMux();
    }

    // Mobile browsers freeze timers in a backgrounded tab, so the cadence alone cannot keep the
    // board honest — refresh when the page comes back instead.
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible" && state.mode === "overview" && isAuthed()) {
        startBoardPolling();
      }
    });

    window.addEventListener("popstate", (e) => {
      const target = (e.state && e.state.mode) || "overview";
      if (target === "attach" && e.state.pane) {
        // Forward navigation back into an attach URL — only act if
        // we're not already attached to the same pane.
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

    // Boot path: honor `#pane/<id>` deep-links on first load so a
    // bookmarked attach URL goes straight to the terminal.
    function bootFromHash() {
      const m = location.hash.match(/^#pane\/(.+)$/);
      if (m) {
        const paneId = decodeURIComponent(m[1]);
        // Seed history with overview as the anchor so first back
        // press returns to overview rather than leaving the page.
        history.replaceState({ mode: "overview" }, "", location.pathname + location.search);
        history.pushState({ mode: "attach", pane: paneId }, "", `#pane/${encodeURIComponent(paneId)}`);
        state.activePane = paneId;
        state.mode = "attach";
      } else {
        history.replaceState({ mode: "overview" }, "", location.pathname + location.search + location.hash);
      }
    }

    // Bearer only: with a cached token, boot; without one, show the setup page. The
    // unauthenticated /api/whoami probe that used to precede this is gone with the identity
    // header path it existed to detect — it now always answers `bearer`, and it is behind the
    // auth middleware anyway, so an unauthenticated call can only 401.
    (() => {
      if (token) {
        bootFromHash(); bootstrap();
      } else {
        render();
      }
    })();
  })();
