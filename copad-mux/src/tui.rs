//! The mux [`App`]: the authoritative `State` (layout authority) + one `PaneTerm`
//! (real shell PTY) per terminal, plus the composition logic. It renders the whole
//! workspace into a `Buffer` ([`App::render_to`]) and interprets keys
//! ([`App::feed_key`]) — tmux-style prefix `Ctrl-b` then `%`/`"` split, `o`/arrows
//! focus, `x` close, `c`/`n`/`p`/`&` tabs, `d` detach; plus prefix-less
//! `Ctrl+Shift+arrow` nav and the `Ctrl-f` switcher.
//!
//! `App` is transport-agnostic: it owns no terminal I/O and no socket. The headless
//! [`crate::server`] drives it (render → ship cell diffs; forwarded keys → feed_key)
//! and the thin [`crate::client`] renders + forwards input, so the same App serves
//! local and detached/reattached sessions.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agentstate;
use crate::config::{self, Action, MuxConfig, SortBy, TabLabels, UsageLayout, UsageStyle};
use crate::control;
use crate::gitinfo;
use crate::model::{
    ClientId, Dir, LayoutSpec, PaneId, PaneRect, Rect, Role, SplitTree, TabId, TerminalId,
    WorkspaceId,
};
use crate::notify;
use crate::persist::{self, MAX_LEAVES_PER_TAB, MAX_TOTAL_PANES, PLayout};
use crate::procinfo;
use crate::proto::MouseKind;
use crate::state::{Command, Event, MuxError, Origin, RestoredTab, State};
use crate::term::{CellColor, PaneTerm};
use crate::usagepoll::{self, UsagePart, UsageSnapshot};
use crate::versionpoll::{self, VersionStatus};

/// Direction for focus navigation with the arrow keys.
#[derive(Clone, Copy)]
enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// One logged agent-turn event, shown in the notification center (`Ctrl-b a`).
#[derive(Debug, Clone)]
struct Notification {
    /// Wall-clock `HH:MM` when it happened.
    when: String,
    /// `"done"` (turn finished) or `"blocked"` (awaiting input).
    kind: &'static str,
    tool: String,
    space: String,
    body: &'static str,
    /// The pane it came from — `Enter` in the center jumps here.
    terminal: TerminalId,
}

/// Max notifications retained in the center.
const NOTIFY_LOG_CAP: usize = 100;

/// What a click on a piece of chrome should do. Recorded per rendered chip/row so
/// [`App::mouse_at`] can turn a click on the status bar or sidebar into navigation.
#[derive(Clone)]
enum ClickTarget {
    /// A status-bar tab chip → select that tab in the active session.
    Tab(TabId),
    /// A sidebar `spaces` row → switch to that session.
    Session(WorkspaceId),
    /// A sidebar `agents` row → jump to that agent's pane (its session + tab + focus).
    Agent(TerminalId),
    /// The status-bar session pill. A left click is a no-op (switching to the already
    /// active session would steal the controller lease); it exists as a RIGHT-click
    /// context-menu anchor for session actions.
    SessionPill(WorkspaceId),
    /// The status-bar usage/limits readout (`usage_layout = paged`). Wheel over it
    /// pages the carousel; a left click advances one page (wrapping).
    Usage,
}

/// A rectangular hit region (inclusive `x0..=x1`, `y0..=y1`) recorded during render.
struct ClickZone {
    x0: u16,
    x1: u16,
    y0: u16,
    y1: u16,
    target: ClickTarget,
}

/// An inline single-line text prompt (session name entry / rename), tmux `command-prompt`.
#[derive(Clone, PartialEq, Eq)]
enum PromptKind {
    /// `Ctrl-b C`: create a session with the typed name (empty → auto `sN`).
    NewSession,
    /// `Ctrl-b W`: create a git worktree for the typed branch + a session in it.
    NewWorktree,
    /// `Ctrl-b $`: rename this session (empty → revert to its id). Captures the TARGET
    /// workspace so a session switch while the prompt is open can't retarget it.
    RenameSession(WorkspaceId),
    /// `Ctrl-b ,` (tmux rename-window): rename the active tab (empty → revert to its
    /// process/index label). Captures the TARGET workspace + tab so a tab or session
    /// switch (this client or a concurrent one) while the prompt is up can't retarget
    /// or silently drop it.
    RenameTab(WorkspaceId, TabId),
}

/// One sidebar/switcher agent row: where the agent pane lives + what it runs.
struct AgentRow {
    /// Display title: the pane's TAB custom name when set (so multiple agents in one
    /// space stay distinguishable), else the session (space) name.
    title: String,
    /// The session (space) name — kept alongside `title` so the switcher can filter on
    /// it and show it even when a tab name overrides the title.
    space: String,
    /// The agent's foreground command (e.g. `claude` / `codex`).
    tool: String,
    /// Rolled-up status: `working` / `ready` / `blocked` / `idle`.
    status: &'static str,
    /// The pane's terminal, so the row is clickable (jump to its pane).
    term: TerminalId,
}

/// Outcome of [`App::create_worktree_session`]: a human status line and a non-fatal
/// warning (a post-create hook failure — the worktree was still created) surfaced
/// separately so BOTH the CLI response and the TUI toast report it.
struct WorktreeCreated {
    message: String,
    warning: Option<String>,
}

/// State of the open inline prompt: what it will do + the text typed so far.
struct Prompt {
    kind: PromptKind,
    buf: String,
}

/// What a selected context-menu item does. Targets are captured at MENU OPEN so a
/// tab/session switch (this client or a concurrent one) while the menu is up can't
/// retarget them; execution re-validates ids against current state.
#[derive(Clone)]
enum MenuAction {
    /// Open the rename-tab prompt for this tab.
    RenameTab(WorkspaceId, TabId),
    /// Close this tab (rejected on the last tab, like `Ctrl-b &`).
    CloseTab(WorkspaceId, TabId),
    /// Create a new tab in this session (a no-op if it is no longer the active one —
    /// `new_tab` spawns into + switches the ACTIVE session, so running it after a
    /// concurrent session switch would land the tab somewhere else).
    NewTab(WorkspaceId),
    /// Open the rename-session prompt for this session.
    RenameSession(WorkspaceId),
    /// Open the new-session name prompt.
    NewSession,
    /// Open the kill-session y/n confirm for this session.
    KillSession(WorkspaceId),
    /// Jump to this agent's pane (session + tab + focus).
    JumpAgent(TerminalId),
}

/// A tmux `display-menu`-style context menu, opened by a RIGHT-click on chrome (tab
/// chip / session pill / sidebar row). Two selection modes, matching tmux: hold the
/// right button and RELEASE over an item to run it; or release elsewhere first, then
/// left-click an item (or navigate with `j`/`k` + Enter, Esc/q closes). The box rect
/// is fixed at open (clamped to the frame); any layout change closes the menu.
struct Menu {
    /// Title shown in the top border (the clicked thing's display name).
    title: String,
    items: Vec<(&'static str, MenuAction)>,
    /// Box rect in frame cells (borders included).
    x0: u16,
    y0: u16,
    w: u16,
    h: u16,
    /// Hover/keyboard-selected item index (`None` = nothing highlighted).
    sel: Option<usize>,
    /// The client whose right-click opened it: only its right-drag/-release drive the
    /// hold-mode selection (mirrors drag-selection ownership).
    owner: ClientId,
    /// Right button still held since open — a release over an item executes it. After
    /// a release elsewhere the menu stays open in click/keyboard mode.
    held: bool,
}

impl Menu {
    /// The item index at frame cell `(x, y)`, if it lands on an item row.
    fn item_at(&self, x: u16, y: u16) -> Option<usize> {
        // Items occupy the interior rows (inside the border), full inner width.
        if x <= self.x0 || x >= self.x0 + self.w.saturating_sub(1) {
            return None;
        }
        let row = y.checked_sub(self.y0 + 1)? as usize;
        (row < self.items.len()).then_some(row)
    }
}

/// Place a context-menu box of `w`×`h` cells near the click at `(ax, ay)` inside a
/// `cols`×`rows` frame: prefer opening BELOW-RIGHT of the pointer, flip ABOVE when the
/// lower half has no room (the status bar case), and clamp into the frame either way.
fn menu_origin(ax: u16, ay: u16, w: u16, h: u16, cols: u16, rows: u16) -> (u16, u16) {
    let x0 = ax.min(cols.saturating_sub(w));
    // Below needs rows ay+1 .. ay+h; above needs ay-h .. ay-1.
    let y0 = if ay + 1 + h <= rows {
        ay + 1
    } else {
        ay.saturating_sub(h)
    };
    (x0, y0.min(rows.saturating_sub(h)))
}

/// A pending destructive action awaiting a `y`/`n` confirmation (tmux `confirm-before`).
/// The target is captured at open time so a focus/session change while the prompt is up
/// can't retarget it (codex review).
enum ConfirmAction {
    /// Kill (remove) this workspace/session.
    KillSession(WorkspaceId),
}

/// An open confirm modal: a message + the action to run on `y`.
struct Confirm {
    message: String,
    action: ConfirmAction,
}

/// A destination in the `Ctrl-f` fuzzy switcher.
#[derive(Clone)]
enum PopupTarget {
    /// Switch to this session (workspace).
    Session(WorkspaceId),
    /// Jump to this agent's pane (its session + tab + focus).
    Agent(TerminalId),
}

/// One rendered row of the `Ctrl-f` switcher (post-filter).
struct PopupRow {
    target: PopupTarget,
    glyph: &'static str,
    color: Color,
    /// The matchable + displayed text (`name  branch` / `tool · status  (space)`).
    text: String,
}

/// Which list the `Ctrl-f` switcher is showing (Left/Right switches).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PopupTab {
    Sessions,
    Agents,
}

/// The `Ctrl-f` fuzzy switcher state: which tab, the typed filter, the selected row.
struct PopupState {
    tab: PopupTab,
    filter: String,
    sel: usize,
}

/// Keyboard-focus state for the always-on left sidebar (`Ctrl-b e`): which group
/// (spaces/agents) + the selected row. Arrows navigate, Enter selects, Esc exits.
struct SidebarFocus {
    tab: PopupTab,
    sel: usize,
}

/// What feeding one key to the app implies for the caller (the server loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Keep the session going.
    Continue,
    /// The user pressed the detach chord (`Ctrl-b d` / `Ctrl-b q`): the client should
    /// leave, but the server + shells keep running.
    Detach,
    /// The user pressed the redraw chord (`Ctrl-b r`): force a full repaint to the client
    /// that asked (server re-sends a `full` frame; the client clears + repaints).
    Redraw,
}

/// Height of the always-on bottom status bar (tmux-style).
const STATUS_H: u16 = 1;

// Catppuccin Mocha — the owner's tmux status palette (`~/dotfiles/tmux/.tmux.conf`).
const CAT_BASE: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
const CAT_TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
const CAT_MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);
const CAT_SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
const CAT_BLUE: Color = Color::Rgb(0x89, 0xb4, 0xfa);
const CAT_YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
const CAT_SUBTEXT: Color = Color::Rgb(0xba, 0xc2, 0xde);
const CAT_OVERLAY: Color = Color::Rgb(0x6c, 0x70, 0x86);
const CAT_GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
const CAT_PEACH: Color = Color::Rgb(0xfa, 0xb3, 0x87);
const CAT_RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
/// Background tint for cells inside an in-progress drag-selection (Catppuccin surface2 —
/// clearly lighter than the base so the selection is visible without hiding the glyphs).
const SEL_BG: Color = Color::Rgb(0x58, 0x5b, 0x70);

/// Color a usage gauge by how full it is: calm green under 70%, yellow warning
/// from 70%, red alert from 90%.
fn usage_threshold_color(pct: f64) -> Color {
    if pct >= 90.0 {
        CAT_RED
    } else if pct >= 70.0 {
        CAT_YELLOW
    } else {
        CAT_GREEN
    }
}

/// An in-progress mouse drag-selection (tmux-style copy). Lives only between button-down and
/// button-up. Coordinates are PANE-RELATIVE viewport cells; `owner` is the client connection
/// that started it, so a concurrent client's Drag/Up can't hijack it (input is multi-client).
#[derive(Clone)]
struct Sel {
    /// The pane the drag started in — the selection is clamped to it, so the sidebar / status
    /// bar / other panes are never included.
    term: TerminalId,
    owner: ClientId,
    /// Where the drag started (pane-relative viewport cell `(col, row)`).
    anchor: (u16, u16),
    /// The current drag position (clamped to the pane).
    head: (u16, u16),
    /// True once the pointer actually moved — distinguishes a real selection from a plain
    /// click (which just focuses and copies nothing).
    moved: bool,
}

/// The multi-pane application: the authoritative layout `State` + a live shell per
/// terminal. The local TUI is the single controller client (`ClientId(0)`).
pub struct App {
    state: State,
    ws: WorkspaceId,
    client: ClientId,
    panes: HashMap<TerminalId, PaneTerm>,
    cols: u16,
    rows: u16,
    /// Neovim-style left panel toggle (`Ctrl-b s`). Honored only when the
    /// terminal is at least `SIDEBAR_MIN_COLS` wide.
    sidebar: bool,
    /// When `Some`, the `Ctrl-f` fuzzy switcher is open (filter + selected row); keys
    /// drive it instead of the shells. Switches SESSIONS + jumps to AGENTS.
    popup: Option<PopupState>,
    /// Keyboard scrollback mode (`Ctrl-b [`): `Some(term)` binds the mode to the
    /// terminal it was entered on, so a focus change (click / another client) can't
    /// strand that pane scrolled-up. Keys scroll it; exit bottoms it. Shared state.
    scroll_pane: Option<TerminalId>,
    /// The logged agent-turn notifications (newest first), shown in the center.
    notifications: VecDeque<Notification>,
    /// When `Some(sel)`, the notification center (`Ctrl-b a`) is open at row `sel`.
    center: Option<usize>,
    /// Env vars injected into every pane's shell (e.g. `COPAD_MUX_SOCK`), so a
    /// shell inside a pane can control its own mux via `copad-mux ctl`.
    sock_env: Vec<(String, String)>,
    /// The active client's live values for [`Self::env_update`] (tmux `update-environment`).
    /// Set from the initiating client before its input is dispatched, so a pane it spawns
    /// inherits that client's SSH/display session rather than the one the server was born in.
    /// Seeded at boot from the daemon's own (pre-scrub) values so the first pane is unchanged.
    client_env: Vec<(String, String)>,
    /// Variable names refreshed from the client (from `cfg.update_environment`); the filter
    /// applied to any incoming client env before it lands in [`Self::client_env`].
    env_update: Vec<String>,
    /// Per-terminal foreground-process label (agent / shell / command), refreshed
    /// on a throttled cadence (`refresh_labels`), read by the sidebar/popup/CLI.
    labels: HashMap<TerminalId, procinfo::Label>,
    /// When the labels were last refreshed (throttle `ps` to ~2 Hz).
    last_labels: std::time::Instant,
    /// Per-agent rolled-up status (working/ready/blocked/idle), refreshed at the
    /// label cadence from Claude's session file + a screen-text fallback (`agentstate`).
    agent_statuses: HashMap<TerminalId, agentstate::AgentStatus>,
    /// Per-session git branch (its focused pane's cwd) for the `spaces` subtitle,
    /// refreshed at the label cadence.
    branches: HashMap<WorkspaceId, String>,
    /// Monotonic counter for minting unique session (workspace) ids (`local` is the
    /// first, so new sessions start at `s1`).
    next_session: u64,
    /// Per-session last-activated tick (for `sort_by = recent`), bumped on switch.
    session_activity: HashMap<WorkspaceId, u64>,
    /// Monotonic tick source for `session_activity`.
    activity_clock: u64,
    /// When `Some`, an inline text prompt (new-session name / rename) is open and
    /// captures keystrokes until Enter (commit) or Esc (cancel).
    prompt: Option<Prompt>,
    /// When `Some`, a `y`/`n` confirm modal (e.g. kill-session) is open.
    confirm: Option<Confirm>,
    /// When `Some`, a right-click context menu (tmux `display-menu`) is open.
    menu: Option<Menu>,
    /// When `Some`, the sidebar has keyboard focus (`Ctrl-b e`) for in-place navigation.
    sidebar_focus: Option<SidebarFocus>,
    /// The effective user config (keybindings + options), loaded once at server start.
    cfg: MuxConfig,
    /// Clickable chrome regions (tab chips, sidebar rows), rebuilt every render so
    /// `mouse_at` can hit-test a click on the status bar / sidebar. Interior mutability
    /// because rendering is `&self`; only ever touched on the single main-loop thread.
    click_zones: RefCell<Vec<ClickZone>>,
    /// Shared usage/limits readout (`coctl usage --limits`), written by a background
    /// poller thread (`usagepoll`), read into `usage_shown` at the label cadence.
    usage_poll: usagepoll::Shared,
    /// Shared "update available" hint, written by the `versionpoll` thread
    /// (server-only); read into `version_shown` at the label cadence.
    version_poll: versionpoll::Shared,
    /// The update hint currently folded into the rendered status bar.
    version_shown: Option<VersionStatus>,
    /// The usage snapshot currently folded into the rendered status bar; compared to
    /// the shared handle so a change triggers a repaint through `maybe_refresh_labels`.
    usage_shown: Option<UsageSnapshot>,
    /// Which carousel page the usage readout shows (`usage_layout = paged`). Advanced
    /// by a wheel/click over the readout; clamped to the live page count at render.
    usage_page: usize,
    /// When the carousel last advanced — a manual page OR an auto-rotate tick resets
    /// it, so `usage_rotate_secs` counts from the last change, not a fixed cadence
    /// (a manual scroll never jumps again immediately after).
    usage_rolled_at: std::time::Instant,
    /// Last-seen alternate-screen bit per VISIBLE pane, polled every render by
    /// [`Self::take_alt_screen_exit`]. Watching the true→false edge lets the render loop
    /// force a full repaint when a full-screen app (nvim, less, …) exits, wiping the
    /// incremental-diff residue the alt→primary grid swap leaves behind.
    alt_screen: HashMap<TerminalId, bool>,
    /// The in-progress mouse drag-selection, if any (tmux-style copy). `None` except between
    /// a button-down and its button-up; the highlight only shows during the drag.
    selection: Option<Sel>,
}

impl App {
    pub fn new(
        cols: u16,
        rows: u16,
        sock_env: Vec<(String, String)>,
        client_env: Vec<(String, String)>,
        cfg: MuxConfig,
    ) -> io::Result<Self> {
        let mut state = State::new();
        let mut panes = HashMap::new();
        let client = ClientId(0);

        let env_update = cfg.update_environment.clone();
        // The boot seed (daemon's pre-scrub values) is filtered to the whitelist, then merged
        // over sock_env so the initial/restored panes inherit exactly the daemon's birth
        // environment — the daemon env itself is scrubbed by `server::run` before this call.
        let client_env = filter_env(client_env, &env_update);
        let boot_env = merge_env(&sock_env, &client_env);

        // Try to restore the saved session layout (continuum-style). Falls back to a
        // fresh single `local` workspace when persistence is off, there's no snapshot, or
        // nothing could be restored (never boot empty).
        let restored = if cfg.persist {
            persist::load().and_then(|snap| {
                restore_sessions(&mut state, &mut panes, &snap, cols, rows, &boot_env)
            })
        } else {
            None
        };
        let (ws, next_session) = match restored {
            Some(r) => r,
            None => {
                let ws = WorkspaceId::new("local");
                let (_tab, _pane, term0) =
                    state.create_workspace(ws.clone(), None, Rect { cols, rows });
                let Some(pt) =
                    PaneTerm::spawn_with_env(cols.max(1), rows.max(1), None, None, &boot_env)
                else {
                    return Err(io::Error::other("failed to spawn shell PTY"));
                };
                panes.insert(term0, pt);
                (ws, 1)
            }
        };
        let _ = state.apply(Command::Attach {
            client,
            workspace: ws.clone(),
            role: Role::Controller,
            takeover: false,
            cols,
            rows,
        });
        let mut app = Self {
            state,
            ws,
            client,
            panes,
            cols,
            rows,
            // Default from config (herdr-style always-on spaces + agents panel); still
            // size-adaptive (hidden below sidebar_min_cols) and toggleable via Ctrl-b s.
            sidebar: cfg.sidebar,
            popup: None,
            scroll_pane: None,
            notifications: VecDeque::new(),
            center: None,
            sock_env,
            client_env,
            env_update,
            labels: HashMap::new(),
            last_labels: std::time::Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(std::time::Instant::now),
            agent_statuses: HashMap::new(),
            branches: HashMap::new(),
            next_session,
            session_activity: HashMap::new(),
            activity_clock: 0,
            prompt: None,
            confirm: None,
            menu: None,
            sidebar_focus: None,
            cfg,
            click_zones: RefCell::new(Vec::new()),
            // Idle until `start_usage_poll` (called by the server); tests that build an
            // App without a server never spawn the poller thread.
            usage_poll: usagepoll::idle(),
            version_poll: versionpoll::idle(),
            version_shown: None,
            usage_shown: None,
            usage_page: 0,
            usage_rolled_at: std::time::Instant::now(),
            alt_screen: HashMap::new(),
            selection: None,
        };
        app.reflow();
        Ok(app)
    }

    /// The environment injected into a NEWLY spawned pane: the socket markers plus the
    /// active client's refreshed session vars (tmux `update-environment`). The daemon's
    /// own env was scrubbed of these names at startup, so this is the sole source of a
    /// pane's SSH/display session.
    fn pane_env(&self) -> Vec<(String, String)> {
        merge_env(&self.sock_env, &self.client_env)
    }

    /// Adopt an attaching/initiating client's environment as the source for future panes,
    /// filtered to the configured whitelist. Called before dispatching that client's input.
    pub fn set_client_env(&mut self, env: Vec<(String, String)>) {
        self.client_env = filter_env(env, &self.env_update);
    }

    // --- reads off the authoritative state ---

    /// Is the sidebar actually drawn? Toggled on AND wide enough (size-adaptive:
    /// narrow / mobile widths fall back to the popup).
    fn sidebar_visible(&self) -> bool {
        self.sidebar && self.cols >= self.cfg.sidebar_min_cols
    }

    /// Sidebar width in columns (configurable; a 1-col border is added to its right).
    fn sidebar_w(&self) -> u16 {
        self.cfg.sidebar_width
    }

    /// Left x-offset of the pane grid (the sidebar reserves `sidebar_w` + 1 border).
    fn content_x(&self) -> u16 {
        if self.sidebar_visible() {
            self.sidebar_w() + 1
        } else {
            0
        }
    }

    /// Width available to the pane grid, right of the sidebar.
    fn content_cols(&self) -> u16 {
        if self.sidebar_visible() {
            self.cols.saturating_sub(self.sidebar_w() + 1)
        } else {
            self.cols
        }
    }

    // --- tabs ---

    fn tab_count(&self) -> usize {
        self.state
            .workspace(&self.ws)
            .map(|w| w.tabs.len())
            .unwrap_or(0)
    }

    fn tab_ids(&self) -> Vec<TabId> {
        self.state
            .workspace(&self.ws)
            .map(|w| w.tabs.iter().map(|t| t.id.clone()).collect())
            .unwrap_or_default()
    }

    fn active_tab_id(&self) -> Option<TabId> {
        self.state.workspace(&self.ws).map(|w| w.active_tab.clone())
    }

    // --- sessions (workspaces) ---

    /// Session ids in CREATION order (stable; used by the `ctl` API for stable indices).
    fn session_ids(&self) -> Vec<WorkspaceId> {
        self.state.workspace_ids()
    }

    /// A session's display name (its `name`, else its id).
    fn session_display_name(&self, wid: &WorkspaceId) -> String {
        self.state
            .workspace(wid)
            .and_then(|w| w.name.clone())
            .unwrap_or_else(|| wid.to_string())
    }

    /// Session ids in the configured DISPLAY order (`sort_by`) — for the sidebar, the
    /// `Ctrl-f` switcher, and `)`/`(` cycling. A stable sort keeps creation order on ties.
    fn sorted_session_ids(&self) -> Vec<WorkspaceId> {
        let mut ids = self.session_ids(); // creation order
        match self.cfg.sort_by {
            SortBy::Created => {}
            SortBy::Alphabetical => {
                ids.sort_by_key(|w| self.session_display_name(w).to_lowercase())
            }
            SortBy::Recent => {
                // Most-recently-switched-to first (0 = never activated → last).
                ids.sort_by(|a, b| {
                    self.session_activity
                        .get(b)
                        .copied()
                        .unwrap_or(0)
                        .cmp(&self.session_activity.get(a).copied().unwrap_or(0))
                });
            }
            SortBy::Activity => {
                // Sessions with an active agent (working/blocked) first.
                ids.sort_by_key(|w| u8::from(!self.session_has_active_agent(w)));
            }
        }
        ids
    }

    /// Does session `wid` have an agent that is working or blocked (not idle/ready)?
    fn session_has_active_agent(&self, wid: &WorkspaceId) -> bool {
        let Some(w) = self.state.workspace(wid) else {
            return false;
        };
        w.tabs.iter().any(|t| {
            t.layout.panes().iter().any(|p| {
                t.layout
                    .terminal_of(p)
                    .and_then(|tid| self.agent_statuses.get(tid))
                    .map(|s| {
                        matches!(
                            s,
                            agentstate::AgentStatus::Working | agentstate::AgentStatus::Blocked
                        )
                    })
                    .unwrap_or(false)
            })
        })
    }

    fn session_count(&self) -> usize {
        self.state.workspace_count()
    }

    /// The active session's index within the DISPLAY (sorted) order — for sidebar
    /// windowing centered on the active session.
    fn active_session_index(&self) -> usize {
        self.sorted_session_ids()
            .iter()
            .position(|w| w == &self.ws)
            .unwrap_or(0)
    }

    /// Does session `wid` host any classified AI agent (across all its tabs)?
    fn session_has_agent(&self, wid: &WorkspaceId) -> bool {
        let Some(w) = self.state.workspace(wid) else {
            return false;
        };
        w.tabs.iter().any(|t| {
            t.layout.panes().iter().any(|p| {
                t.layout
                    .terminal_of(p)
                    .and_then(|tid| self.labels.get(tid))
                    .map(|l| l.kind == procinfo::Kind::Agent)
                    .unwrap_or(false)
            })
        })
    }

    /// Top y-offset of the pane grid. Tabs live in the bottom status bar now, so
    /// there is no top bar — the grid starts at row 0.
    fn content_y(&self) -> u16 {
        0
    }

    /// Height available to the pane grid, above the always-on bottom status bar.
    fn content_rows(&self) -> u16 {
        self.rows.saturating_sub(STATUS_H)
    }

    /// Placed rects for the active tab, tiling the content area (right of the
    /// sidebar when it is visible). Rects are already offset by `content_x`.
    fn layout(&self) -> Vec<PaneRect> {
        let mut out = Vec::new();
        if let Some(w) = self.state.workspace(&self.ws)
            && let Some(tab) = w.tab(&w.active_tab)
        {
            tab.layout.derive_layout(
                self.content_x(),
                self.content_y(),
                self.content_cols(),
                self.content_rows(),
                &mut out,
            );
        }
        out
    }

    fn focused_pane(&self) -> Option<PaneId> {
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        Some(tab.focused.clone())
    }

    fn focused_terminal(&self) -> Option<TerminalId> {
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        tab.layout.terminal_of(&tab.focused).cloned()
    }

    fn pane_order(&self) -> Vec<PaneId> {
        self.state
            .workspace(&self.ws)
            .and_then(|w| w.tab(&w.active_tab))
            .map(|t| t.layout.panes())
            .unwrap_or_default()
    }

    fn pane_of_terminal(&self, term: &TerminalId) -> Option<PaneId> {
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        tab.layout
            .panes()
            .into_iter()
            .find(|p| tab.layout.terminal_of(p) == Some(term))
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Resize every hosted PTY to match its on-screen rect, so shell output wraps
    /// at the right width (the layout is the source of truth for geometry). Also the
    /// central hook where any LAYOUT change (split / pane-resize / close / tab / session)
    /// cancels an in-progress drag-selection — its pane-relative coords are only valid for
    /// the layout at button-down, so applying them to a resized snapshot would copy the
    /// wrong range.
    fn sync_sizes(&mut self) {
        self.selection = None;
        // Any layout/viewport change invalidates the menu's fixed box rect too.
        self.menu = None;
        for rect in self.layout() {
            if let Some(pt) = self.panes.get(&rect.terminal) {
                pt.resize(rect.cols, rect.rows);
            }
        }
    }

    /// Apply any `Resized` geometry carried in `events` directly to the matching
    /// PaneTerms. Needed for BACKGROUND sessions/tabs: `sync_sizes`/`reflow` only
    /// touch the ACTIVE session's layout, but a reap in a background session
    /// recomputes ITS surviving terminals (G3) — those PTYs must still be SIGWINCH'd.
    fn apply_resized_events(&self, events: &[Event]) {
        for e in events {
            if let Event::Resized { terminals, .. } = e {
                for tg in terminals {
                    if let Some(pt) = self.panes.get(&tg.id) {
                        pt.resize(tg.cols, tg.rows);
                    }
                }
            }
        }
    }

    /// Push the CONTENT-area size (terminal minus sidebar + tab-bar chrome) to the
    /// authoritative `State` as the workspace viewport, then resize every PTY.
    /// Keeping the viewport equal to the real pane area means the authoritative
    /// geometry — and the `Resized` events a remote client will rely on — matches
    /// the actual PTY sizes (G2), even as chrome toggles. Call this on any change
    /// that alters the chrome footprint (terminal resize, sidebar toggle, tab
    /// add/remove); a pure in-tab layout change (split/close/focus) can use
    /// `sync_sizes` since the viewport is unchanged.
    fn reflow(&mut self) {
        // Any chrome/layout change invalidates a drag-selection's pane-relative coords.
        self.selection = None;
        let cols = self.content_cols().max(1);
        let rows = self.content_rows().max(1);
        let _ = self.state.apply(Command::Resize {
            client: self.client,
            cols,
            rows,
        });
        self.sync_sizes();
    }

    // --- mutations (through the authoritative actor) ---

    fn split(&mut self, dir: Dir) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        // Inherit the split source pane's cwd (tmux `-c '#{pane_current_path}'`).
        let cwd = self.focused_cwd();
        let events = match self.state.apply(Command::SplitPane {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            pane,
            dir,
            if_rev: None,
        }) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut new_term = None;
        let mut new_pane = None;
        for e in &events {
            if let Event::PaneSplit {
                new_terminal,
                new_pane: np,
                ..
            } = e
            {
                new_term = Some(new_terminal.clone());
                new_pane = Some(np.clone());
            }
        }
        if let (Some(nt), Some(np)) = (new_term, new_pane) {
            let pane_env = self.pane_env();
            match PaneTerm::spawn_with_env(self.cols.max(1), self.rows.max(1), None, cwd, &pane_env)
            {
                Some(pt) => {
                    self.panes.insert(nt, pt);
                    // tmux-style: focus the freshly created pane.
                    let _ = self.state.apply(Command::FocusPane {
                        client: self.client,
                        pane: np,
                    });
                }
                None => {
                    // PTY spawn failed — roll the split back so authoritative state
                    // never holds a pane without a matching PaneTerm (which would
                    // render blank + silently drop input). Closing `np` collapses the
                    // branch and refocuses the original pane.
                    let _ = self.state.apply(Command::ClosePane {
                        origin: Origin::Client(self.client),
                        workspace: self.ws.clone(),
                        pane: np,
                        if_rev: None,
                    });
                }
            }
        }
        self.sync_sizes();
    }

    /// Grow/shrink the focused pane along an axis (`Right` = width, `Down` = height)
    /// by nudging its split divider.
    fn resize_focused(&mut self, axis: Dir, grow: bool) {
        let Some(pane) = self.focused_pane() else {
            return;
        };
        if self
            .state
            .apply(Command::ResizePane {
                origin: Origin::Client(self.client),
                workspace: self.ws.clone(),
                pane,
                axis,
                grow,
            })
            .is_ok()
        {
            self.sync_sizes();
        }
    }

    fn close_focused(&mut self) {
        if let Some(pane) = self.focused_pane() {
            // The `x` key ignores a rejected close (e.g. the last pane stays;
            // the shell-exit path handles quitting).
            let _ = self.close_pane(pane);
        }
    }

    /// Close a specific pane (used by the `x` key on the focused pane and by the
    /// control API's `close <index>`).
    fn close_pane(&mut self, pane: PaneId) -> Result<(), MuxError> {
        // Propagate the actor's verdict (e.g. `CannotCloseLastPane`) so the
        // control API reports a real failure instead of a false `ok`.
        let events = self.state.apply(Command::ClosePane {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            pane,
            if_rev: None,
        })?;
        for e in &events {
            if let Event::PaneClosed { terminal, .. } = e {
                self.panes.remove(terminal);
            }
        }
        self.sync_sizes();
        Ok(())
    }

    /// Create a new tab (a fresh single-pane layout) and switch to it, spawning its
    /// shell. Rolls the tab back if the PTY can't spawn (never leave a blank tab).
    fn new_tab(&mut self) {
        // Inherit the current pane's cwd into the new tab's shell (tmux-style).
        let cwd = self.focused_cwd();
        let events = match self.state.apply(Command::NewTab {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
        }) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut created: Option<(TabId, TerminalId)> = None;
        for e in &events {
            if let Event::TabCreated { tab, terminal, .. } = e {
                created = Some((tab.clone(), terminal.clone()));
            }
        }
        if let Some((tab_id, term)) = created {
            let pane_env = self.pane_env();
            match PaneTerm::spawn_with_env(self.cols.max(1), self.rows.max(1), None, cwd, &pane_env)
            {
                Some(pt) => {
                    self.panes.insert(term, pt);
                }
                None => {
                    // Spawn failed — close the just-created tab so state never holds a
                    // tab whose pane has no live terminal (blank render + dropped input).
                    let _ = self.state.apply(Command::CloseTab {
                        origin: Origin::Client(self.client),
                        workspace: self.ws.clone(),
                        tab: tab_id,
                    });
                }
            }
        }
        // A new tab may make the tab bar appear (1→2 tabs), shrinking the content
        // height → re-derive the viewport, not just PTY sizes.
        self.reflow();
    }

    /// Switch the active tab by `delta` (wrapping): `+1` next, `-1` previous.
    fn cycle_tab(&mut self, delta: i32) {
        let ids = self.tab_ids();
        if ids.len() < 2 {
            return;
        }
        let Some(active) = self.active_tab_id() else {
            return;
        };
        let cur = ids.iter().position(|t| t == &active).unwrap_or(0);
        let next = ids[(cur as i32 + delta).rem_euclid(ids.len() as i32) as usize].clone();
        let _ = self.state.apply(Command::SelectTab {
            origin: Origin::Client(self.client),
            workspace: self.ws.clone(),
            tab: next,
        });
        self.sync_sizes();
    }

    /// Switch to the tab at 0-based `index` (the tab bar shows 1-based labels, so
    /// `Ctrl-b 1` → index 0). Out-of-range is ignored.
    fn select_tab_index(&mut self, index: usize) {
        if let Some(tab) = self.tab_ids().get(index).cloned() {
            self.selection = None; // switching tabs invalidates a drag-selection
            let _ = self.state.apply(Command::SelectTab {
                origin: Origin::Client(self.client),
                workspace: self.ws.clone(),
                tab,
            });
            self.sync_sizes();
        }
    }

    /// Close the active tab (and reap its shells). Rejected when it is the last tab.
    fn close_active_tab(&mut self) {
        let Some(active) = self.active_tab_id() else {
            return;
        };
        self.close_tab(&self.ws.clone(), active);
    }

    /// Close a specific tab of `ws` (context menu can target a non-active one) and reap
    /// its shells. Rejected when it is the workspace's last tab.
    fn close_tab(&mut self, ws: &WorkspaceId, tab: TabId) {
        let events = match self.state.apply(Command::CloseTab {
            origin: Origin::Client(self.client),
            workspace: ws.clone(),
            tab,
        }) {
            Ok(e) => e,
            Err(_) => return, // e.g. last tab — keep it
        };
        for e in &events {
            if let Event::TabClosed { terminals, .. } = e {
                for t in terminals {
                    self.panes.remove(t);
                }
            }
        }
        // Closing a tab may hide the tab bar (2→1 tabs), growing the content
        // height → re-derive the viewport.
        self.reflow();
    }

    /// Make `wid` the active session, re-attaching the local client as its controller
    /// (takeover releases the previous session's lease → it freezes, G3; shells keep
    /// running in the background). Reflows the new session to the current size.
    fn switch_session(&mut self, wid: WorkspaceId) {
        if self.ws == wid || self.state.workspace(&wid).is_none() {
            return;
        }
        // Record recency for `sort_by = recent`.
        self.activity_clock += 1;
        self.session_activity
            .insert(wid.clone(), self.activity_clock);
        self.ws = wid.clone();
        let cols = self.content_cols().max(1);
        let rows = self.content_rows().max(1);
        let _ = self.state.apply(Command::Attach {
            client: self.client,
            workspace: wid,
            role: Role::Controller,
            takeover: true,
            cols,
            rows,
        });
        self.sync_sizes();
    }

    /// The working directory of the focused pane's shell, for inheriting into a new
    /// session/tab/split (tmux `-c '#{pane_current_path}'`). `None` if unreadable.
    fn focused_cwd(&self) -> Option<PathBuf> {
        self.focused_terminal()
            .and_then(|t| self.panes.get(&t))
            .and_then(|p| p.pid())
            .and_then(procinfo::process_cwd)
    }

    /// Create a new session (a fresh single-pane workspace) and switch to it. `name`
    /// is the tmux-style session name (`None` → shown by its `sN` id); `cwd` is the
    /// directory to start its shell in (`None` → the server's cwd). Rolls the session
    /// back if the PTY can't spawn (never leave a blank session).
    fn new_session(&mut self, name: Option<String>, cwd: Option<PathBuf>) {
        let n = self.next_session;
        self.next_session += 1;
        let id = WorkspaceId::new(format!("s{n}"));
        let cols = self.content_cols().max(1);
        let rows = self.content_rows().max(1);
        let (_tab, _pane, term0) =
            self.state
                .create_workspace(id.clone(), name, Rect { cols, rows });
        let pane_env = self.pane_env();
        match PaneTerm::spawn_with_env(self.cols.max(1), self.rows.max(1), None, cwd, &pane_env) {
            Some(pt) => {
                self.panes.insert(term0, pt);
                self.switch_session(id);
            }
            None => {
                // Spawn failed — drop the just-created session so state never holds a
                // session whose pane has no live terminal.
                if let Some(terms) = self.state.remove_workspace(&id) {
                    for t in &terms {
                        self.panes.remove(t);
                    }
                }
            }
        }
    }

    /// Set (or clear, with `None`) a session's display name.
    fn rename_session(&mut self, ws: &WorkspaceId, name: Option<String>) {
        self.state.set_workspace_name(ws, name);
    }

    /// Set (or clear, with `None`) a tab's custom display name. The name takes display
    /// precedence over the tab's foreground-process label (chips, sidebar agents, Ctrl-f).
    /// Takes the owning workspace explicitly so a prompt committed after a session
    /// switch still renames the tab it was opened on.
    fn rename_tab(&mut self, ws: &WorkspaceId, tab: &TabId, name: Option<String>) {
        self.state.set_tab_name(ws, tab, name);
    }

    /// Sessions with at least one pane whose cwd is inside `path` (the worktree). Used
    /// for `worktree`-session reuse and removal safety. Each pane is tested by its live
    /// `process_cwd`, falling back to its recorded `spawn_cwd` so a momentary read
    /// failure on a worktree session still counts it as live (fail-safe for removal).
    fn sessions_in_path(&self, path: &std::path::Path) -> Vec<WorkspaceId> {
        let target = crate::worktree::canonical_or_lexical(path);
        let mut hits = Vec::new();
        for wid in self.session_ids() {
            let Some(w) = self.state.workspace(&wid) else {
                continue;
            };
            let inside = w.tabs.iter().any(|t| {
                t.layout.panes().iter().any(|p| {
                    let Some(tid) = t.layout.terminal_of(p) else {
                        return false;
                    };
                    let Some(pane) = self.panes.get(tid) else {
                        return false;
                    };
                    let cwd = pane
                        .pid()
                        .and_then(procinfo::process_cwd)
                        .or_else(|| pane.spawn_cwd().cloned());
                    cwd.map(|c| crate::worktree::canonical_or_lexical(&c).starts_with(&target))
                        .unwrap_or(false)
                })
            });
            if inside {
                hits.push(wid);
            }
        }
        hits
    }

    /// Create a git worktree for `branch` (sibling of the repo's MAIN worktree) and open
    /// a comux session in it, switching to it — the `comux worktree create` core, shared
    /// by the control API and the `Ctrl-b W` prompt. Returns `(worktree_path, status)`.
    ///
    /// Identity is anchored on git, never the bare computed path (naming is
    /// non-injective): if the target path is already a registered worktree for the SAME
    /// branch we reuse/recover it; a different branch or a non-worktree directory there
    /// is an error. The `git worktree add` is the sole durable side effect — a failing
    /// post-create hook or session spawn is reported, never rolled back.
    fn create_worktree_session(
        &mut self,
        branch: &str,
        from: &str,
        start: Option<PathBuf>,
    ) -> Result<WorktreeCreated, String> {
        // Normalize the input branch the same way parsed porcelain branches are (short
        // form), so naming, identity comparison, and `git worktree add -b` all agree even
        // if a caller passes a fully-qualified `refs/heads/…` ref.
        let branch = branch.trim();
        let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        if branch.is_empty() {
            return Err("branch name is required".into());
        }
        let start = start
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| "could not resolve a starting directory".to_string())?;
        let repo_root = crate::worktree::resolve_repo_root(&start)?;
        let entries = crate::worktree::list_entries(&repo_root)?;
        let planned = crate::worktree::plan_path(&entries, &self.cfg.worktree.naming, branch)?;
        let wt = planned.worktree_path.clone();
        let wt_key = crate::worktree::canonical_or_lexical(&wt);

        if let Some(e) = entries
            .iter()
            .find(|e| crate::worktree::canonical_or_lexical(&e.path) == wt_key)
        {
            // A registered worktree already occupies the target path.
            if e.branch.as_deref() != Some(branch) {
                return Err(format!(
                    "{} already holds branch '{}' — pick another branch name",
                    wt.display(),
                    e.branch.as_deref().unwrap_or("(detached)")
                ));
            }
            if !wt.exists() {
                return Err(format!(
                    "worktree {} is registered but missing on disk — \
                     run `git worktree prune` first",
                    wt.display()
                ));
            }
            // Recover: reuse a live session inside it, else open a fresh one.
            if let Some(wid) = self.sessions_in_path(&wt).into_iter().next() {
                self.switch_session(wid);
                return Ok(WorktreeCreated {
                    message: format!("switched to session for {}", planned.dir_name),
                    warning: None,
                });
            }
            self.spawn_worktree_session(&planned)?;
            return Ok(WorktreeCreated {
                message: format!("opened session in worktree {}", planned.dir_name),
                warning: None,
            });
        }
        if wt.exists() {
            return Err(format!(
                "{} already exists but is not a git worktree — \
                 remove it or pick another branch",
                wt.display()
            ));
        }

        // Fresh worktree: `git worktree add` is the sole durable side effect.
        crate::worktree::add(&repo_root, &wt, branch, from)?;
        let script_err = self
            .cfg
            .worktree
            .script_for(&planned.main_root)
            .and_then(|s| crate::worktree::run_hook(s, &wt));

        self.spawn_worktree_session(&planned)?;
        Ok(WorktreeCreated {
            message: format!("created worktree {}", wt.display()),
            warning: script_err,
        })
    }

    /// Open a session named after the worktree dir, in the worktree. Detects a shell
    /// spawn failure via the session-count delta and reports it (the worktree stays).
    fn spawn_worktree_session(&mut self, planned: &crate::worktree::Planned) -> Result<(), String> {
        let before = self.session_count();
        self.new_session(
            Some(planned.dir_name.clone()),
            Some(planned.worktree_path.clone()),
        );
        if self.session_count() > before {
            Ok(())
        } else {
            Err(format!(
                "worktree {} created, but its session failed to spawn — \
                 rerun `comux worktree create` to open it",
                planned.worktree_path.display()
            ))
        }
    }

    /// Open the kill-session confirm modal (`Ctrl-b X`, tmux `prefix X`) for the
    /// active session.
    fn open_kill_session_confirm(&mut self) {
        self.open_kill_session_confirm_for(self.ws.clone());
    }

    /// Open the kill-session confirm for a specific session (context menu can target a
    /// non-active one). No-op when only one session exists (a mux always keeps ≥1).
    /// Captures the TARGET workspace so a focus/session change while the prompt is up
    /// can't retarget it.
    fn open_kill_session_confirm_for(&mut self, ws: WorkspaceId) {
        if self.state.workspace_count() <= 1 {
            return;
        }
        let name = self
            .state
            .workspace(&ws)
            .and_then(|w| w.name.clone())
            .unwrap_or_else(|| ws.to_string());
        self.confirm = Some(Confirm {
            message: format!("kill session '{name}'? (y/n)"),
            action: ConfirmAction::KillSession(ws),
        });
    }

    fn perform_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::KillSession(wid) => self.kill_session(&wid),
        }
    }

    /// Remove a session (workspace), reap its PTYs, and switch to a survivor if it was
    /// active. Revalidates the target still exists and isn't the last session (it may
    /// have changed between opening the confirm and pressing `y`).
    fn kill_session(&mut self, wid: &WorkspaceId) {
        if self.state.workspace(wid).is_none() || self.state.workspace_count() <= 1 {
            return;
        }
        let killing_active = &self.ws == wid;
        if let Some(terms) = self.state.remove_workspace(wid) {
            for t in &terms {
                self.panes.remove(t);
            }
        }
        if killing_active && let Some(surv) = self.session_ids().into_iter().next() {
            // `self.ws` still names the removed session, so `switch_session` proceeds.
            self.switch_session(surv);
        }
    }

    /// Open the inline new-session name prompt (`Ctrl-b C`); Enter commits, Esc cancels.
    fn open_new_session_prompt(&mut self) {
        self.prompt = Some(Prompt {
            kind: PromptKind::NewSession,
            buf: String::new(),
        });
    }

    /// Open the inline new-worktree prompt (`Ctrl-b W`): the typed text is the branch
    /// for `worktree create`. Enter commits, Esc cancels.
    fn open_new_worktree_prompt(&mut self) {
        self.prompt = Some(Prompt {
            kind: PromptKind::NewWorktree,
            buf: String::new(),
        });
    }

    /// Open the inline rename prompt (`Ctrl-b $`) for the active session.
    fn open_rename_prompt(&mut self) {
        self.open_rename_prompt_for(self.ws.clone());
    }

    /// Open the inline rename prompt for a specific session (context menu can target a
    /// non-active one), seeded with its current name.
    fn open_rename_prompt_for(&mut self, ws: WorkspaceId) {
        let seed = self
            .state
            .workspace(&ws)
            .and_then(|w| w.name.clone())
            .unwrap_or_default();
        self.prompt = Some(Prompt {
            kind: PromptKind::RenameSession(ws),
            buf: seed,
        });
    }

    /// Open the inline rename-tab prompt (`Ctrl-b ,`) for the active tab.
    fn open_rename_tab_prompt(&mut self) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        self.open_rename_tab_prompt_for(self.ws.clone(), tab);
    }

    /// Open the inline rename-tab prompt for a specific tab (context menu can target a
    /// non-active one), seeded with its current custom name.
    fn open_rename_tab_prompt_for(&mut self, ws: WorkspaceId, tab: TabId) {
        let seed = self
            .state
            .workspace(&ws)
            .and_then(|w| w.tab(&tab))
            .and_then(|t| t.name.clone())
            .unwrap_or_default();
        self.prompt = Some(Prompt {
            kind: PromptKind::RenameTab(ws, tab),
            buf: seed,
        });
    }

    /// Apply the open prompt on Enter: create (name → `None` when blank) or rename.
    fn commit_prompt(&mut self, p: Prompt) {
        let trimmed = p.buf.trim();
        let name = (!trimmed.is_empty()).then(|| trimmed.to_string());
        match p.kind {
            // A session created from the TUI inherits the focused pane's cwd (tmux-style).
            PromptKind::NewSession => self.new_session(name, self.focused_cwd()),
            PromptKind::NewWorktree => {
                let Some(branch) = name else { return };
                // The worktree is resolved from the focused pane's repo (tmux-style cwd).
                // The CLI returns errors/warnings directly; the TUI has no inline message
                // line, so a failure OR a non-fatal hook warning is surfaced as a toast.
                // This is direct feedback for a key the user just pressed, so — unlike the
                // routine agent-turn toasts — it is NOT gated by the `notify` config (else
                // a `notify=false` user would get NO signal that their action failed).
                // `COPAD_MUX_NOTIFY=0` still hard-disables all toasts. Success needs no
                // note — the session visibly switched.
                let note = match self.create_worktree_session(&branch, "", self.focused_cwd()) {
                    Ok(c) => c.warning,
                    Err(e) => Some(e),
                };
                if let Some(msg) = note
                    && notify::env_override().unwrap_or(true)
                {
                    notify::desktop("comux worktree", &msg);
                }
            }
            PromptKind::RenameSession(ws) => self.rename_session(&ws, name),
            PromptKind::RenameTab(ws, tab) => self.rename_tab(&ws, &tab, name),
        }
    }

    /// Switch to the next/previous session (wrapping): `+1` next, `-1` previous.
    fn cycle_session(&mut self, delta: i32) {
        // Cycle in STABLE creation order (not the display sort): `sort_by = recent` mutates
        // the sorted order on every switch, so cycling by it would ping-pong between two
        // sessions and never reach the rest. Creation order always visits every session.
        let ids = self.session_ids();
        if ids.len() < 2 {
            return;
        }
        let cur = ids.iter().position(|w| w == &self.ws).unwrap_or(0);
        let next = ids[(cur as i32 + delta).rem_euclid(ids.len() as i32) as usize].clone();
        self.switch_session(next);
    }

    /// Jump to an agent pane that is BLOCKED (awaiting input) — switch to its session
    /// and tab and focus it, cycling to the next blocked agent after the current focus
    /// (the retired tmux `prefix+a` jump-to-attention).
    fn jump_to_attention(&mut self) {
        let mut blocked: Vec<TerminalId> = self
            .agent_statuses
            .iter()
            .filter(|(_, s)| **s == agentstate::AgentStatus::Blocked)
            .map(|(tid, _)| tid.clone())
            .collect();
        if blocked.is_empty() {
            return;
        }
        blocked.sort_by_key(|t| t.to_string()); // stable order
        // Cycle: the blocked agent immediately AFTER the current focus (wrapping), so
        // repeated presses visit every one.
        let cur = self.focused_terminal();
        let target = match cur
            .as_ref()
            .and_then(|c| blocked.iter().position(|t| t == c))
        {
            Some(i) => blocked[(i + 1) % blocked.len()].clone(),
            None => blocked[0].clone(),
        };
        self.jump_to_terminal(&target);
    }

    /// Switch to the session at 0-based `index` (as listed by `list-sessions`).
    fn select_session_index(&mut self, index: usize) {
        if let Some(id) = self.session_ids().get(index).cloned() {
            self.switch_session(id);
        }
    }

    /// Locate a terminal's `(workspace, tab, pane)` across ALL sessions and tabs (not
    /// just the active one), so a background session's/tab's exited shell is reaped.
    fn locate_terminal(&self, term: &TerminalId) -> Option<(WorkspaceId, TabId, PaneId)> {
        for wid in self.state.workspace_ids() {
            let Some(w) = self.state.workspace(&wid) else {
                continue;
            };
            for t in &w.tabs {
                for p in t.layout.panes() {
                    if t.layout.terminal_of(&p) == Some(term) {
                        return Some((wid.clone(), t.id.clone(), p));
                    }
                }
            }
        }
        None
    }

    // --- control API (spec §3): applied on the main loop (single writer) ---

    /// Handle one control request, mutating state as the sole writer. Returns the
    /// wire response. Never blocks.
    pub fn handle_control(&mut self, req: &control::Req) -> control::Resp {
        use control::{PaneInfo, Req, Resp};
        match req {
            Req::List => {
                let order = self.pane_order();
                let focused = self.focused_pane();
                let rects = self.layout();
                let panes = order
                    .iter()
                    .enumerate()
                    .map(|(index, p)| {
                        let term = self
                            .state
                            .workspace(&self.ws)
                            .and_then(|w| w.tab(&w.active_tab))
                            .and_then(|t| t.layout.terminal_of(p).cloned());
                        let (cols, rows) = term
                            .as_ref()
                            .and_then(|tid| rects.iter().find(|r| &r.terminal == tid))
                            .map(|r| (r.cols, r.rows))
                            .unwrap_or((0, 0));
                        let is_agent = self.pane_label_at(index).map(|l| l.kind)
                            == Some(procinfo::Kind::Agent);
                        let kind = if is_agent {
                            "agent"
                        } else if self.pane_label_at(index).map(|l| l.kind)
                            == Some(procinfo::Kind::Shell)
                        {
                            "shell"
                        } else {
                            "other"
                        };
                        let status = match (is_agent, term.as_ref()) {
                            (true, Some(tid)) => self.agent_status(tid).to_string(),
                            _ => String::new(),
                        };
                        PaneInfo {
                            index,
                            id: p.to_string(),
                            focused: focused.as_ref() == Some(p),
                            cols,
                            rows,
                            label: self.pane_label(index),
                            kind: kind.to_string(),
                            status,
                        }
                    })
                    .collect();
                let fi = focused
                    .and_then(|f| order.iter().position(|p| p == &f))
                    .unwrap_or(0);
                Resp::list(panes, fi)
            }
            Req::Split { dir } => {
                let d = match dir.as_str() {
                    "down" => Dir::Down,
                    "right" => Dir::Right,
                    other => return Resp::err(format!("bad dir '{other}' (right|down)")),
                };
                let before = self.pane_order().len();
                self.split(d);
                if self.pane_order().len() > before {
                    Resp::ok()
                } else {
                    Resp::err("split failed (could not spawn a shell)")
                }
            }
            Req::ResizePane { index, dir } => {
                let (axis, grow) = match dir.as_str() {
                    "right" => (Dir::Right, true),
                    "left" => (Dir::Right, false),
                    "down" => (Dir::Down, true),
                    "up" => (Dir::Down, false),
                    other => return Resp::err(format!("bad dir '{other}' (left|right|up|down)")),
                };
                match self.pane_order().get(*index).cloned() {
                    Some(pane) => {
                        let _ = self.state.apply(Command::ResizePane {
                            origin: Origin::Client(self.client),
                            workspace: self.ws.clone(),
                            pane,
                            axis,
                            grow,
                        });
                        self.sync_sizes();
                        Resp::ok()
                    }
                    None => Resp::err(format!("no pane at index {index}")),
                }
            }
            Req::Focus { index } => match self.pane_order().get(*index).cloned() {
                Some(pane) => {
                    let _ = self.state.apply(Command::FocusPane {
                        client: self.client,
                        pane,
                    });
                    Resp::ok()
                }
                None => Resp::err(format!("no pane at index {index}")),
            },
            Req::Close { index } => match self.pane_order().get(*index).cloned() {
                Some(pane) => match self.close_pane(pane) {
                    Ok(()) => Resp::ok(),
                    Err(e) => Resp::err(e.to_string()),
                },
                None => Resp::err(format!("no pane at index {index}")),
            },
            Req::SendKeys { index, text } => {
                let order = self.pane_order();
                let Some(pane) = order.get(*index) else {
                    return Resp::err(format!("no pane at index {index}"));
                };
                let term = self
                    .state
                    .workspace(&self.ws)
                    .and_then(|w| w.tab(&w.active_tab))
                    .and_then(|t| t.layout.terminal_of(pane).cloned());
                match term.and_then(|tid| self.panes.get(&tid)) {
                    Some(pt) => {
                        pt.input(text.as_bytes());
                        Resp::ok()
                    }
                    None => Resp::err("pane has no live terminal"),
                }
            }
            Req::ListTabs => {
                let Some(w) = self.state.workspace(&self.ws) else {
                    return Resp::err("no workspace");
                };
                let active_id = w.active_tab.clone();
                let mut active_idx = 0;
                let infos = w
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let panes = t.layout.panes();
                        let agents = panes
                            .iter()
                            .filter(|p| {
                                t.layout
                                    .terminal_of(p)
                                    .and_then(|tid| self.labels.get(tid))
                                    .map(|l| l.kind == procinfo::Kind::Agent)
                                    .unwrap_or(false)
                            })
                            .count();
                        let active = t.id == active_id;
                        if active {
                            active_idx = i;
                        }
                        control::TabInfo {
                            index: i,
                            id: t.id.to_string(),
                            active,
                            name: t.name.clone().unwrap_or_default(),
                            panes: panes.len(),
                            agents,
                        }
                    })
                    .collect();
                Resp::tab_list(infos, active_idx)
            }
            Req::NewTab => {
                let before = self.tab_count();
                self.new_tab();
                if self.tab_count() > before {
                    Resp::ok()
                } else {
                    Resp::err("new-tab failed (could not spawn a shell)")
                }
            }
            Req::SelectTab { index } => {
                if self.tab_ids().get(*index).is_some() {
                    self.select_tab_index(*index);
                    Resp::ok()
                } else {
                    Resp::err(format!("no tab at index {index}"))
                }
            }
            Req::RenameTab { index, name } => {
                // No index → the ACTIVE tab (so a pane's shell can rename its own tab).
                let tab = match index {
                    Some(i) => self.tab_ids().get(*i).cloned(),
                    None => self.active_tab_id(),
                };
                match tab {
                    Some(tab) => {
                        let new = (!name.trim().is_empty()).then(|| name.trim().to_string());
                        self.rename_tab(&self.ws.clone(), &tab, new);
                        Resp::ok()
                    }
                    None => Resp::err(match index {
                        Some(i) => format!("no tab at index {i}"),
                        None => "no active tab".to_string(),
                    }),
                }
            }
            Req::ListSessions => {
                let ids = self.session_ids();
                // `ctl` uses CREATION order for stable indices — find active in THAT order
                // (not the sorted display order).
                let active_idx = ids.iter().position(|w| w == &self.ws).unwrap_or(0);
                let infos = ids
                    .iter()
                    .enumerate()
                    .map(|(i, wid)| {
                        let (tabs, panes, agents) = self
                            .state
                            .workspace(wid)
                            .map(|w| {
                                let mut panes = 0usize;
                                let mut agents = 0usize;
                                for t in &w.tabs {
                                    for p in t.layout.panes() {
                                        panes += 1;
                                        let is_agent = t
                                            .layout
                                            .terminal_of(&p)
                                            .and_then(|tid| self.labels.get(tid))
                                            .map(|l| l.kind == procinfo::Kind::Agent)
                                            .unwrap_or(false);
                                        if is_agent {
                                            agents += 1;
                                        }
                                    }
                                }
                                (w.tabs.len(), panes, agents)
                            })
                            .unwrap_or((0, 0, 0));
                        let name = self
                            .state
                            .workspace(wid)
                            .and_then(|w| w.name.clone())
                            .unwrap_or_default();
                        control::SessionInfo {
                            index: i,
                            id: wid.to_string(),
                            name,
                            active: i == active_idx,
                            tabs,
                            panes,
                            agents,
                        }
                    })
                    .collect();
                Resp::session_list(infos, active_idx)
            }
            Req::NewSession { name, cwd } => {
                let before = self.session_count();
                // Prefer the caller's cwd (CLI); fall back to the focused pane's.
                let dir = cwd
                    .clone()
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .or_else(|| self.focused_cwd());
                self.new_session(name.clone(), dir);
                if self.session_count() > before {
                    Resp::ok()
                } else {
                    Resp::err("new-session failed (could not spawn a shell)")
                }
            }
            Req::RenameSession { index, name } => {
                // No index → the ACTIVE session (mirrors `rename-tab`).
                let wid = match index {
                    Some(i) => self.session_ids().get(*i).cloned(),
                    None => Some(self.ws.clone()),
                };
                match wid {
                    Some(wid) => {
                        let new = (!name.trim().is_empty()).then(|| name.trim().to_string());
                        self.state.set_workspace_name(&wid, new);
                        Resp::ok()
                    }
                    None => Resp::err(format!(
                        "no session at index {}",
                        index.expect("None resolves to the active session")
                    )),
                }
            }
            Req::SelectSession { index } => {
                if self.session_ids().get(*index).is_some() {
                    self.select_session_index(*index);
                    Resp::ok()
                } else {
                    Resp::err(format!("no session at index {index}"))
                }
            }
            Req::WorktreeCreate { branch, from, cwd } => {
                let start = cwd
                    .clone()
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .or_else(|| self.focused_cwd());
                match self.create_worktree_session(branch, from.as_deref().unwrap_or(""), start) {
                    Ok(c) => Resp::message(match c.warning {
                        Some(w) => format!("{} (warning: {w})", c.message),
                        None => c.message,
                    }),
                    Err(e) => Resp::err(e),
                }
            }
            Req::WorktreeList { cwd } => {
                let start = cwd
                    .clone()
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .or_else(|| self.focused_cwd());
                match self.worktree_list_infos(start) {
                    Ok(infos) => Resp::worktree_list(infos),
                    Err(e) => Resp::err(e),
                }
            }
            Req::WorktreeRm {
                target,
                force,
                delete_branch,
                cwd,
            } => {
                let start = cwd
                    .clone()
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .or_else(|| self.focused_cwd());
                self.worktree_rm(target, *force, *delete_branch, start)
            }
            // Intercepted by the server before it reaches here; answered ok for a
            // (hypothetical) direct call so the match stays exhaustive.
            Req::KillServer => Resp::ok(),
        }
    }

    /// Enumerate the repo's worktrees (resolved from `start`) with a `live` flag for
    /// worktrees a comux session is currently inside.
    fn worktree_list_infos(
        &self,
        start: Option<PathBuf>,
    ) -> Result<Vec<control::WorktreeInfo>, String> {
        let start = start
            .or_else(|| std::env::current_dir().ok())
            .ok_or("could not resolve a starting directory")?;
        let repo = crate::worktree::resolve_repo_root(&start)?;
        let entries = crate::worktree::list_entries(&repo)?;
        Ok(entries
            .iter()
            .map(|e| control::WorktreeInfo {
                path: e.path.display().to_string(),
                branch: e.branch.clone().unwrap_or_default(),
                is_main: e.is_main,
                live: !self.sessions_in_path(&e.path).is_empty(),
                locked: e.locked,
            })
            .collect())
    }

    /// Remove a worktree (resolved from `start`), enforcing live-session safety: refuse a
    /// worktree a session is inside unless `force`, which first kills those sessions —
    /// but only after a preflight guarantees ≥1 workspace survives (the mux keeps the
    /// last), then re-checks the worktree is truly vacated before touching git.
    fn worktree_rm(
        &mut self,
        target: &str,
        force: bool,
        delete_branch: bool,
        start: Option<PathBuf>,
    ) -> control::Resp {
        let Some(start) = start.or_else(|| std::env::current_dir().ok()) else {
            return control::Resp::err("could not resolve a starting directory");
        };
        let repo = match crate::worktree::resolve_repo_root(&start) {
            Ok(r) => r,
            Err(e) => return control::Resp::err(e),
        };
        let entries = match crate::worktree::list_entries(&repo) {
            Ok(e) => e,
            Err(e) => return control::Resp::err(e),
        };
        let entry = match crate::worktree::validate_removal(&entries, target, &start, delete_branch)
        {
            Ok(e) => e,
            Err(e) => return control::Resp::err(e),
        };
        let live = self.sessions_in_path(&entry.path);
        if !live.is_empty() {
            if !force {
                return control::Resp::err(format!(
                    "{} live session(s) are inside {} — use --force to kill them",
                    live.len(),
                    entry.path.display()
                ));
            }
            // Killing every live session must leave ≥1 workspace (the mux refuses to
            // remove the last), else the worktree can never be fully vacated.
            if live.len() >= self.session_count() {
                return control::Resp::err(
                    "refusing to remove: every session is inside this worktree (the mux keeps ≥1)",
                );
            }
            for wid in &live {
                self.kill_session(wid);
            }
            if !self.sessions_in_path(&entry.path).is_empty() {
                return control::Resp::err(
                    "could not vacate the worktree (a session survived) — not removing",
                );
            }
        }
        if let Err(e) = crate::worktree::remove(&repo, &entry.path, force) {
            return control::Resp::err(e);
        }
        control::finish_branch_delete(&repo, &entry, delete_branch, force)
    }

    /// Route one key event through the mux (popup → prefix → direct-nav → shell).
    /// Returns [`KeyAction::Detach`] on the detach chord. The `Ctrl-b` prefix state is
    /// PER-CLIENT (`prefix`, owned by the caller) so a chord can't span connections
    /// when several clients share input; the popup is shared workspace state.
    pub fn feed_key(&mut self, k: KeyEvent, prefix: &mut bool) -> KeyAction {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        // An open inline prompt (new-session name / rename) captures all input: type
        // to edit, Enter commits, Esc cancels. Shared state, so clear this client's
        // pending prefix too.
        if let Some(mut p) = self.prompt.take() {
            *prefix = false;
            match k.code {
                KeyCode::Enter => self.commit_prompt(p),
                KeyCode::Esc => {} // cancel: dropped by the take()
                KeyCode::Backspace => {
                    p.buf.pop();
                    self.prompt = Some(p);
                }
                KeyCode::Char(c) if !ctrl => {
                    p.buf.push(c);
                    self.prompt = Some(p);
                }
                _ => self.prompt = Some(p),
            }
            return KeyAction::Continue;
        }

        // A y/n confirm modal (kill-session) captures input while open: `y`/`Y`
        // performs the captured action, anything else cancels.
        if let Some(cf) = self.confirm.take() {
            *prefix = false;
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.perform_confirm(cf.action);
            }
            return KeyAction::Continue;
        }

        // An open right-click context menu captures input (tmux display-menu keys):
        // `j`/`k`/arrows navigate, Enter runs the selection, anything else closes.
        if self.menu.is_some() {
            *prefix = false;
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => self.menu_move(1),
                KeyCode::Up | KeyCode::Char('k') => self.menu_move(-1),
                KeyCode::Enter => {
                    let action = self
                        .menu
                        .take()
                        .and_then(|m| m.sel.map(|i| m.items[i].1.clone()));
                    if let Some(a) = action {
                        self.run_menu_action(a);
                    }
                }
                _ => self.menu = None,
            }
            return KeyAction::Continue;
        }

        // Sidebar keyboard-focus mode (`Ctrl-b e`): hjkl/arrows navigate the spaces/agents
        // lists (h/l or ←/→ switch group, j/k or ↑/↓ move), Enter selects, Esc/q exits.
        if self.sidebar_focus.is_some() {
            *prefix = false;
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => self.sidebar_focus_move(1),
                KeyCode::Up | KeyCode::Char('k') => self.sidebar_focus_move(-1),
                KeyCode::Left | KeyCode::Char('h') => self.sidebar_focus_tab(PopupTab::Sessions),
                KeyCode::Right | KeyCode::Char('l') => self.sidebar_focus_tab(PopupTab::Agents),
                KeyCode::Enter => self.sidebar_focus_select(),
                KeyCode::Esc | KeyCode::Char('q') => self.sidebar_focus = None,
                _ => {}
            }
            return KeyAction::Continue;
        }

        // The switcher captures all input while open. Clear THIS client's pending
        // prefix too: the popup is shared state, so a `Ctrl-b` armed on one client
        // must not survive another client's popup interaction and fire a stray mux
        // command (close/detach) after the popup closes.
        if self.popup.is_some() {
            *prefix = false;
            match k.code {
                // ↑/↓ (or Ctrl-n/p) move the list; ←/→ switch the sessions/agents tab.
                KeyCode::Down => self.popup_move(1),
                KeyCode::Up => self.popup_move(-1),
                KeyCode::Char('n') if ctrl => self.popup_move(1),
                KeyCode::Char('p') if ctrl => self.popup_move(-1),
                KeyCode::Left => self.popup_tab(PopupTab::Sessions),
                KeyCode::Right => self.popup_tab(PopupTab::Agents),
                KeyCode::Enter => self.popup_select(),
                // Ctrl-r / F2: rename the selected session inline.
                KeyCode::Char('r') if ctrl => self.popup_rename(),
                KeyCode::Char('\u{12}') => self.popup_rename(),
                KeyCode::F(2) => self.popup_rename(),
                KeyCode::Esc | KeyCode::Char('\u{6}') => self.popup = None,
                KeyCode::Char('f') if ctrl => self.popup = None, // Ctrl-f toggles closed
                KeyCode::Backspace => self.popup_backspace(),
                // Any other printable char extends the fuzzy filter.
                KeyCode::Char(c) if !ctrl => self.popup_type(c),
                _ => {}
            }
            return KeyAction::Continue;
        }

        // Scrollback mode (`Ctrl-b [`): keys drive the BOUND pane's history (bound at
        // entry, so a focus change can't strand it). If that pane is gone, drop out.
        if let Some(term) = self.scroll_pane.clone() {
            if !self.panes.contains_key(&term) {
                self.scroll_pane = None;
            } else {
                let page = (self.term_rows(&term) / 2).max(1) as i32;
                match k.code {
                    KeyCode::Char('k') | KeyCode::Up => self.scroll_term(&term, 1),
                    KeyCode::Char('j') | KeyCode::Down => self.scroll_term(&term, -1),
                    KeyCode::PageUp => self.scroll_term(&term, page),
                    KeyCode::PageDown => self.scroll_term(&term, -page),
                    KeyCode::Char('u') if ctrl => self.scroll_term(&term, page),
                    KeyCode::Char('d') if ctrl => self.scroll_term(&term, -page),
                    KeyCode::Char('g') => self.scroll_term(&term, 1_000_000), // clamps to top
                    // `G`/`q`/`Esc` return the bound pane to the live bottom AND exit.
                    KeyCode::Char('G') | KeyCode::Char('q') | KeyCode::Esc => {
                        self.scroll_pane = None;
                        self.scroll_term_to_bottom(&term);
                    }
                    _ => {}
                }
                *prefix = false;
                return KeyAction::Continue;
            }
        }

        // Notification center (`Ctrl-b a`) captures input while open.
        if self.center.is_some() {
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => self.center_move(1),
                KeyCode::Char('k') | KeyCode::Up => self.center_move(-1),
                KeyCode::Enter => self.center_jump(),
                KeyCode::Char('d') => self.center_dismiss(),
                KeyCode::Char('D') => self.notifications.clear(),
                KeyCode::Esc | KeyCode::Char('q') => self.center = None,
                _ => {}
            }
            *prefix = false;
            return KeyAction::Continue;
        }

        // From here on, keys resolve through the configurable keymap (see `config.rs`).
        // A key we never bind (function keys, etc.) can't form a chord — treat it as
        // shell input (and disarm a dangling prefix).
        let Some(chord) = config::chord_of(&k) else {
            *prefix = false;
            if let Some(bytes) = key_to_bytes(k.code, k.modifiers) {
                self.input_focused(&bytes);
            }
            return KeyAction::Continue;
        };

        // Prefix armed: the next chord is a prefix-table binding (or nothing).
        if *prefix {
            *prefix = false;
            if let Some(action) = self.cfg.keymap.prefix_action(&chord) {
                return self.dispatch(action);
            }
            return KeyAction::Continue;
        }

        // Prefix-less: a global binding wins (incl. entering the prefix); otherwise the
        // key is shell input.
        if let Some(action) = self.cfg.keymap.global_action(&chord) {
            if action == Action::EnterPrefix {
                *prefix = true;
                return KeyAction::Continue;
            }
            return self.dispatch(action);
        }
        if let Some(bytes) = key_to_bytes(k.code, k.modifiers) {
            self.input_focused(&bytes);
        }
        KeyAction::Continue
    }

    /// Execute a resolved keymap [`Action`]. Returns [`KeyAction::Detach`] for the
    /// detach action, [`KeyAction::Continue`] otherwise.
    fn dispatch(&mut self, action: Action) -> KeyAction {
        use Action::*;
        match action {
            SplitRight => self.split(Dir::Right),
            SplitDown => self.split(Dir::Down),
            NewTab => self.new_tab(),
            NextTab => self.cycle_tab(1),
            PrevTab => self.cycle_tab(-1),
            CloseTab => self.close_active_tab(),
            RenameTab => self.open_rename_tab_prompt(),
            SelectTab(n) => self.select_tab_index(n as usize),
            NewSession => self.open_new_session_prompt(),
            NewWorktree => self.open_new_worktree_prompt(),
            RenameSession => self.open_rename_prompt(),
            NextSession => self.cycle_session(1),
            PrevSession => self.cycle_session(-1),
            KillSession => self.open_kill_session_confirm(),
            NotificationCenter => self.open_center(),
            JumpAttention => self.jump_to_attention(),
            Detach => return KeyAction::Detach,
            ClosePane => self.close_focused(),
            ToggleSidebar => self.toggle_sidebar(),
            Scrollback => self.scroll_pane = self.focused_terminal(),
            FocusNext => self.focus_next(),
            FocusLeft => self.focus_dir(FocusDir::Left),
            FocusDown => self.focus_dir(FocusDir::Down),
            FocusUp => self.focus_dir(FocusDir::Up),
            FocusRight => self.focus_dir(FocusDir::Right),
            // L/H widen/narrow (Right axis), J/K taller/shorter (Down axis).
            ResizeRight => self.resize_focused(Dir::Right, true),
            ResizeLeft => self.resize_focused(Dir::Right, false),
            ResizeDown => self.resize_focused(Dir::Down, true),
            ResizeUp => self.resize_focused(Dir::Down, false),
            Popup => self.open_popup(),
            Redraw => return KeyAction::Redraw,
            FocusSidebar => self.open_sidebar_focus(),
            EnterPrefix => {} // handled by the caller (arms the prefix)
        }
        KeyAction::Continue
    }

    fn focus_next(&mut self) {
        let order = self.pane_order();
        if order.len() < 2 {
            return;
        }
        let Some(cur) = self.focused_pane() else {
            return;
        };
        let idx = order.iter().position(|p| p == &cur).unwrap_or(0);
        let next = order[(idx + 1) % order.len()].clone();
        let _ = self.state.apply(Command::FocusPane {
            client: self.client,
            pane: next,
        });
    }

    /// Focus the nearest pane in a direction, by rect-center heuristic.
    fn focus_dir(&mut self, dir: FocusDir) {
        let rects = self.layout();
        let Some(cur_term) = self.focused_terminal() else {
            return;
        };
        let Some(cur) = rects.iter().find(|r| r.terminal == cur_term) else {
            return;
        };
        let ccx = cur.x as i32 + cur.cols as i32 / 2;
        let ccy = cur.y as i32 + cur.rows as i32 / 2;
        let mut best: Option<(&PaneRect, i32)> = None;
        for r in &rects {
            if r.terminal == cur_term {
                continue;
            }
            let cx = r.x as i32 + r.cols as i32 / 2;
            let cy = r.y as i32 + r.rows as i32 / 2;
            let (dx, dy) = (cx - ccx, cy - ccy);
            let ok = match dir {
                FocusDir::Left => dx < 0 && dx.abs() >= dy.abs(),
                FocusDir::Right => dx > 0 && dx.abs() >= dy.abs(),
                FocusDir::Up => dy < 0 && dy.abs() >= dx.abs(),
                FocusDir::Down => dy > 0 && dy.abs() >= dx.abs(),
            };
            if !ok {
                continue;
            }
            let dist = dx * dx + dy * dy;
            if best.map(|(_, d)| dist < d).unwrap_or(true) {
                best = Some((r, dist));
            }
        }
        if let Some((r, _)) = best
            && let Some(pane) = self.pane_of_terminal(&r.terminal)
        {
            let _ = self.state.apply(Command::FocusPane {
                client: self.client,
                pane,
            });
        }
    }

    fn input_focused(&self, bytes: &[u8]) {
        if let Some(term) = self.focused_terminal()
            && let Some(pt) = self.panes.get(&term)
        {
            pt.scroll_to_bottom(); // typing returns to the live bottom
            pt.input(bytes);
        }
    }

    // --- scrollback ---

    /// Handle a forwarded mouse action at frame cell `(x, y)`: the wheel scrolls the
    /// pane UNDER the cursor (falling back to the focused pane); a left click focuses
    /// the pane under the cursor.
    ///
    /// Returns `Some(text)` only on a button-up that ends a real drag — the pane text the
    /// server should ship to the initiating `client` as an OSC 52 clipboard copy.
    pub fn mouse_at(
        &mut self,
        client: ClientId,
        x: u16,
        y: u16,
        kind: MouseKind,
    ) -> Option<String> {
        if !self.cfg.mouse {
            return None; // mouse disabled in config (client also skips capture)
        }
        // Right-button events + clicks on an OPEN menu belong to the context menu.
        match kind {
            MouseKind::RightClick | MouseKind::RightDrag | MouseKind::RightUp => {
                return self.menu_mouse(client, x, y, kind);
            }
            MouseKind::Click if self.menu.is_some() => {
                // Left click while the menu is open: on an item runs it (any client —
                // input is shared, tmux-style); elsewhere it just dismisses. Either
                // way the click is CONSUMED — it never falls through to the panes.
                let hit = self
                    .menu
                    .as_ref()
                    .and_then(|m| m.item_at(x, y).map(|i| m.items[i].1.clone()));
                self.menu = None;
                if let Some(a) = hit {
                    self.run_menu_action(a);
                }
                return None;
            }
            _ => {}
        }
        let target = self
            .layout()
            .into_iter()
            .find(|r| x >= r.x && x < r.x + r.cols && y >= r.y && y < r.y + r.rows);
        match kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let up = matches!(kind, MouseKind::ScrollUp);
                // A wheel over the usage readout pages the carousel (up = previous,
                // down = next; wraps) instead of scrolling a pane.
                if matches!(self.click_target_at(x, y), Some(ClickTarget::Usage)) {
                    self.page_usage(if up { -1 } else { 1 });
                    return None;
                }
                // The pane under the cursor, else the focused one (tmux forwards to the
                // pane the pointer is over).
                let rect = target.clone().or_else(|| {
                    let f = self.focused_terminal()?;
                    self.layout().into_iter().find(|r| r.terminal == f)
                });
                if let Some(rect) = rect
                    && let Some(pt) = self.panes.get(&rect.terminal)
                {
                    // 1-based cell coords within the pane (clamped for the focused-pane
                    // fallback, where the pointer can sit outside it).
                    let col = x.saturating_sub(rect.x).min(rect.cols.saturating_sub(1)) + 1;
                    let row = y.saturating_sub(rect.y).min(rect.rows.saturating_sub(1)) + 1;
                    if let Some(bytes) = pt.wheel_bytes(up, col, row) {
                        // The app is listening for the wheel (mouse mode / alternate-scroll)
                        // — forward it so its OWN scroll advances (Claude Code, less, nvim).
                        pt.input(&bytes);
                    } else {
                        // No mouse app in the pane: scroll comux's own scrollback.
                        let lines = if up {
                            self.cfg.scroll_step
                        } else {
                            -self.cfg.scroll_step
                        };
                        pt.scroll(lines);
                    }
                }
                None
            }
            MouseKind::Click => {
                // Don't disturb ANOTHER client's in-progress drag (input is multi-client): its
                // owner alone may end it. This client's own click still acts normally.
                let other_dragging = self
                    .selection
                    .as_ref()
                    .is_some_and(|s| s.owner != client && s.moved);
                // Chrome first: a click on a tab chip / sidebar row navigates. Only if
                // it misses all zones does it fall through to click-to-focus a pane.
                if let Some(t) = self.click_target_at(x, y) {
                    if !other_dragging {
                        self.selection = None; // a chrome click never starts a selection
                    }
                    match t {
                        ClickTarget::Tab(id) => {
                            if let Some(i) = self.tab_ids().iter().position(|t| t == &id) {
                                self.select_tab_index(i);
                            }
                        }
                        ClickTarget::Session(wid) => self.switch_session(wid),
                        ClickTarget::Agent(term) => self.jump_to_terminal(&term),
                        // Right-click menu anchor only — a left click must NOT
                        // re-attach to the active session (lease churn).
                        ClickTarget::SessionPill(_) => {}
                        // A left click advances the carousel one page (wraps) — the
                        // no-wheel fallback for paging the usage readout.
                        ClickTarget::Usage => self.page_usage(1),
                    }
                    return None;
                }
                if let Some(r) = target {
                    if let Some(pane) = self.pane_of_terminal(&r.terminal) {
                        let _ = self.state.apply(Command::FocusPane {
                            client: self.client,
                            pane,
                        });
                    }
                    // Anchor a drag-selection at this pane-relative cell. A plain click (no
                    // drag) just leaves `moved: false` and copies nothing on release. Guarded by
                    // `other_dragging` so a fresh selection can't clobber another client's active
                    // drag — it waits until that drag releases.
                    if !other_dragging {
                        self.selection = Some(Sel {
                            term: r.terminal.clone(),
                            owner: client,
                            anchor: (x - r.x, y - r.y),
                            head: (x - r.x, y - r.y),
                            moved: false,
                        });
                    }
                } else if !other_dragging {
                    self.selection = None; // clicked chrome gap / letterbox
                }
                None
            }
            MouseKind::Drag => {
                // Only the owning client extends its own selection.
                let owner = self.selection.as_ref().map(|s| s.owner);
                if owner != Some(client) {
                    return None;
                }
                let term = self.selection.as_ref().map(|s| s.term.clone())?;
                // Re-look-up the pane rect in the CURRENT layout — it may have moved/vanished
                // since button-down. Gone → drop the selection (no stale coords).
                let Some(rect) = self.layout().into_iter().find(|r| r.terminal == term) else {
                    self.selection = None;
                    return None;
                };
                // CLAMP the pointer to the origin pane, so the selection can never extend into
                // the sidebar / status bar / dividers / another pane.
                let cx = x.clamp(rect.x, rect.x + rect.cols.saturating_sub(1));
                let cy = y.clamp(rect.y, rect.y + rect.rows.saturating_sub(1));
                if let Some(sel) = self.selection.as_mut() {
                    sel.head = (cx - rect.x, cy - rect.y);
                    sel.moved = true;
                }
                None
            }
            MouseKind::Up => {
                // Only the owner finishes its selection; a non-owner release is ignored.
                if self.selection.as_ref().map(|s| s.owner) != Some(client) {
                    return None;
                }
                let sel = self.selection.take()?; // release always ends the selection
                if !sel.moved {
                    return None; // it was a plain click, not a drag — nothing to copy
                }
                // Extract from the CURRENT snapshot (WYSIWYG: copy what's highlighted now).
                let rect = self.layout().into_iter().find(|r| r.terminal == sel.term)?;
                let _ = rect; // presence check: the pane is still visible
                let snap = self.panes.get(&sel.term)?.snapshot();
                let text = extract_selection(&snap, sel.anchor, sel.head);
                (!text.is_empty()).then_some(text)
            }
            // Routed to `menu_mouse` by the early match above.
            MouseKind::RightClick | MouseKind::RightDrag | MouseKind::RightUp => None,
        }
    }

    /// Cancel any in-progress drag-selection (its coords are only valid for the layout at
    /// button-down). Called on layout mutations and on the owner's key input / disconnect.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Drop the selection if it belongs to `client` (its owner detached).
    pub fn clear_selection_of(&mut self, client: ClientId) {
        if self.selection.as_ref().is_some_and(|s| s.owner == client) {
            self.selection = None;
        }
    }

    /// The chrome action for a click at frame cell `(x, y)`, if it lands on a recorded
    /// tab chip / sidebar row (first match wins).
    fn click_target_at(&self, x: u16, y: u16) -> Option<ClickTarget> {
        self.click_zones
            .borrow()
            .iter()
            .find(|z| x >= z.x0 && x <= z.x1 && y >= z.y0 && y <= z.y1)
            .map(|z| z.target.clone())
    }

    /// Switch to a specific agent/pane by terminal id: its session, then its tab, then
    /// focus it. Shared with sidebar agent-row clicks (cf. [`Self::jump_to_attention`]).
    fn jump_to_terminal(&mut self, term: &TerminalId) {
        if let Some((wid, tab, pane)) = self.locate_terminal(term) {
            self.switch_session(wid.clone());
            let _ = self.state.apply(Command::SelectTab {
                origin: Origin::Client(self.client),
                workspace: wid,
                tab,
            });
            let _ = self.state.apply(Command::FocusPane {
                client: self.client,
                pane,
            });
            self.reflow();
        }
    }

    // --- context menu (right-click on chrome, tmux `display-menu`) ---

    /// Handle a right-button event: down on chrome opens the menu (hold mode), drag
    /// hovers its items, release over an item runs it — release elsewhere leaves the
    /// menu open for left-click / keyboard selection (tmux behavior). Never copies.
    fn menu_mouse(&mut self, client: ClientId, x: u16, y: u16, kind: MouseKind) -> Option<String> {
        match kind {
            MouseKind::RightClick => {
                // A text prompt / y-n confirm owns the keyboard (feed_key handles them
                // before the menu), so a menu opened over one would render on top while
                // keystrokes still edit the overlay underneath. Refuse instead.
                if self.prompt.is_some() || self.confirm.is_some() {
                    return None;
                }
                // A re-press on the open menu's items re-arms hold-release mode.
                let mut handled = false;
                if let Some(m) = self.menu.as_mut()
                    && m.owner == client
                    && let Some(i) = m.item_at(x, y)
                {
                    m.sel = Some(i);
                    m.held = true;
                    handled = true;
                }
                if !handled {
                    self.menu = None; // dismiss any open menu…
                    if let Some(t) = self.click_target_at(x, y) {
                        self.open_menu(client, x, y, t); // …and reopen on a new target
                    }
                }
            }
            MouseKind::RightDrag => {
                if let Some(m) = self.menu.as_mut()
                    && m.owner == client
                    && m.held
                {
                    m.sel = m.item_at(x, y);
                }
            }
            MouseKind::RightUp => {
                let mut fire = None;
                if let Some(m) = self.menu.as_mut()
                    && m.owner == client
                {
                    match m.held.then(|| m.item_at(x, y)).flatten() {
                        Some(i) => fire = Some(m.items[i].1.clone()),
                        // Released off the items: keep the menu, drop hold mode.
                        None => m.held = false,
                    }
                }
                if let Some(a) = fire {
                    self.menu = None;
                    self.run_menu_action(a);
                }
            }
            _ => {}
        }
        None
    }

    /// Build + open the context menu for a right-clicked chrome target at frame cell
    /// `(x, y)`. Item targets are captured NOW (menu-open time), not at selection.
    fn open_menu(&mut self, client: ClientId, x: u16, y: u16, target: ClickTarget) {
        let (title, items): (String, Vec<(&'static str, MenuAction)>) = match target {
            // The usage readout has no context menu (v1) — a right-click on it is inert.
            ClickTarget::Usage => return,
            ClickTarget::Tab(id) => {
                let n = self
                    .tab_ids()
                    .iter()
                    .position(|t| t == &id)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let title = self
                    .tab_custom_name(&id)
                    .unwrap_or_else(|| format!("tab {n}"));
                (
                    title,
                    vec![
                        (
                            "rename tab",
                            MenuAction::RenameTab(self.ws.clone(), id.clone()),
                        ),
                        ("close tab", MenuAction::CloseTab(self.ws.clone(), id)),
                        ("new tab", MenuAction::NewTab(self.ws.clone())),
                    ],
                )
            }
            ClickTarget::Session(wid) | ClickTarget::SessionPill(wid) => {
                let title = self
                    .state
                    .workspace(&wid)
                    .and_then(|w| w.name.clone())
                    .unwrap_or_else(|| wid.to_string());
                (
                    title,
                    vec![
                        ("rename session", MenuAction::RenameSession(wid.clone())),
                        ("new session", MenuAction::NewSession),
                        ("kill session", MenuAction::KillSession(wid)),
                    ],
                )
            }
            ClickTarget::Agent(term) => {
                let title = self
                    .labels
                    .get(&term)
                    .map(|l| l.text.clone())
                    .unwrap_or_else(|| "agent".to_string());
                let mut items = vec![("jump to pane", MenuAction::JumpAgent(term.clone()))];
                // Rename the agent's TAB — the alias shown for it in the sidebar.
                if let Some((wid, tab, _)) = self.locate_terminal(&term) {
                    items.push(("rename tab", MenuAction::RenameTab(wid, tab)));
                }
                (title, items)
            }
        };
        // Box size: widest of items / bordered title, + 1-cell item padding each side.
        let inner = items
            .iter()
            .map(|(l, _)| l.width())
            .max()
            .unwrap_or(0)
            .max(title.width() + 2);
        let w = (inner as u16 + 4).min(self.cols.max(1));
        let h = (items.len() as u16 + 2).min(self.rows.max(1));
        let (x0, y0) = menu_origin(x, y, w, h, self.cols, self.rows);
        self.menu = Some(Menu {
            title,
            items,
            x0,
            y0,
            w,
            h,
            sel: None,
            owner: client,
            held: true,
        });
    }

    /// Run a selected menu item. The menu is closed by the caller; captured ids are
    /// re-validated by each operation (a vanished tab/session is a no-op).
    fn run_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::RenameTab(ws, tab) => self.open_rename_tab_prompt_for(ws, tab),
            MenuAction::CloseTab(ws, tab) => self.close_tab(&ws, tab),
            MenuAction::NewTab(ws) => {
                if self.ws == ws {
                    self.new_tab();
                }
            }
            MenuAction::RenameSession(ws) => self.open_rename_prompt_for(ws),
            MenuAction::NewSession => self.open_new_session_prompt(),
            MenuAction::KillSession(ws) => self.open_kill_session_confirm_for(ws),
            MenuAction::JumpAgent(term) => self.jump_to_terminal(&term),
        }
    }

    /// Move the menu's keyboard selection by `delta`, wrapping. From no selection,
    /// `j` enters at the top and `k` at the bottom.
    fn menu_move(&mut self, delta: i32) {
        if let Some(m) = self.menu.as_mut() {
            let n = m.items.len() as i32;
            if n == 0 {
                return;
            }
            m.sel = Some(match m.sel {
                Some(s) => (s as i32 + delta).rem_euclid(n) as usize,
                None if delta > 0 => 0,
                None => (n - 1) as usize,
            });
        }
    }

    /// Drop the context menu if it belongs to `client` (its owner disconnected).
    pub fn close_menu_of(&mut self, client: ClientId) {
        if self.menu.as_ref().is_some_and(|m| m.owner == client) {
            self.menu = None;
        }
    }

    /// Scroll a specific terminal by `lines` (positive = up / older).
    fn scroll_term(&self, term: &TerminalId, lines: i32) {
        if let Some(pt) = self.panes.get(term) {
            pt.scroll(lines);
        }
    }

    fn scroll_term_to_bottom(&self, term: &TerminalId) {
        if let Some(pt) = self.panes.get(term) {
            pt.scroll_to_bottom();
        }
    }

    /// On-screen rows of a terminal (the page size for page scrolling).
    fn term_rows(&self, term: &TerminalId) -> u16 {
        self.layout()
            .into_iter()
            .find(|r| &r.terminal == term)
            .map(|r| r.rows)
            .unwrap_or(24)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        // reflow derives the content-area viewport (minus chrome) and pushes it.
        self.reflow();
    }

    // --- sidebar + popup ---

    /// Toggle the neovim-style left panel (honored only when wide enough). The
    /// pane content area shrinks/grows, so resize the PTYs to match.
    fn toggle_sidebar(&mut self) {
        self.sidebar = !self.sidebar;
        // The content width changes → re-derive the viewport, not just PTY sizes.
        self.reflow();
    }

    /// Open the `Ctrl-f` fuzzy switcher (Sessions tab, empty filter, first row selected).
    fn open_popup(&mut self) {
        self.popup = Some(PopupState {
            tab: PopupTab::Sessions,
            filter: String::new(),
            sel: 0,
        });
    }

    /// The switcher rows for `tab` matching `filter`: SESSIONS (name + git branch) or
    /// AGENTS (tool · status · space), fuzzy-filtered (subsequence, case-insensitive).
    /// Left/Right switch the tab (owner's tmux `Ctrl-f` session switch + `prefix g` agents).
    fn popup_items(&self, tab: PopupTab, filter: &str) -> Vec<PopupRow> {
        let mut rows = Vec::new();
        match tab {
            PopupTab::Sessions => {
                for wid in self.sorted_session_ids() {
                    let name = self
                        .state
                        .workspace(&wid)
                        .and_then(|w| w.name.clone())
                        .unwrap_or_else(|| wid.to_string());
                    let branch = self.branches.get(&wid).cloned().unwrap_or_default();
                    let is_active = wid == self.ws;
                    let color = if is_active {
                        CAT_MAUVE
                    } else if self.session_has_agent(&wid) {
                        CAT_PEACH
                    } else {
                        CAT_OVERLAY
                    };
                    let text = if branch.is_empty() {
                        name.clone()
                    } else {
                        format!("{name}  ({branch})")
                    };
                    if fuzzy_match(filter, &format!("{name} {branch}")) {
                        rows.push(PopupRow {
                            target: PopupTarget::Session(wid),
                            glyph: "▪",
                            color,
                            text,
                        });
                    }
                }
            }
            PopupTab::Agents => {
                for row in self.agent_rows() {
                    let (glyph, color) = match row.status {
                        "working" => ("●", CAT_GREEN),
                        "ready" => ("◐", CAT_BLUE),
                        "blocked" => ("▲", CAT_YELLOW),
                        _ => ("○", CAT_OVERLAY),
                    };
                    // With a custom tab name the title differs from the space — show both
                    // (the switcher is wide enough), and let the filter match either.
                    let place = if row.title == row.space {
                        row.space.clone()
                    } else {
                        format!("{} · {}", row.space, row.title)
                    };
                    if fuzzy_match(
                        filter,
                        &format!("{} {} {} {}", row.space, row.title, row.tool, row.status),
                    ) {
                        rows.push(PopupRow {
                            target: PopupTarget::Agent(row.term),
                            glyph,
                            color,
                            text: format!("{} · {}  ({place})", row.tool, row.status),
                        });
                    }
                }
            }
        }
        rows
    }

    /// Keep the switcher selection in range as its (filtered) row set changes. Returns
    /// whether it moved (so the render loop repaints). Never closes on an empty result —
    /// an over-narrow filter just shows the empty state.
    pub fn reconcile_popup(&mut self) -> bool {
        let Some((tab, filter)) = self.popup.as_ref().map(|p| (p.tab, p.filter.clone())) else {
            return false;
        };
        let n = self.popup_items(tab, &filter).len();
        if let Some(p) = &mut self.popup {
            let clamped = if n == 0 { 0 } else { p.sel.min(n - 1) };
            if clamped != p.sel {
                p.sel = clamped;
                return true;
            }
        }
        false
    }

    /// Switch the switcher tab (Left/Right), resetting the selection to the top.
    fn popup_tab(&mut self, to: PopupTab) {
        if let Some(p) = &mut self.popup {
            p.tab = to;
            p.sel = 0;
        }
    }

    /// Move the switcher selection by `delta`, wrapping over the filtered rows.
    fn popup_move(&mut self, delta: i32) {
        let Some((tab, filter)) = self.popup.as_ref().map(|p| (p.tab, p.filter.clone())) else {
            return;
        };
        let n = self.popup_items(tab, &filter).len() as i32;
        if n == 0 {
            return;
        }
        if let Some(p) = &mut self.popup {
            p.sel = (p.sel as i32 + delta).rem_euclid(n) as usize;
        }
    }

    /// Append `c` to the switcher filter (resets the selection to the top).
    fn popup_type(&mut self, c: char) {
        if let Some(p) = &mut self.popup {
            p.filter.push(c);
            p.sel = 0;
        }
    }

    /// Delete the last filter char.
    fn popup_backspace(&mut self) {
        if let Some(p) = &mut self.popup {
            p.filter.pop();
            p.sel = 0;
        }
    }

    /// Act on the selected row (switch session / jump to agent) and close the switcher.
    /// With no matching row (over-narrow filter) Enter is a no-op — the switcher stays
    /// open showing the empty state rather than closing on nothing.
    fn popup_select(&mut self) {
        let Some(p) = self.popup.as_ref() else {
            return;
        };
        let items = self.popup_items(p.tab, &p.filter);
        let Some(row) = items.get(p.sel) else {
            return; // no valid row → keep the switcher open
        };
        let target = row.target.clone();
        self.popup = None; // resolved a target — close now
        match target {
            PopupTarget::Session(wid) => self.switch_session(wid),
            PopupTarget::Agent(term) => self.jump_to_terminal(&term),
        }
    }

    /// Rename the SELECTED session from the switcher (Sessions tab): switch to it and open
    /// the inline rename prompt. No-op on the Agents tab or an empty selection.
    fn popup_rename(&mut self) {
        let Some(p) = self.popup.as_ref() else {
            return;
        };
        if p.tab != PopupTab::Sessions {
            return;
        }
        let items = self.popup_items(p.tab, &p.filter);
        let Some(PopupRow {
            target: PopupTarget::Session(wid),
            ..
        }) = items.get(p.sel)
        else {
            return;
        };
        let wid = wid.clone();
        self.popup = None;
        self.switch_session(wid);
        self.open_rename_prompt();
    }

    // --- sidebar keyboard focus (Ctrl-b e) ---

    /// Focus the sidebar for keyboard nav, revealing it if hidden. Falls back to the
    /// `Ctrl-f` switcher when the sidebar can't show (too narrow / mobile).
    fn open_sidebar_focus(&mut self) {
        if !self.sidebar {
            self.sidebar = true;
            self.reflow();
        }
        if !self.sidebar_visible() {
            self.open_popup();
            return;
        }
        self.sidebar_focus = Some(SidebarFocus {
            tab: PopupTab::Sessions,
            sel: 0,
        });
    }

    fn sidebar_focus_len(&self, tab: PopupTab) -> usize {
        match tab {
            PopupTab::Sessions => self.sorted_session_ids().len(),
            PopupTab::Agents => self.agent_rows().len(),
        }
    }

    fn sidebar_focus_move(&mut self, delta: i32) {
        let Some(tab) = self.sidebar_focus.as_ref().map(|f| f.tab) else {
            return;
        };
        let n = self.sidebar_focus_len(tab) as i32;
        if n == 0 {
            return;
        }
        if let Some(f) = &mut self.sidebar_focus {
            f.sel = (f.sel as i32 + delta).rem_euclid(n) as usize;
        }
    }

    fn sidebar_focus_tab(&mut self, to: PopupTab) {
        if let Some(f) = &mut self.sidebar_focus {
            f.tab = to;
            f.sel = 0;
        }
    }

    /// Act on the focused sidebar row (switch session / jump to agent) and exit focus.
    fn sidebar_focus_select(&mut self) {
        let Some(f) = self.sidebar_focus.take() else {
            return;
        };
        match f.tab {
            PopupTab::Sessions => {
                if let Some(wid) = self.sorted_session_ids().get(f.sel).cloned() {
                    self.switch_session(wid);
                }
            }
            PopupTab::Agents => {
                if let Some(term) = self.agent_rows().get(f.sel).map(|r| r.term.clone()) {
                    self.jump_to_terminal(&term);
                }
            }
        }
    }

    // --- notification center (Ctrl-b a) ---

    fn open_center(&mut self) {
        self.center = Some(0);
    }

    /// Keep the center selection valid as notifications arrive/dismiss (called each
    /// frame). Stays open (empty state) when the log is empty.
    pub fn reconcile_center(&mut self) -> bool {
        let before = self.center;
        if let Some(sel) = self.center {
            let n = self.notifications.len();
            if n > 0 && sel >= n {
                self.center = Some(n - 1);
            }
        }
        before != self.center
    }

    fn center_move(&mut self, delta: i32) {
        let n = self.notifications.len() as i32;
        if n == 0 {
            return;
        }
        if let Some(sel) = self.center {
            self.center = Some((sel as i32 + delta).rem_euclid(n) as usize);
        }
    }

    fn center_dismiss(&mut self) {
        if let Some(sel) = self.center
            && sel < self.notifications.len()
        {
            self.notifications.remove(sel);
            let n = self.notifications.len();
            if n > 0 && sel >= n {
                self.center = Some(n - 1);
            }
        }
    }

    /// Jump to the selected notification's source pane (session + tab + focus) and
    /// close the center.
    fn center_jump(&mut self) {
        if let Some(sel) = self.center
            && let Some(note) = self.notifications.get(sel).cloned()
        {
            self.center = None;
            if let Some((wid, tab, pane)) = self.locate_terminal(&note.terminal) {
                self.switch_session(wid.clone());
                let _ = self.state.apply(Command::SelectTab {
                    origin: Origin::Client(self.client),
                    workspace: wid,
                    tab,
                });
                let _ = self.state.apply(Command::FocusPane {
                    client: self.client,
                    pane,
                });
                self.reflow();
            }
        }
    }

    /// Close panes whose shell has exited (in ANY tab). Returns true if the app is
    /// now empty (last pane of the last tab exited → quit). A tab whose last pane
    /// exits is closed whole; other tabs keep running in the background.
    /// Reap any exited shells, collapsing panes/tabs/sessions as needed. Returns
    /// whether it CHANGED visible state (so the render loop repaints the removed pane);
    /// the caller checks [`is_empty`](Self::is_empty) separately to decide whether to quit.
    pub fn reap_exited(&mut self) -> bool {
        let exited: Vec<TerminalId> = self
            .panes
            .iter()
            .filter(|(_, p)| p.has_exited())
            .map(|(id, _)| id.clone())
            .collect();
        // Fast path: nothing exited this tick — don't reflow (which would bump the
        // workspace rev + resize every PTY) on every idle main-loop iteration.
        if exited.is_empty() {
            return false;
        }
        for term in exited {
            let Some((wid, tab_id, pane)) = self.locate_terminal(&term) else {
                // No longer in any session/tab (already collapsed) — drop the runtime.
                self.panes.remove(&term);
                continue;
            };
            // Read the tab/session shape of the OWNING session (may be a background
            // one), using Api-origin mutations since the client controls only the
            // active session.
            let (tab_is_single, ws_tab_count) = self
                .state
                .workspace(&wid)
                .map(|w| {
                    let single = w
                        .tab(&tab_id)
                        .map(|t| t.layout.is_single_leaf())
                        .unwrap_or(true);
                    (single, w.tabs.len())
                })
                .unwrap_or((true, 0));

            if !tab_is_single {
                // The tab has other panes — collapse just this one.
                match self.state.apply(Command::ClosePane {
                    origin: Origin::Api,
                    workspace: wid,
                    pane,
                    if_rev: None,
                }) {
                    Ok(evs) => {
                        for e in &evs {
                            if let Event::PaneClosed { terminal, .. } = e {
                                self.panes.remove(terminal);
                            }
                        }
                        // Reflow the OWNING session's survivors (works for background
                        // sessions, which `reflow()` below would otherwise miss).
                        self.apply_resized_events(&evs);
                    }
                    Err(_) => {
                        self.panes.remove(&term);
                    }
                }
            } else if ws_tab_count > 1 {
                // Last pane of this tab, but other tabs survive — close the tab.
                match self.state.apply(Command::CloseTab {
                    origin: Origin::Api,
                    workspace: wid,
                    tab: tab_id,
                }) {
                    Ok(evs) => {
                        for e in &evs {
                            if let Event::TabClosed { terminals, .. } = e {
                                for t in terminals {
                                    self.panes.remove(t);
                                }
                            }
                        }
                        // Reflow the owning session's now-active tab (background-safe).
                        self.apply_resized_events(&evs);
                    }
                    Err(_) => {
                        self.panes.remove(&term);
                    }
                }
            } else if self.session_count() > 1 {
                // Last pane of the last tab of a NON-last session — drop the whole
                // session; if it was the active one, switch to another.
                let was_active = self.ws == wid;
                match self.state.remove_workspace(&wid) {
                    Some(terms) => {
                        for t in &terms {
                            self.panes.remove(t);
                        }
                        if was_active
                            && let Some(next) = self.state.workspace_ids().into_iter().next()
                        {
                            self.switch_session(next);
                        }
                    }
                    None => {
                        self.panes.remove(&term);
                    }
                }
            } else {
                // Last pane / last tab / only session — its exit means "quit": drop it
                // so the app becomes empty and the main loop terminates.
                self.panes.remove(&term);
            }
        }
        // Reaping may have closed a tab or session (changing chrome) → re-derive.
        self.reflow();
        true
    }

    // --- render ---

    /// Render the whole composite (panes + dividers + sidebar + tab bar + popup)
    /// into `buf`, returning the desired cursor position (or `None` when a popup
    /// captures input). Rendering targets a plain `Buffer` — not a `Frame` — so the
    /// same code path serves both the local terminal and the (headless) server that
    /// ships cell diffs to a remote client.
    pub fn render_to(&self, buf: &mut Buffer) -> Option<Position> {
        let area = buf.area;
        let rects = self.layout();
        let focused_term = self.focused_terminal();
        let mut cursor_pos: Option<Position> = None;

        // Rebuild clickable-chrome hit regions from scratch each frame (tab chips +
        // sidebar rows push into this as they render).
        self.click_zones.borrow_mut().clear();

        // 1) pane contents (inactive panes dimmed so the focused one stands out).
        for rect in &rects {
            let Some(pt) = self.panes.get(&rect.terminal) else {
                continue;
            };
            let snap = pt.snapshot();
            let is_focused = Some(&rect.terminal) == focused_term.as_ref();
            // Drag-selection highlight: only the pane the drag started in, once it has moved.
            // Normalize the start the SAME way extraction does so the tint matches the copy.
            let sel_span = self
                .selection
                .as_ref()
                .filter(|s| s.moved && s.term == rect.terminal)
                .map(|s| {
                    let (start, end) = sel_bounds(s.anchor, s.head);
                    (sel_start_norm(&snap, start), end)
                });
            for (ry, row) in snap.cells.iter().enumerate() {
                if ry as u16 >= rect.rows {
                    break;
                }
                let y = rect.y + ry as u16;
                if y >= area.height {
                    break;
                }
                // The selected column span for this row (if any), computed once per row.
                let row_sel = sel_span.and_then(|(a, b)| sel_cols(a, b, rect.cols, ry as u16));
                for (rx, cell) in row.iter().enumerate() {
                    if rx as u16 >= rect.cols {
                        break;
                    }
                    let x = rect.x + rx as u16;
                    if x >= area.width {
                        break;
                    }
                    if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                        if cell.spacer {
                            bc.set_skip(true);
                            continue;
                        }
                        let mut style =
                            Style::default().fg(to_color(cell.fg)).bg(to_color(cell.bg));
                        if cell.bold {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if cell.reverse {
                            style = style.add_modifier(Modifier::REVERSED);
                        }
                        if !is_focused {
                            style = style.add_modifier(Modifier::DIM);
                        }
                        // A selected cell gets the selection background (the leading cell of a
                        // wide glyph tints both of its columns). Overrides the cell's own bg.
                        if row_sel.is_some_and(|(c0, c1)| rx as u16 >= c0 && rx as u16 <= c1) {
                            style = style.bg(SEL_BG);
                        }
                        bc.set_symbol(&cell.sym);
                        bc.set_style(style);
                    }
                }
            }
            if is_focused {
                let (cx, cy) = snap.cursor;
                if cx < rect.cols && cy < rect.rows {
                    let (ax, ay) = (rect.x + cx, rect.y + cy);
                    if ax < area.width && ay < area.height {
                        cursor_pos = Some(Position::new(ax, ay));
                    }
                }
            }
        }

        // 2) dividers in the 1-cell gaps between panes; accent the focused pane's.
        let mut covered = vec![vec![false; area.width as usize]; area.height as usize];
        let mut focus_rect: Option<PaneRect> = None;
        for rect in &rects {
            if Some(&rect.terminal) == focused_term.as_ref() {
                focus_rect = Some(rect.clone());
            }
            for yy in rect.y..(rect.y + rect.rows).min(area.height) {
                for xx in rect.x..(rect.x + rect.cols).min(area.width) {
                    covered[yy as usize][xx as usize] = true;
                }
            }
        }
        let content_x = self.content_x();
        let content_rows = self.content_rows();
        for yy in 0..area.height {
            for xx in 0..area.width {
                // The sidebar owns the left strip and the status bar owns the bottom
                // row; don't paint dividers over either.
                if yy >= content_rows || xx < content_x || covered[yy as usize][xx as usize] {
                    continue;
                }
                let left = xx > 0 && covered[yy as usize][(xx - 1) as usize];
                let right = (xx + 1) < area.width && covered[yy as usize][(xx + 1) as usize];
                let glyph = if left || right { "│" } else { "─" };
                let accent = focus_rect
                    .as_ref()
                    .map(|fr| divider_touches(fr, xx, yy))
                    .unwrap_or(false);
                let color = if accent { Color::Cyan } else { Color::DarkGray };
                if let Some(bc) = buf.cell_mut(Position::new(xx, yy)) {
                    bc.set_symbol(glyph);
                    bc.set_style(Style::default().fg(color));
                }
            }
        }

        // 3) the neovim-style left panel, over the reserved strip.
        if self.sidebar_visible() {
            self.render_sidebar(buf);
        }

        // 4) the always-on bottom status bar (session · tabs · scroll/agents/clock/host).
        self.render_status_bar(buf);

        // 5) the Ctrl-f switcher popup / notification center, over everything.
        if self.popup.is_some() {
            self.render_popup(buf);
        }
        if self.center.is_some() {
            self.render_center(buf);
        }
        if self.prompt.is_some() {
            self.render_prompt(buf);
        }
        if self.confirm.is_some() {
            self.render_confirm(buf);
        }
        if self.menu.is_some() {
            self.render_menu(buf);
        }

        // The shell cursor shows only when nothing modal is capturing input.
        if self.popup.is_none()
            && self.scroll_pane.is_none()
            && self.center.is_none()
            && self.prompt.is_none()
            && self.confirm.is_none()
            && self.menu.is_none()
            && self.sidebar_focus.is_none()
        {
            cursor_pos
        } else {
            None
        }
    }

    /// The current viewport size (terminal size the app renders at).
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Refresh foreground-process labels at most ~2 Hz (throttled internally), so
    /// the server loop can call it every tick without hammering `ps`.
    /// Read+clear the dirty flag of EVERY hosted pane (must drain all, not short-circuit,
    /// so no pane's pending change is lost), returning whether any pane's screen changed
    /// since the last frame. The render loop ORs this into its dirty decision.
    pub fn drain_pane_dirty(&self) -> bool {
        let mut dirty = false;
        for pt in self.panes.values() {
            if pt.take_dirty() {
                dirty = true;
            }
        }
        dirty
    }

    /// Poll the VISIBLE panes' alternate-screen bits and report whether any just LEFT it
    /// (true→false) — a full-screen app (nvim, less, htop, …) exited. The render loop reacts
    /// by forcing a full repaint so the restored primary screen is clean: during the app's
    /// life the client's unicode-width render can desync from alacritty's grid on wide
    /// graphemes (e.g. VS16 emoji), and the alt→primary grid swap is where that residue shows;
    /// only a full frame guarantees a clean screen. This updates the stored per-pane state.
    ///
    /// Scoped to the active tab's panes (a background pane toggling alt-screen doesn't affect
    /// the composed frame; when its tab is next shown the whole buffer recomposes anyway).
    /// Detection is a per-render poll — alacritty applies `?1049h`/`?1049l` on its own reader
    /// thread with no callback we can hook — so an app that enters AND exits within a single
    /// render interval (e.g. `nvim +q` from a script) can be missed. That isn't the interactive
    /// full-screen case the user hits, and the wholesale content change usually clears it.
    pub fn take_alt_screen_exit(&mut self) -> bool {
        let visible: Vec<(TerminalId, bool)> = self
            .layout()
            .iter()
            .map(|r| {
                let alt = self
                    .panes
                    .get(&r.terminal)
                    .is_some_and(|p| p.in_alt_screen());
                (r.terminal.clone(), alt)
            })
            .collect();
        detect_alt_screen_exit(&mut self.alt_screen, &visible)
    }

    /// Wall-clock minute-of-day (`hour*60 + minute`), for the render loop to force a
    /// repaint when the status-bar `HH:MM` rolls over while otherwise idle.
    pub fn clock_minute(&self) -> u32 {
        local_minute()
    }

    /// Build the on-disk snapshot of the current session layout (names + tabs + BSP split
    /// trees + per-leaf cwd). Read-only — safe to call on the render loop for autosave.
    pub fn snapshot(&self) -> persist::PersistState {
        let mut sessions = Vec::new();
        let mut active_session = 0;
        for wid in self.state.workspace_ids() {
            let Some(w) = self.state.workspace(&wid) else {
                continue;
            };
            if wid == self.ws {
                active_session = sessions.len(); // index in the vec we're building
            }
            let mut tabs = Vec::new();
            let mut active_tab = 0;
            for t in &w.tabs {
                if t.id == w.active_tab {
                    active_tab = tabs.len();
                }
                tabs.push(persist::PTab {
                    name: t.name.clone(),
                    layout: self.layout_to_persist(&t.layout),
                });
            }
            sessions.push(persist::PSession {
                name: w.name.clone(),
                active_tab,
                tabs,
            });
        }
        persist::PersistState {
            version: persist::SCHEMA_VERSION,
            saved_at: now_secs(),
            active_session,
            sessions,
        }
    }

    /// Convert a live `SplitTree` to its persisted form, reading each leaf's shell cwd and
    /// (for a whitelisted foreground program, e.g. an agent) its command to re-run.
    fn layout_to_persist(&self, tree: &SplitTree) -> PLayout {
        match tree {
            SplitTree::Leaf { terminal, .. } => {
                // Non-UTF-8 cwd → None (that pane restores to $HOME); the save never fails.
                let cwd = self
                    .panes
                    .get(terminal)
                    .and_then(|p| p.pid())
                    .and_then(procinfo::process_cwd)
                    .and_then(|p| p.to_str().map(|s| s.to_string()));
                // Save the running command's argv only when its basename is whitelisted
                // (`restore_processes`, default = the AI agents), so restore re-runs it.
                // Capped at SAVE time (argv count + total length) so an absurd command can't
                // bloat the snapshot past its size limit and get the whole session rejected.
                let command = self.labels.get(terminal).and_then(|label| {
                    let whitelisted = self
                        .cfg
                        .restore_processes
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(&label.text));
                    if !whitelisted {
                        return None;
                    }
                    let mut argv = procinfo::process_command(label.pid)?;
                    // Resume the live conversation instead of re-running the agent fresh:
                    // rebuild the command to resume the agent's current session. Gated on the
                    // pid still hosting the same agent we labeled (guards a pid reused between
                    // the process-tree snapshot and this read) so we never splice a session id
                    // onto an unrelated process's argv.
                    if self.cfg.restore_agent_sessions
                        && argv
                            .first()
                            .map(|p| p.rsplit('/').next().unwrap_or(p))
                            .is_some_and(|base| base.eq_ignore_ascii_case(&label.text))
                        && let Some(id) = agentstate::agent_session_id(&label.text, label.pid)
                    {
                        argv = agentstate::resume_argv(&label.text, &argv, &id);
                    }
                    let total: usize = argv.iter().map(|a| a.len() + 1).sum();
                    if argv.is_empty() || argv.len() > 64 || total > persist::MAX_COMMAND_LEN {
                        return None;
                    }
                    Some(argv)
                });
                PLayout::Leaf { cwd, command }
            }
            SplitTree::Branch {
                dir,
                ratio,
                first,
                second,
            } => PLayout::Branch {
                dir: *dir,
                ratio: *ratio,
                first: Box::new(self.layout_to_persist(first)),
                second: Box::new(self.layout_to_persist(second)),
            },
        }
    }

    /// Returns whether any chrome-affecting data (labels / agent statuses / branches)
    /// actually CHANGED, so the render loop only recomposes when the sidebar/status bar
    /// would differ (throttled to ~2 Hz; unchanged refreshes are free of a repaint).
    pub fn maybe_refresh_labels(&mut self) -> bool {
        if self.last_labels.elapsed() >= Duration::from_millis(500) {
            let a = self.refresh_labels();
            let b = self.refresh_agent_statuses();
            let c = self.refresh_branches();
            let d = self.refresh_usage();
            let e = self.refresh_version();
            a || b || c || d || e
        } else {
            false
        }
    }

    /// Number of carousel pages the usage readout currently has (0 when hidden,
    /// inline, or nothing visible). Recomputed from the live snapshot + config —
    /// cheap (a handful of windows) and always consistent with what renders.
    fn usage_page_count(&self) -> usize {
        // Mirror the render gate exactly (cols >= 100, not Off, paged, something
        // visible) so auto-rotate/manual paging only act when the carousel actually
        // shows — a hidden readout must not force needless repaints.
        if self.cfg.usage_layout != UsageLayout::Paged
            || self.cfg.usage == UsageStyle::Off
            || self.cols < 100
        {
            return 0;
        }
        match &self.usage_shown {
            Some(u) if u.has_visible(self.cfg.usage_windows) => u
                .pages(
                    self.cfg.usage_windows,
                    self.cfg.usage_page_unit,
                    self.cfg.usage_reset,
                )
                .len(),
            _ => 0,
        }
    }

    /// Move the usage carousel by `delta` pages, wrapping. No-op when there's 0 or 1
    /// page (nothing to page through). Resets the auto-rotate timer so a manual page
    /// isn't immediately followed by an auto one.
    fn page_usage(&mut self, delta: i32) {
        let n = self.usage_page_count();
        if n <= 1 {
            return;
        }
        self.usage_page = wrap_page(self.usage_page, delta, n);
        self.usage_rolled_at = std::time::Instant::now();
    }

    /// Auto-rotate hook, called every render tick (~30 fps) by the server loop.
    /// Advances the carousel one page when `usage_rotate_secs` has elapsed since the
    /// last change AND there's more than one page; returns whether it advanced (→
    /// repaint). Off (`usage_rotate_secs = 0`), single-page, and non-paged layouts
    /// never roll. The cheap interval gate runs first so the hot path avoids
    /// recomputing the page list every tick.
    pub fn maybe_auto_roll_usage(&mut self) -> bool {
        if !usage_should_roll(self.cfg.usage_rotate_secs, self.usage_rolled_at.elapsed()) {
            return false;
        }
        let n = self.usage_page_count();
        // The interval elapsed: reset the timer regardless, so a 1-page readout
        // doesn't recompute the page list every single tick once it's due.
        self.usage_rolled_at = std::time::Instant::now();
        if n <= 1 {
            return false;
        }
        self.usage_page = wrap_page(self.usage_page, 1, n);
        true
    }

    /// Start the background usage/limits poller (server-only; see `usagepoll`).
    /// Replaces the idle handle installed by `new` with a live one. Skipped when
    /// `usage = "off"` so a disabled readout costs no `coctl`/network polling
    /// (matching `COPAD_MUX_USAGE=0`).
    pub fn start_usage_poll(&mut self) {
        if self.cfg.usage == UsageStyle::Off {
            return;
        }
        self.usage_poll = usagepoll::spawn();
    }

    /// Start the background GitHub-release update checker (server-only; see
    /// `versionpoll`). Skipped when `update_check = false` so a disabled check
    /// costs no network polling (matching `COPAD_MUX_UPDATE_CHECK=0`).
    pub fn start_version_poll(&mut self) {
        if !self.cfg.update_check {
            return;
        }
        self.version_poll = versionpoll::spawn();
    }

    /// Fold the latest polled usage snapshot into `usage_shown`; returns whether it
    /// changed (so the loop repaints the status bar). Poison-safe read.
    fn refresh_usage(&mut self) -> bool {
        let current = self.usage_poll.lock().map(|g| g.clone()).unwrap_or(None);
        if current != self.usage_shown {
            self.usage_shown = current;
            true
        } else {
            false
        }
    }

    /// Fold the latest polled update hint into `version_shown`; returns whether it
    /// changed (so the loop repaints the status bar). Poison-safe read.
    fn refresh_version(&mut self) -> bool {
        let current = self.version_poll.lock().map(|g| g.clone()).unwrap_or(None);
        if current != self.version_shown {
            self.version_shown = current;
            true
        } else {
            false
        }
    }

    /// Recompute each session's git branch from its focused pane's shell cwd (for the
    /// `spaces` subtitle). Rebuilt each pass so a `cd` out of a repo clears it.
    fn refresh_branches(&mut self) -> bool {
        let mut next = HashMap::new();
        for wid in self.state.workspace_ids() {
            let branch = self
                .state
                .workspace(&wid)
                .and_then(|w| w.tab(&w.active_tab))
                .and_then(|t| t.layout.terminal_of(&t.focused).cloned())
                .and_then(|tid| self.panes.get(&tid))
                .and_then(|pane| pane.pid())
                .and_then(procinfo::process_cwd)
                .and_then(|cwd| gitinfo::branch(&cwd));
            if let Some(branch) = branch {
                next.insert(wid, branch);
            }
        }
        let changed = next != self.branches;
        self.branches = next;
        changed
    }

    /// Recompute each agent pane's status (working/ready/blocked/idle) from Claude's
    /// session file + a screen-text fallback (`agentstate`). Agent panes only; the map
    /// is rebuilt each pass so closed/reclassified panes drop out.
    fn refresh_agent_statuses(&mut self) -> bool {
        use agentstate::AgentStatus;
        let mut next = HashMap::new();
        // Meaningful status TRANSITIONS to notify on (fired after the borrow ends).
        let mut events: Vec<(TerminalId, String, &'static str)> = Vec::new();
        for (tid, pane) in &self.panes {
            let Some(label) = self.labels.get(tid) else {
                continue;
            };
            if label.kind != procinfo::Kind::Agent {
                continue;
            }
            let status = agentstate::resolve(Some(label.pid), &pane.snapshot());
            let body = match (self.agent_statuses.get(tid).copied(), status) {
                // turn just finished (was running, now parked at the prompt)
                (Some(AgentStatus::Working), AgentStatus::Ready) => Some("turn finished"),
                // just started blocking — only a real transition (require a KNOWN old
                // status, so a server restart that first observes `Blocked` is silent).
                (Some(old), AgentStatus::Blocked) if old != AgentStatus::Blocked => {
                    Some("awaiting input")
                }
                _ => None,
            };
            if let Some(body) = body {
                events.push((tid.clone(), label.text.clone(), body));
            }
            next.insert(tid.clone(), status);
        }
        let changed = next != self.agent_statuses;
        self.agent_statuses = next;

        // Fire desktop toasts (best-effort, non-blocking) — the server does this, so
        // they arrive even while detached. Replaces the retired `~/.claude` notify hooks.
        for (tid, tool, body) in events {
            let space = self
                .locate_terminal(&tid)
                .map(|(w, _, _)| {
                    self.state
                        .workspace(&w)
                        .and_then(|ws| ws.name.clone())
                        .unwrap_or_else(|| w.to_string())
                })
                .unwrap_or_default();
            // Desktop toast: env override wins, else the config `notify` flag. The
            // in-app center below is logged regardless (it's not a desktop toast).
            if notify::env_override().unwrap_or(self.cfg.notify) {
                notify::desktop(&format!("{tool} · {space}"), body);
            }
            // Also log it in the notification center (Ctrl-b a).
            let kind = if body == "awaiting input" {
                "blocked"
            } else {
                "done"
            };
            self.notifications.push_front(Notification {
                when: local_hhmm(),
                kind,
                tool,
                space,
                body,
                terminal: tid,
            });
        }
        while self.notifications.len() > NOTIFY_LOG_CAP {
            self.notifications.pop_back();
        }
        changed
    }

    /// Number of agent panes currently BLOCKED (awaiting input) — the status-bar
    /// attention count (`⚑N`), replacing the retired tmux `⚑N` chip.
    fn attention_count(&self) -> usize {
        self.agent_statuses
            .values()
            .filter(|s| **s == agentstate::AgentStatus::Blocked)
            .count()
    }

    /// An agent pane's rolled-up status label for the sidebar (`idle` if unknown).
    fn agent_status(&self, tid: &TerminalId) -> &'static str {
        self.agent_statuses
            .get(tid)
            .map(|s| s.label())
            .unwrap_or("idle")
    }

    /// A one-line label for a pane row (index + short cwd basename if available).
    /// Refresh every pane's foreground-process label from a single `ps` sweep
    /// (throttled by the caller to ~2 Hz — never per frame).
    fn refresh_labels(&mut self) -> bool {
        let tree = procinfo::ProcTree::snapshot();
        let mut next = HashMap::new();
        for (tid, pane) in &self.panes {
            // The terminal's foreground process GROUP is the real foreground
            // command (resolve a live member — the pgid leader may have exited in
            // a pipeline); fall back to descending from the shell pid.
            let label = pane
                .foreground_pgid()
                .and_then(|pg| tree.command_of_pgroup(pg))
                .or_else(|| pane.pid().and_then(|p| tree.foreground(p)));
            if let Some(label) = label {
                next.insert(tid.clone(), label);
            }
        }
        let changed = next != self.labels;
        self.labels = next;
        self.last_labels = std::time::Instant::now();
        changed
    }

    /// The terminal id backing the pane at list index `idx`.
    fn terminal_at(&self, idx: usize) -> Option<TerminalId> {
        let pane = self.pane_order().get(idx)?.clone();
        let w = self.state.workspace(&self.ws)?;
        let tab = w.tab(&w.active_tab)?;
        tab.layout.terminal_of(&pane).cloned()
    }

    /// The classified label for the pane at list index `idx`, if known.
    fn pane_label_at(&self, idx: usize) -> Option<&procinfo::Label> {
        let term = self.terminal_at(idx)?;
        self.labels.get(&term)
    }

    /// Display text for the pane at list index `idx`: its foreground command
    /// (agent / shell / program), or a fallback before the first label sweep.
    fn pane_label(&self, idx: usize) -> String {
        self.pane_label_at(idx)
            .map(|l| l.text.clone())
            .unwrap_or_else(|| format!("pane {idx}"))
    }

    /// The foreground command of a session's focused pane (its "what's happening"
    /// subtitle), e.g. `claude` / `zsh` / `nvim`.
    fn session_focus_label(&self, wid: &WorkspaceId) -> String {
        let Some(w) = self.state.workspace(wid) else {
            return String::new();
        };
        let Some(t) = w.tab(&w.active_tab) else {
            return String::new();
        };
        t.layout
            .terminal_of(&t.focused)
            .and_then(|tid| self.labels.get(tid))
            .map(|l| l.text.clone())
            .unwrap_or_default()
    }

    /// Every agent pane across ALL sessions. The terminal id lets the sidebar make
    /// each agent row clickable (jump to its pane).
    fn agent_rows(&self) -> Vec<AgentRow> {
        let mut out = Vec::new();
        for wid in self.state.workspace_ids() {
            let Some(w) = self.state.workspace(&wid) else {
                continue;
            };
            let space = w.name.clone().unwrap_or_else(|| wid.to_string());
            for t in &w.tabs {
                // The tab's custom name titles its agent rows, so several agents in ONE
                // space stay distinguishable (the reason tab rename exists).
                let tab_name = t.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
                for p in t.layout.panes() {
                    if let Some(tid) = t.layout.terminal_of(&p)
                        && let Some(label) = self.labels.get(tid)
                        && label.kind == procinfo::Kind::Agent
                    {
                        out.push(AgentRow {
                            title: tab_name.unwrap_or(&space).to_string(),
                            space: space.clone(),
                            tool: label.text.clone(),
                            status: self.agent_status(tid),
                            term: tid.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// The herdr-style left panel: `spaces` (sessions, top half) + `agents` (agent
    /// panes with status·tool, bottom half). Always on when wide enough.
    fn render_sidebar(&self, buf: &mut Buffer) {
        let h = self.content_rows(); // above the bottom status bar
        // Transparent fill (default terminal bg) so a configured background image shows
        // through the sidebar — the right-border `│` column is the only visual separator.
        let panel_bg = Color::Reset;
        let sidebar_w = self.sidebar_w();

        // Fill the strip + the right border column (down to the status bar).
        for y in 0..h {
            for x in 0..sidebar_w {
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(" ");
                    bc.set_skip(false);
                    bc.set_style(Style::default().bg(panel_bg));
                }
            }
            if let Some(bc) = buf.cell_mut(Position::new(sidebar_w, y)) {
                bc.set_symbol("│");
                bc.set_skip(false);
                bc.set_style(Style::default().fg(CAT_SURFACE0).bg(panel_bg));
            }
        }

        // Display-width-aware writer that ADVANCES `x` (so a colored dot and the name
        // after it don't overwrite each other). Clipped to the strip.
        let put = |buf: &mut Buffer, x: &mut u16, y: u16, s: &str, style: Style| {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if cw == 0 {
                    continue;
                }
                if *x + cw > sidebar_w {
                    break;
                }
                if let Some(bc) = buf.cell_mut(Position::new(*x, y)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_skip(false);
                    bc.set_style(style);
                }
                if cw == 2
                    && let Some(bc) = buf.cell_mut(Position::new(*x + 1, y))
                {
                    bc.set_symbol(" ");
                    bc.set_skip(true);
                    bc.set_style(style);
                }
                *x += cw;
            }
        };
        let header = Style::default()
            .fg(CAT_OVERLAY)
            .bg(panel_bg)
            .add_modifier(Modifier::BOLD);
        let sub_style = Style::default().fg(CAT_OVERLAY).bg(panel_bg);

        // ---- spaces (top half) ----
        let mid = (h / 2).max(2);
        put(buf, &mut 0, 0, " spaces", header);
        let sids = self.sorted_session_ids();
        let active_si = self.active_session_index();
        // When the sidebar has keyboard focus on Sessions, center the window on the FOCUSED
        // row (and highlight it) instead of the active session.
        let space_focus = match &self.sidebar_focus {
            Some(f) if f.tab == PopupTab::Sessions => Some(f.sel.min(sids.len().saturating_sub(1))),
            _ => None,
        };
        let space_center = space_focus.unwrap_or(active_si);
        // Window the list so the centered session stays visible with many sessions; reserve
        // a row for a "+N more" hint when truncated (the full list is in `Ctrl-f`). Only
        // window when there's room for at least a session + hint (≥2 slots); otherwise show
        // what fits with no fabricated/overlapping row.
        let space_max = (mid.saturating_sub(1) / 2) as usize; // 2 rows per session
        let space_total = sids.len();
        let (space_start, space_vis, space_trunc) = if space_total <= space_max || space_max < 2 {
            (0, space_max.min(space_total), false)
        } else {
            let vis = space_max - 1; // reserve one row for the "+N more" hint
            (list_window_start(space_total, space_center, vis), vis, true)
        };
        let mut y = 1u16;
        for (i, sid) in sids.iter().enumerate().skip(space_start).take(space_vis) {
            if y + 1 >= mid {
                break; // safety: never spill into the agents half
            }
            let is_active = i == active_si;
            let is_focused = space_focus == Some(i);
            let name = self
                .state
                .workspace(sid)
                .and_then(|w| w.name.clone())
                .unwrap_or_else(|| sid.to_string());
            let dot_color = if is_active {
                CAT_MAUVE
            } else if self.session_has_agent(sid) {
                CAT_PEACH
            } else {
                CAT_OVERLAY
            };
            let name_style = if is_focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(if is_active { CAT_TEXT } else { CAT_SUBTEXT })
                    .bg(panel_bg)
                    .add_modifier(if is_active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    })
            };
            let mut x = 1u16;
            put(
                buf,
                &mut x,
                y,
                "●",
                Style::default().fg(dot_color).bg(panel_bg),
            );
            put(buf, &mut x, y, " ", name_style);
            put(buf, &mut x, y, &name, name_style);
            // subtitle: git branch (herdr-style), falling back to the focused command
            // when the cwd isn't a git repo.
            let sub = self
                .branches
                .get(sid)
                .cloned()
                .unwrap_or_else(|| self.session_focus_label(sid));
            let mut sx = 4u16;
            put(buf, &mut sx, y + 1, &sub, sub_style);
            // Both rows (name + subtitle) click to switch to this session.
            self.click_zones.borrow_mut().push(ClickZone {
                x0: 0,
                x1: sidebar_w - 1,
                y0: y,
                y1: y + 1,
                target: ClickTarget::Session(sid.clone()),
            });
            y += 2;
        }
        if space_trunc {
            let hidden = space_total - space_vis;
            let mut hx = 1u16;
            put(
                buf,
                &mut hx,
                y,
                &format!("+{hidden} more · Ctrl-f"),
                sub_style,
            );
        }

        // ---- agents (bottom half) ----
        let mut y = mid;
        put(buf, &mut 0, y, " agents", header);
        y += 1;
        // Window agents (around the keyboard-focused row when focusing Agents, else the
        // top) with a "+M more" hint; the full list is in Ctrl-f.
        let agent_rows = self.agent_rows();
        let agent_max = (h.saturating_sub(mid + 1) / 2) as usize;
        let agent_total = agent_rows.len();
        let agent_focus = match &self.sidebar_focus {
            Some(f) if f.tab == PopupTab::Agents => Some(f.sel.min(agent_total.saturating_sub(1))),
            _ => None,
        };
        let (agent_vis, agent_trunc) = if agent_total <= agent_max || agent_max < 2 {
            (agent_max.min(agent_total), false)
        } else {
            (agent_max - 1, true) // reserve one row for the "+M more" hint
        };
        let agent_start = list_window_start(agent_total, agent_focus.unwrap_or(0), agent_vis);
        for (ai, row) in agent_rows
            .into_iter()
            .enumerate()
            .skip(agent_start)
            .take(agent_vis)
        {
            if y + 1 >= h {
                break;
            }
            let is_focused = agent_focus == Some(ai);
            // tmx-style glyph + colour per status (all width-1 geometric shapes).
            let (dot, scolor) = match row.status {
                "working" => ("●", CAT_GREEN),
                "ready" => ("◐", CAT_BLUE),
                "blocked" => ("▲", CAT_YELLOW),
                _ => ("○", CAT_OVERLAY), // idle
            };
            let mut x = 1u16;
            put(
                buf,
                &mut x,
                y,
                dot,
                Style::default().fg(scolor).bg(panel_bg),
            );
            let name_style = if is_focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(CAT_TEXT)
                    .bg(panel_bg)
                    .add_modifier(Modifier::BOLD)
            };
            put(buf, &mut x, y, " ", name_style);
            put(buf, &mut x, y, &row.title, name_style);
            let mut sx = 4u16;
            put(
                buf,
                &mut sx,
                y + 1,
                &format!("{} · {}", row.status, row.tool),
                sub_style,
            );
            // Both rows click to jump to this agent's pane.
            self.click_zones.borrow_mut().push(ClickZone {
                x0: 0,
                x1: sidebar_w - 1,
                y0: y,
                y1: y + 1,
                target: ClickTarget::Agent(row.term.clone()),
            });
            y += 2;
        }
        if agent_trunc && y < h {
            let hidden = agent_total - agent_vis;
            let mut hx = 1u16;
            put(
                buf,
                &mut hx,
                y,
                &format!("+{hidden} more · Ctrl-f"),
                sub_style,
            );
        }
    }

    /// The tab's CUSTOM display name (`Ctrl-b ,` / `comux rename-tab`), chip-truncated.
    /// Takes display precedence over the process label in every `tab_labels` style.
    fn tab_custom_name(&self, tab_id: &TabId) -> Option<String> {
        let w = self.state.workspace(&self.ws)?;
        let name = w.tab(tab_id)?.name.as_deref()?.trim();
        (!name.is_empty()).then(|| clip_chip(name))
    }

    /// The focused pane's foreground-command name for a tab (e.g. `claude` / `nvim`),
    /// truncated to a chip-friendly width and cleaned of a login-shell leading `-`.
    /// `None` when the tab / focused terminal has no resolved label yet, so the caller
    /// falls back to the bare index. Used for the `name`/`both` tab-label styles.
    fn tab_focus_label(&self, tab_id: &TabId) -> Option<String> {
        let w = self.state.workspace(&self.ws)?;
        let t = w.tab(tab_id)?;
        let tid = t.layout.terminal_of(&t.focused)?;
        let raw = self.labels.get(tid)?.text.trim();
        let name = raw.strip_prefix('-').unwrap_or(raw); // login shell `-zsh` → `zsh`
        if name.is_empty() {
            return None;
        }
        Some(clip_chip(name))
    }

    /// Does any pane in `tab_id` currently run a classified AI agent?
    fn tab_has_agent(&self, tab_id: &TabId) -> bool {
        let Some(w) = self.state.workspace(&self.ws) else {
            return false;
        };
        let Some(t) = w.tab(tab_id) else {
            return false;
        };
        t.layout.panes().iter().any(|p| {
            t.layout
                .terminal_of(p)
                .and_then(|tid| self.labels.get(tid))
                .map(|l| l.kind == procinfo::Kind::Agent)
                .unwrap_or(false)
        })
    }

    /// Number of agent panes across the ACTIVE session (all its tabs).
    fn active_session_agent_count(&self) -> usize {
        let Some(w) = self.state.workspace(&self.ws) else {
            return 0;
        };
        let mut n = 0;
        for t in &w.tabs {
            for p in t.layout.panes() {
                let is_agent = t
                    .layout
                    .terminal_of(&p)
                    .and_then(|tid| self.labels.get(tid))
                    .map(|l| l.kind == procinfo::Kind::Agent)
                    .unwrap_or(false);
                if is_agent {
                    n += 1;
                }
            }
        }
        n
    }

    /// The always-on bottom status bar (Catppuccin Mocha, matching the owner's tmux):
    /// LEFT = session pill + tab chips (active highlighted, agent `●`); RIGHT =
    /// scroll flag · agent count · clock · host.
    fn render_status_bar(&self, buf: &mut Buffer) {
        let area = buf.area;
        let w = area.width;
        let y = area.height.saturating_sub(1);

        // Fill the row (also lifts any wide-char skip flag left by a pane cell).
        for x in 0..w {
            if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                bc.set_symbol(" ");
                bc.set_skip(false);
                bc.set_style(Style::default().bg(CAT_BASE));
            }
        }

        // Display-width-aware: advance by each glyph's terminal width and mark the
        // continuation cell of a wide (CJK/emoji) glyph, so a wide session name
        // can't corrupt the chips to its right.
        let put = |buf: &mut Buffer, x: &mut u16, s: &str, st: Style| {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if cw == 0 {
                    continue;
                }
                if *x + cw > w {
                    break;
                }
                if let Some(bc) = buf.cell_mut(Position::new(*x, y)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_skip(false);
                    bc.set_style(st);
                }
                if cw == 2
                    && let Some(bc) = buf.cell_mut(Position::new(*x + 1, y))
                {
                    bc.set_symbol(" ");
                    bc.set_skip(true);
                    bc.set_style(st);
                }
                *x += cw;
            }
        };

        // LEFT: session pill + tab chips.
        let mut x = 0u16;
        let sname = self
            .state
            .workspace(&self.ws)
            .and_then(|w| w.name.clone())
            .unwrap_or_else(|| self.ws.to_string());
        put(
            buf,
            &mut x,
            &format!(" {sname} "),
            Style::default()
                .fg(CAT_BASE)
                .bg(CAT_MAUVE)
                .add_modifier(Modifier::BOLD),
        );
        // The pill is a RIGHT-click context-menu anchor (rename/new/kill session);
        // a left click on it is a no-op.
        if x > 0 {
            self.click_zones.borrow_mut().push(ClickZone {
                x0: 0,
                x1: x - 1,
                y0: y,
                y1: y,
                target: ClickTarget::SessionPill(self.ws.clone()),
            });
        }
        put(buf, &mut x, " ", Style::default().bg(CAT_BASE));

        // RIGHT cluster (attention · scroll · agents · clock · host) — built FIRST so the
        // tab chips know how much width they may use before it.
        let mut segs: Vec<(String, Style)> = Vec::new();
        let attn = self.attention_count();
        if attn > 0 {
            segs.push((
                format!(" ⚑ {attn} "),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_PEACH)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if self.scroll_pane.is_some() {
            segs.push((
                " SCROLL ".to_string(),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_YELLOW)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let agents = self.active_session_agent_count();
        if agents > 0 {
            // A space between the agent dot and the count so they don't read as one blob.
            segs.push((
                format!(" ● {agents} "),
                Style::default()
                    .fg(CAT_GREEN)
                    .bg(CAT_SURFACE0)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        // "Update available" hint — only present when the versionpoll thread has
        // seen a release newer than this build. A passive nudge for users who
        // installed via install-comux.sh; hidden entirely when up to date.
        if let Some(v) = &self.version_shown {
            segs.push((
                format!(" ⬆ {} ", v.latest),
                Style::default()
                    .fg(CAT_YELLOW)
                    .bg(CAT_SURFACE0)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        // Usage/limits readout (Claude 5h+weekly · Codex weekly), left of the clock.
        // `usage_layout = paged` (default) shows ONE window (or provider) at a time as a
        // wheel-/click-paged carousel WITH a reset countdown and an `n/N` indicator;
        // `inline` is the legacy single row of every window (no room for resets). The
        // gauge (bar/text/off) is `usage`. Either layout only shows at cols >= 100 —
        // it would crowd out tab chips on a narrow/mobile view (size-adaptive).
        // `usage_zone` records the paged segment's seg-index span so the draw loop can
        // register a `ClickTarget::Usage` hit region for wheel/click paging.
        let mut usage_zone: Option<(usize, usize)> = None;
        if let Some(u) = &self.usage_shown
            && u.has_visible(self.cfg.usage_windows)
            && self.cfg.usage != UsageStyle::Off
            && self.cols >= 100
        {
            let pad = Style::default().bg(CAT_SURFACE0);
            // Each window's gauge is colored by its utilization (green < 70 ≤ yellow
            // < 90 ≤ red); labels/separators/resets stay muted. One segment per part.
            let push_parts = |segs: &mut Vec<(String, Style)>, parts: Vec<UsagePart>| {
                for part in parts {
                    let (text, fg) = match part {
                        UsagePart::Window { text, pct } => (text, usage_threshold_color(pct)),
                        UsagePart::Neutral(s) => (s, CAT_SUBTEXT),
                    };
                    segs.push((text, Style::default().fg(fg).bg(CAT_SURFACE0)));
                }
            };
            let width = |parts: &[UsagePart]| parts.iter().map(|p| p.text().width()).sum::<usize>();
            match self.cfg.usage_layout {
                UsageLayout::Inline => {
                    // Bars are wider than text, so only draw them when the WHOLE bar
                    // segment fits alongside `USAGE_BARS_RESERVE_COLS` kept for the rest of
                    // the right cluster plus a couple of tab chips; else fall back to text.
                    const USAGE_BARS_RESERVE_COLS: usize = 64;
                    let bar_width = (self.cfg.usage == UsageStyle::Bar
                        && (self.cols as usize)
                            >= USAGE_BARS_RESERVE_COLS
                                + usagepoll::bar_display_width(
                                    u,
                                    self.cfg.usage_bar_width,
                                    self.cfg.usage_windows,
                                ))
                    .then_some(self.cfg.usage_bar_width);
                    segs.push((" ".to_string(), pad));
                    push_parts(&mut segs, u.parts(bar_width, self.cfg.usage_windows));
                    segs.push((" ".to_string(), pad));
                }
                UsageLayout::Paged => {
                    let pages = u.pages(
                        self.cfg.usage_windows,
                        self.cfg.usage_page_unit,
                        self.cfg.usage_reset,
                    );
                    if !pages.is_empty() {
                        let now = now_secs() as i64;
                        let n = pages.len();
                        let idx = self.usage_page.min(n - 1);
                        // Bars only when the WIDEST page still leaves room for the rest of
                        // the right cluster; else percentages. A single page is far narrower
                        // than the inline row, so this reserve is smaller.
                        const USAGE_PAGE_RESERVE_COLS: usize = 56;
                        let bar_width = if self.cfg.usage == UsageStyle::Bar {
                            let w = self.cfg.usage_bar_width;
                            let widest = pages
                                .iter()
                                .map(|p| {
                                    width(&usagepoll::page_parts(
                                        p,
                                        Some(w),
                                        now,
                                        self.cfg.usage_reset,
                                    ))
                                })
                                .max()
                                .unwrap_or(0);
                            ((self.cols as usize) >= USAGE_PAGE_RESERVE_COLS + widest).then_some(w)
                        } else {
                            None
                        };
                        // Render every page to find the fixed (widest) width, then PAD the
                        // shown page to it — the segment width stays CONSTANT across pages so
                        // tab chips never shift as you scroll (relative countdowns vary in
                        // width too, e.g. `3d04h` vs `<1m`).
                        let rendered: Vec<Vec<UsagePart>> = pages
                            .iter()
                            .map(|p| usagepoll::page_parts(p, bar_width, now, self.cfg.usage_reset))
                            .collect();
                        let maxw = rendered.iter().map(|r| width(r)).max().unwrap_or(0);
                        segs.push((" ".to_string(), pad));
                        let start = segs.len();
                        let shown = &rendered[idx];
                        let shown_w = width(shown);
                        push_parts(&mut segs, shown.clone());
                        if maxw > shown_w {
                            segs.push((" ".repeat(maxw - shown_w), pad));
                        }
                        // `n/N` page indicator (only with >1 page); else a trailing pad.
                        if n > 1 {
                            segs.push((
                                format!(" {}/{} ", idx + 1, n),
                                Style::default().fg(CAT_SUBTEXT).bg(CAT_SURFACE0),
                            ));
                        } else {
                            segs.push((" ".to_string(), pad));
                        }
                        usage_zone = Some((start, segs.len()));
                    }
                }
            }
        }
        segs.push((
            format!(" {} ", local_hhmm()),
            Style::default().fg(CAT_SUBTEXT).bg(CAT_SURFACE0),
        ));
        segs.push((
            format!(" {} ", hostname()),
            Style::default()
                .fg(CAT_BASE)
                .bg(CAT_BLUE)
                .add_modifier(Modifier::BOLD),
        ));
        let right_total: u16 = segs.iter().map(|(s, _)| s.width() as u16).sum();

        // TAB chips, windowed around the active tab so it STAYS VISIBLE when there are
        // more tabs than fit (‹ / › flag hidden tabs). Agent tabs are yellow + carry a
        // `● ` marker (spaced from the number); active is the mauve pill.
        let ids = self.tab_ids();
        let active = self.active_tab_id();
        let active_idx = active
            .as_ref()
            .and_then(|a| ids.iter().position(|t| t == a))
            .unwrap_or(0);
        let chips: Vec<(String, bool)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let agent = self.tab_has_agent(id);
                let dot = if agent { "● " } else { "" };
                let n = i + 1;
                // `tab_labels` picks number / process name / both. A CUSTOM name (`Ctrl-b ,`
                // / `comux rename-tab`) takes precedence over the process label in every
                // style — even `number` shows it (as `n:name`, keeping the jump index),
                // since renaming is explicit intent to see that name. `name`/`both` fall
                // back to the bare number when the pane has no resolved label yet (or a
                // blank one), so a fresh tab is never an empty chip.
                let custom = self.tab_custom_name(id);
                let inner = match self.cfg.tab_labels {
                    TabLabels::Number => match custom {
                        Some(name) => format!("{n}:{name}"),
                        None => n.to_string(),
                    },
                    TabLabels::Name => match custom.or_else(|| self.tab_focus_label(id)) {
                        Some(name) => name,
                        None => n.to_string(),
                    },
                    TabLabels::Both => match custom.or_else(|| self.tab_focus_label(id)) {
                        Some(name) => format!("{n}:{name}"),
                        None => n.to_string(),
                    },
                };
                (format!(" {dot}{inner} "), agent)
            })
            .collect();
        let widths: Vec<u16> = chips.iter().map(|(s, _)| s.width() as u16).collect();
        let tab_end = w.saturating_sub(right_total);
        let avail = tab_end.saturating_sub(x);
        let (start, end, marker_l, marker_r) = tab_window(&widths, active_idx, avail);
        let dim = Style::default().fg(CAT_OVERLAY).bg(CAT_BASE);
        // Hard-clip every chip at `tab_end` so tabs can NEVER cross into the right cluster
        // (even if the window math is generous on a very narrow bar).
        if marker_l && x < tab_end {
            put(buf, &mut x, "‹", dim);
        }
        for i in start..end {
            let (label, agent) = &chips[i];
            if x + widths[i] > tab_end {
                break; // no room for this (or any further) chip
            }
            let is_active = active.as_ref() == Some(&ids[i]);
            let st = if is_active {
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_MAUVE)
                    .add_modifier(Modifier::BOLD)
            } else if *agent {
                Style::default().fg(CAT_YELLOW).bg(CAT_BASE)
            } else {
                Style::default().fg(CAT_OVERLAY).bg(CAT_BASE)
            };
            let chip_x0 = x;
            put(buf, &mut x, label, st);
            if x > chip_x0 {
                self.click_zones.borrow_mut().push(ClickZone {
                    x0: chip_x0,
                    x1: x - 1,
                    y0: y,
                    y1: y,
                    target: ClickTarget::Tab(ids[i].clone()),
                });
            }
        }
        if marker_r && x < tab_end {
            put(buf, &mut x, "›", dim);
        }

        // Draw the right cluster, right-aligned, never overwriting the left content.
        // While drawing, capture the x-span of the paged usage segment (seg indices in
        // `usage_zone`) and register it as a `ClickTarget::Usage` hit region for paging.
        let mut rx = w.saturating_sub(right_total).max(x);
        let mut usage_x0: Option<u16> = None;
        for (i, (s, st)) in segs.iter().enumerate() {
            if usage_zone.is_some_and(|(a, _)| a == i) {
                usage_x0 = Some(rx);
            }
            put(buf, &mut rx, s, *st);
            if let Some((_, end)) = usage_zone
                && end == i + 1
                && let Some(x0) = usage_x0
            {
                self.click_zones.borrow_mut().push(ClickZone {
                    x0,
                    x1: rx.saturating_sub(1),
                    y0: y,
                    y1: y,
                    target: ClickTarget::Usage,
                });
            }
        }
    }

    /// Centered `Ctrl-f` switcher over the panes: a bordered box listing panes with
    /// a selection cursor.
    fn render_popup(&self, buf: &mut Buffer) {
        let Some(p) = self.popup.as_ref() else { return };
        let area = buf.area;
        let items = self.popup_items(p.tab, &p.filter);

        // Clamp bounds so `min <= max` even on tiny/narrow terminals (the popup
        // is the narrow/mobile fallback, so it must never panic there).
        let maxw = area.width.saturating_sub(2).max(1);
        let w = (area.width / 2).clamp(24.min(maxw), maxw);
        let maxh = area.height.saturating_sub(2).max(1);
        let h = ((area.height as u32 * 3 / 5) as u16).clamp(5.min(maxh), maxh);
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        // Transparent interior (default terminal bg) so a background image shows through;
        // only the border + selection highlights remain painted.
        let bg = Color::Reset;
        let border = Style::default().fg(Color::Cyan).bg(bg);

        // Box background + border.
        for y in y0..(y0 + h).min(area.height) {
            for x in x0..(x0 + w).min(area.width) {
                let sym = if y == y0 && x == x0 {
                    "┌"
                } else if y == y0 && x == x0 + w - 1 {
                    "┐"
                } else if y == y0 + h - 1 && x == x0 {
                    "└"
                } else if y == y0 + h - 1 && x == x0 + w - 1 {
                    "┘"
                } else if y == y0 || y == y0 + h - 1 {
                    "─"
                } else if x == x0 || x == x0 + w - 1 {
                    "│"
                } else {
                    " "
                };
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(sym);
                    bc.set_skip(false);
                    let edge = y == y0 || y == y0 + h - 1 || x == x0 || x == x0 + w - 1;
                    bc.set_style(if edge {
                        border
                    } else {
                        Style::default().bg(bg)
                    });
                }
            }
        }

        let put =
            |buf: &mut ratatui::buffer::Buffer, y: u16, x: u16, maxw: u16, s: &str, st: Style| {
                for (i, ch) in s.chars().take(maxw as usize).enumerate() {
                    if let Some(bc) = buf.cell_mut(Position::new(x + i as u16, y)) {
                        let mut sb = [0u8; 4];
                        bc.set_symbol(ch.encode_utf8(&mut sb));
                        // Clear any wide-char skip flag inherited from the pane cell
                        // underneath, or buffer-diffing omits the overlay cell.
                        bc.set_skip(false);
                        bc.set_style(st);
                    }
                }
            };

        // Title in the top border (with the key hints).
        put(
            buf,
            y0,
            x0 + 2,
            w.saturating_sub(4),
            " go to  (←/→ tab · ^R rename) ",
            Style::default()
                .fg(Color::Cyan)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );

        let inner_x = x0 + 2;
        let inner_w = w.saturating_sub(4);

        // Row 1: the two tabs (Left/Right switches), active one highlighted.
        let tab_hi = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let tab_lo = Style::default().fg(CAT_OVERLAY).bg(bg);
        let mut tx = inner_x;
        put(
            buf,
            y0 + 1,
            tx,
            10,
            " sessions ",
            if p.tab == PopupTab::Sessions {
                tab_hi
            } else {
                tab_lo
            },
        );
        tx += 10;
        put(
            buf,
            y0 + 1,
            tx,
            8,
            " agents ",
            if p.tab == PopupTab::Agents {
                tab_hi
            } else {
                tab_lo
            },
        );

        // Row 2: the fuzzy filter input with a block cursor.
        put(
            buf,
            y0 + 2,
            inner_x,
            inner_w,
            &format!("> {}█", p.filter),
            Style::default().fg(CAT_TEXT).bg(bg),
        );

        // Remaining rows: the filtered items, scrolled so the selection stays visible.
        let list_top = y0 + 3;
        let visible = (h.saturating_sub(4)) as usize; // borders + tab + filter lines
        if items.is_empty() {
            put(
                buf,
                list_top,
                inner_x,
                inner_w,
                "  (no match)",
                Style::default().fg(CAT_OVERLAY).bg(bg),
            );
            return;
        }
        let start = if p.sel >= visible {
            p.sel + 1 - visible
        } else {
            0
        };
        for (row_i, item) in items.iter().enumerate().skip(start).take(visible) {
            let y = list_top + (row_i - start) as u16;
            let selected = row_i == p.sel;
            let arrow = if selected { "▸" } else { " " };
            let text = format!("{arrow} {} {}", item.glyph, item.text);
            let padded = format!("{text:<width$}", width = inner_w as usize);
            let st = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(item.color).bg(bg)
            };
            put(buf, y, inner_x, inner_w, &padded, st);
        }
    }

    /// The notification center (`Ctrl-b a`): a centered box listing logged agent-turn
    /// events, newest first — Enter jumps to the source pane, `d` dismisses.
    /// Draw the inline single-line prompt (new-session name / rename) as a small
    /// centered box with the typed text + a block cursor. Clamps on tiny terminals.
    fn render_prompt(&self, buf: &mut Buffer) {
        let Some(p) = self.prompt.as_ref() else {
            return;
        };
        let area = buf.area;
        let maxw = area.width.saturating_sub(2).max(1);
        let w = ((area.width as u32 * 3 / 5) as u16).clamp(24.min(maxw), maxw);
        let h = 3u16.min(area.height.max(1));
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        // Transparent interior (default terminal bg) so a background image shows through;
        // only the border + selection highlights remain painted.
        let bg = Color::Reset;
        let border = Style::default().fg(CAT_MAUVE).bg(bg);

        for y in y0..(y0 + h).min(area.height) {
            for x in x0..(x0 + w).min(area.width) {
                let sym = if y == y0 && x == x0 {
                    "┌"
                } else if y == y0 && x == x0 + w - 1 {
                    "┐"
                } else if y == y0 + h - 1 && x == x0 {
                    "└"
                } else if y == y0 + h - 1 && x == x0 + w - 1 {
                    "┘"
                } else if y == y0 || y == y0 + h - 1 {
                    "─"
                } else if x == x0 || x == x0 + w - 1 {
                    "│"
                } else {
                    " "
                };
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(sym);
                    bc.set_skip(false);
                    let edge = y == y0 || y == y0 + h - 1 || x == x0 || x == x0 + w - 1;
                    bc.set_style(if edge {
                        border
                    } else {
                        Style::default().bg(bg)
                    });
                }
            }
        }

        let put = |buf: &mut Buffer, y: u16, x: u16, maxw: u16, s: &str, st: Style| {
            for (i, ch) in s.chars().take(maxw as usize).enumerate() {
                if let Some(bc) = buf.cell_mut(Position::new(x + i as u16, y)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_skip(false);
                    bc.set_style(st);
                }
            }
        };

        let title = match p.kind {
            PromptKind::NewSession => " new session ",
            PromptKind::NewWorktree => " new worktree (branch) ",
            PromptKind::RenameSession(_) => " rename session ",
            PromptKind::RenameTab(..) => " rename tab ",
        };
        put(
            buf,
            y0,
            x0 + 2,
            w.saturating_sub(4),
            title,
            border.add_modifier(Modifier::BOLD),
        );
        // Text line with a trailing block cursor; right-truncate so the caret stays
        // visible as the input grows past the box width.
        let inner_w = w.saturating_sub(4) as usize;
        let shown: String = {
            let with_caret = format!("{}█", p.buf);
            let chars: Vec<char> = with_caret.chars().collect();
            if chars.len() > inner_w {
                chars[chars.len() - inner_w..].iter().collect()
            } else {
                with_caret
            }
        };
        put(
            buf,
            y0 + 1,
            x0 + 2,
            w.saturating_sub(4),
            &shown,
            Style::default().fg(CAT_TEXT).bg(bg),
        );
    }

    /// Draw the `y`/`n` confirm modal (kill-session) as a small centered box holding the
    /// message. Reuses `render_prompt`'s box geometry; clamps on tiny terminals.
    fn render_confirm(&self, buf: &mut Buffer) {
        let Some(cf) = self.confirm.as_ref() else {
            return;
        };
        let area = buf.area;
        let msg_w = UnicodeWidthStr::width(cf.message.as_str()) as u16 + 4;
        let maxw = area.width.saturating_sub(2).max(1);
        let w = msg_w.clamp(24.min(maxw), maxw);
        let h = 3u16.min(area.height.max(1));
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        // Transparent interior (default terminal bg) so a background image shows through;
        // only the border + selection highlights remain painted.
        let bg = Color::Reset;
        let border = Style::default().fg(CAT_PEACH).bg(bg);

        for y in y0..(y0 + h).min(area.height) {
            for x in x0..(x0 + w).min(area.width) {
                let sym = if y == y0 && x == x0 {
                    "┌"
                } else if y == y0 && x == x0 + w - 1 {
                    "┐"
                } else if y == y0 + h - 1 && x == x0 {
                    "└"
                } else if y == y0 + h - 1 && x == x0 + w - 1 {
                    "┘"
                } else if y == y0 || y == y0 + h - 1 {
                    "─"
                } else if x == x0 || x == x0 + w - 1 {
                    "│"
                } else {
                    " "
                };
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(sym);
                    bc.set_skip(false);
                    let edge = y == y0 || y == y0 + h - 1 || x == x0 || x == x0 + w - 1;
                    bc.set_style(if edge {
                        border
                    } else {
                        Style::default().bg(bg)
                    });
                }
            }
        }
        let inner_w = w.saturating_sub(4);
        for (i, ch) in cf.message.chars().take(inner_w as usize).enumerate() {
            if let Some(bc) = buf.cell_mut(Position::new(x0 + 2 + i as u16, y0 + 1)) {
                let mut sb = [0u8; 4];
                bc.set_symbol(ch.encode_utf8(&mut sb));
                bc.set_skip(false);
                bc.set_style(Style::default().fg(CAT_TEXT).bg(bg));
            }
        }
    }

    /// Draw the right-click context menu at its fixed rect: a bordered box with the
    /// clicked thing's name in the top border and one item per row (the hovered /
    /// keyboard-selected one highlighted). Anchored near the click, not centered.
    fn render_menu(&self, buf: &mut Buffer) {
        let Some(m) = self.menu.as_ref() else { return };
        let area = buf.area;
        let (x0, y0, w, h) = (m.x0, m.y0, m.w, m.h);
        let bg = Color::Reset;
        let border = Style::default().fg(CAT_MAUVE).bg(bg);

        for y in y0..(y0 + h).min(area.height) {
            for x in x0..(x0 + w).min(area.width) {
                let sym = if y == y0 && x == x0 {
                    "┌"
                } else if y == y0 && x == x0 + w - 1 {
                    "┐"
                } else if y == y0 + h - 1 && x == x0 {
                    "└"
                } else if y == y0 + h - 1 && x == x0 + w - 1 {
                    "┘"
                } else if y == y0 || y == y0 + h - 1 {
                    "─"
                } else if x == x0 || x == x0 + w - 1 {
                    "│"
                } else {
                    " "
                };
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(sym);
                    bc.set_skip(false);
                    let edge = y == y0 || y == y0 + h - 1 || x == x0 || x == x0 + w - 1;
                    bc.set_style(if edge {
                        border
                    } else {
                        Style::default().bg(bg)
                    });
                }
            }
        }

        // Width-aware writer (a wide session/tab name must not corrupt the border).
        let put = |buf: &mut Buffer, mut x: u16, y: u16, max_x: u16, s: &str, st: Style| {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if cw == 0 {
                    continue;
                }
                if x + cw > max_x || y >= area.height {
                    break;
                }
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    let mut sb = [0u8; 4];
                    bc.set_symbol(ch.encode_utf8(&mut sb));
                    bc.set_skip(false);
                    bc.set_style(st);
                }
                if cw == 2
                    && let Some(bc) = buf.cell_mut(Position::new(x + 1, y))
                {
                    bc.set_symbol(" ");
                    bc.set_skip(true);
                    bc.set_style(st);
                }
                x += cw;
            }
        };

        // Title in the top border.
        put(
            buf,
            x0 + 1,
            y0,
            (x0 + w).saturating_sub(1).min(area.width),
            &format!(" {} ", m.title),
            border.add_modifier(Modifier::BOLD),
        );
        // Items: one per interior row, padded to the full inner width so the
        // hover highlight reads as a bar.
        let inner_w = w.saturating_sub(2) as usize;
        for (i, (label, _)) in m.items.iter().enumerate() {
            let yy = y0 + 1 + i as u16;
            let st = if m.sel == Some(i) {
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_MAUVE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(CAT_TEXT).bg(bg)
            };
            let text = format!(" {label:<width$}", width = inner_w.saturating_sub(1));
            put(
                buf,
                x0 + 1,
                yy,
                (x0 + w).saturating_sub(1).min(area.width),
                &text,
                st,
            );
        }
    }

    fn render_center(&self, buf: &mut Buffer) {
        let Some(sel) = self.center else { return };
        let area = buf.area;
        let maxw = area.width.saturating_sub(2).max(1);
        let w = ((area.width as u32 * 3 / 4) as u16).clamp(30.min(maxw), maxw);
        let maxh = area.height.saturating_sub(2).max(1);
        let h = ((area.height as u32 * 3 / 5) as u16).clamp(6.min(maxh), maxh);
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        // Transparent interior (default terminal bg) so a background image shows through;
        // only the border + selection highlights remain painted.
        let bg = Color::Reset;
        let border = Style::default().fg(CAT_MAUVE).bg(bg);

        for y in y0..(y0 + h).min(area.height) {
            for x in x0..(x0 + w).min(area.width) {
                let sym = if y == y0 && x == x0 {
                    "┌"
                } else if y == y0 && x == x0 + w - 1 {
                    "┐"
                } else if y == y0 + h - 1 && x == x0 {
                    "└"
                } else if y == y0 + h - 1 && x == x0 + w - 1 {
                    "┘"
                } else if y == y0 || y == y0 + h - 1 {
                    "─"
                } else if x == x0 || x == x0 + w - 1 {
                    "│"
                } else {
                    " "
                };
                if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                    bc.set_symbol(sym);
                    bc.set_skip(false);
                    let edge = y == y0 || y == y0 + h - 1 || x == x0 || x == x0 + w - 1;
                    bc.set_style(if edge {
                        border
                    } else {
                        Style::default().bg(bg)
                    });
                }
            }
        }

        let put =
            |buf: &mut ratatui::buffer::Buffer, y: u16, x: u16, maxw: u16, s: &str, st: Style| {
                for (i, ch) in s.chars().take(maxw as usize).enumerate() {
                    if let Some(bc) = buf.cell_mut(Position::new(x + i as u16, y)) {
                        let mut sb = [0u8; 4];
                        bc.set_symbol(ch.encode_utf8(&mut sb));
                        bc.set_skip(false);
                        bc.set_style(st);
                    }
                }
            };

        let inner_x = x0 + 2;
        let inner_w = w.saturating_sub(4);
        put(
            buf,
            y0,
            inner_x,
            inner_w,
            &format!(
                " notifications ({}) — j/k · enter jump · d dismiss · D clear · q ",
                self.notifications.len()
            ),
            Style::default()
                .fg(CAT_MAUVE)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );

        if self.notifications.is_empty() {
            put(
                buf,
                y0 + 2,
                inner_x,
                inner_w,
                "  (no notifications yet)",
                Style::default().fg(CAT_OVERLAY).bg(bg),
            );
            return;
        }

        // Scroll the viewport so the selected row is always visible.
        let visible = h.saturating_sub(2) as usize; // rows between the borders
        let start = if visible > 0 && sel >= visible {
            sel + 1 - visible
        } else {
            0
        };
        for (j, note) in self
            .notifications
            .iter()
            .skip(start)
            .take(visible)
            .enumerate()
        {
            let i = start + j;
            let y = y0 + 1 + j as u16;
            let selected = i == sel;
            let glyph = if note.kind == "blocked" { "▲" } else { "✓" };
            let row = format!(
                "{} {} {} {} · {}  {}",
                if selected { "▸" } else { " " },
                note.when,
                glyph,
                note.tool,
                note.space,
                note.body
            );
            let st = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(CAT_MAUVE)
                    .add_modifier(Modifier::BOLD)
            } else if note.kind == "blocked" {
                Style::default().fg(CAT_YELLOW).bg(bg)
            } else {
                Style::default().fg(CAT_SUBTEXT).bg(bg)
            };
            let padded = format!("{row:<width$}", width = inner_w as usize);
            put(buf, y, inner_x, inner_w, &padded, st);
        }
    }
}

/// Does the gap cell `(x, y)` border the focused rect (so its divider is accented)?
fn divider_touches(fr: &PaneRect, x: u16, y: u16) -> bool {
    let in_y = y >= fr.y && y < fr.y + fr.rows;
    let in_x = x >= fr.x && x < fr.x + fr.cols;
    let vert = in_y && (x + 1 == fr.x || x == fr.x + fr.cols);
    let horiz = in_x && (y + 1 == fr.y || y == fr.y + fr.rows);
    vert || horiz
}

/// Truncate a tab-chip label on DISPLAY width (max 12 cells; `comm` is already ≤15 on
/// Linux but a long process or custom name still shouldn't dominate the bar), adding an
/// ellipsis when clipped. Width-aware so CJK/emoji names truncate cleanly.
fn clip_chip(name: &str) -> String {
    const MAX: usize = 12;
    if name.width() <= MAX {
        return name.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in name.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > MAX - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Cheap gate for the usage carousel's auto-rotate: is it enabled and has the
/// interval elapsed? `secs == 0` means off. Kept separate (and pure) so the hot
/// 30 fps render tick avoids recomputing the page list until the interval is due.
fn usage_should_roll(secs: u32, elapsed: std::time::Duration) -> bool {
    secs != 0 && elapsed >= std::time::Duration::from_secs(secs as u64)
}

/// Wrap `cur + delta` into `0..n` for the usage carousel (n ≥ 1). A stale `cur`
/// (page count shrank) is clamped into range before stepping, so a scroll always
/// lands on a valid page. `n == 0` is a defensive no-op.
fn wrap_page(cur: usize, delta: i32, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let cur = cur.min(n - 1) as i32;
    (cur + delta).rem_euclid(n as i32) as usize
}

/// Current Unix time in seconds (0 on a clock before the epoch — informational only).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Keep only the entries whose name is in `whitelist` (order preserved, last value wins on
/// a duplicate name). Applied to a client's advertised env before it becomes `client_env`.
fn filter_env(env: Vec<(String, String)>, whitelist: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in env {
        if !whitelist.iter().any(|w| w == &k) {
            continue;
        }
        if let Some(slot) = out.iter_mut().find(|(ek, _)| ek == &k) {
            slot.1 = v;
        } else {
            out.push((k, v));
        }
    }
    out
}

/// `base` with `over` layered on top: `over`'s value replaces a matching key, otherwise it
/// appends. Used to build a pane's env = socket markers (base) + client session vars (over).
fn merge_env(base: &[(String, String)], over: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = base.to_vec();
    for (k, v) in over {
        if let Some(slot) = out.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v.clone();
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// Restore saved sessions into `state`/`panes` (continuum-style). Returns the active
/// workspace + the `next_session` counter (so freshly created sessions can't collide with
/// restored `sN` ids), or `None` if nothing valid could be restored. Transactional per
/// session: PTYs are spawned OFF-STATE and pruned on failure, so `State` only ever gets
/// fully-built, non-empty trees (no dangling leaves).
fn restore_sessions(
    state: &mut State,
    panes: &mut HashMap<TerminalId, PaneTerm>,
    snap: &persist::PersistState,
    cols: u16,
    rows: u16,
    sock_env: &[(String, String)],
) -> Option<(WorkspaceId, u64)> {
    let viewport = Rect { cols, rows };
    let mut budget = MAX_TOTAL_PANES; // total panes across all sessions
    let mut installed: Vec<WorkspaceId> = Vec::new();
    let mut active_ws: Option<WorkspaceId> = None;

    for (si, ps) in snap.sessions.iter().enumerate() {
        // Phase 1: spawn+prune every tab OFF-STATE.
        let mut restored_tabs: Vec<RestoredTab> = Vec::new();
        let mut tab_paneterms: Vec<Vec<PaneTerm>> = Vec::new();
        for (ti, pt) in ps.tabs.iter().enumerate() {
            // Reject a suspiciously large tab outright (untrusted file).
            if pt.layout.leaf_count() > MAX_LEAVES_PER_TAB {
                continue;
            }
            if let Some((spec, pts)) =
                spawn_layout(&pt.layout, cols, rows, 0, &mut budget, sock_env)
            {
                restored_tabs.push(RestoredTab {
                    name: pt.name.clone(),
                    layout: spec,
                    // Active-by-IDENTITY: flag the tab that was active by its ORIGINAL
                    // index, so pruning earlier tabs can't misselect (codex review).
                    active: ti == ps.active_tab,
                });
                tab_paneterms.push(pts);
            }
        }
        if restored_tabs.is_empty() {
            continue; // whole session died / empty → skip it
        }

        // Phase 2: single atomic commit into State (mints ids, registers terminals).
        let idx = installed.len();
        let id = if idx == 0 {
            WorkspaceId::new("local")
        } else {
            WorkspaceId::new(format!("s{idx}"))
        };
        let per_tab_terms =
            state.install_restored_session(id.clone(), ps.name.clone(), restored_tabs, viewport);

        // Phase 3: attach the already-spawned PaneTerms by the ids State minted (DFS order
        // matches spawn order, since both walk the same pruned spec first-then-second).
        for (terms, pts) in per_tab_terms.into_iter().zip(tab_paneterms) {
            for (tid, pane_term) in terms.into_iter().zip(pts) {
                panes.insert(tid, pane_term);
            }
        }
        if si == snap.active_session {
            active_ws = Some(id.clone());
        }
        installed.push(id);
    }

    if installed.is_empty() {
        return None;
    }
    let active = active_ws.unwrap_or_else(|| installed[0].clone());
    // Restored ids are `local, s1, .., s{n-1}`; next new session must be `s{n}`.
    Some((active, installed.len() as u64))
}

/// Spawn PTYs for a persisted layout, pruning any leaf whose shell can't spawn. Returns
/// the pruned `LayoutSpec` (live leaves only) + the `PaneTerm`s in DFS pre-order, or `None`
/// if the whole subtree died. Enforces the depth cap and the shared pane `budget`.
fn spawn_layout(
    node: &PLayout,
    cols: u16,
    rows: u16,
    depth: usize,
    budget: &mut usize,
    sock_env: &[(String, String)],
) -> Option<(LayoutSpec, Vec<PaneTerm>)> {
    match node {
        PLayout::Leaf { cwd, command } => {
            if *budget == 0 {
                return None;
            }
            let dir = resolve_cwd(cwd.as_deref());
            let pt = PaneTerm::spawn_with_env(cols.max(1), rows.max(1), None, dir, sock_env)?;
            *budget -= 1;
            // Re-run a whitelisted program (agent): shell-quote each argv element and
            // inject the line into the fresh shell. Quoting preserves argument boundaries
            // and neutralizes metacharacters/newlines (a saved `claude "a; b"` stays one
            // arg, never two shell commands); the shell buffers it until its prompt is ready.
            if let Some(argv) = command
                && let Some(line) = build_command_line(argv)
            {
                pt.input(format!("{line}\n").as_bytes());
            }
            Some((LayoutSpec::Leaf, vec![pt]))
        }
        PLayout::Branch {
            dir,
            ratio,
            first,
            second,
        } => {
            if depth >= persist::MAX_DEPTH {
                return None; // too deep (untrusted) → prune this subtree
            }
            let f = spawn_layout(first, cols, rows, depth + 1, budget, sock_env);
            let s = spawn_layout(second, cols, rows, depth + 1, budget, sock_env);
            match (f, s) {
                (Some((lf, mut pf)), Some((ls, ps))) => {
                    pf.extend(ps);
                    Some((
                        LayoutSpec::Branch {
                            dir: *dir,
                            ratio: *ratio,
                            first: Box::new(lf),
                            second: Box::new(ls),
                        },
                        pf,
                    ))
                }
                // One child died → collapse to the survivor (prune the dead branch).
                (Some(one), None) | (None, Some(one)) => Some(one),
                (None, None) => None,
            }
        }
    }
}

/// Build a shell command line from a saved argv by POSIX single-quoting each argument, so
/// argument boundaries + any metacharacters/newlines are preserved literally (the shell
/// runs exactly this argv, never re-splitting a quoted arg into extra commands). Returns
/// `None` for an empty argv or if the result exceeds the length cap.
fn build_command_line(argv: &[String]) -> Option<String> {
    if argv.is_empty() {
        return None;
    }
    let line = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    (line.len() <= persist::MAX_COMMAND_LEN).then_some(line)
}

/// POSIX single-quote one argument: wrap in `'…'` and turn any embedded `'` into `'\''`.
/// Safe for sh/bash/zsh (fish quoting differs — a niche the owner's agents don't hit).
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The start index of a vertical list window that keeps `active` visible when `total`
/// items don't fit in `visible` rows (centers `active`, clamped to the ends).
/// Update `prev` with the visible panes' current alt-screen bits and report whether any pane
/// just LEFT the alternate screen (its bit went true→false). Panes no longer visible are
/// dropped from `prev` so a later re-appearance re-seeds cleanly (and can't spuriously fire).
/// Split out as a pure function so the edge logic is unit-testable without a live PTY.
fn detect_alt_screen_exit(
    prev: &mut HashMap<TerminalId, bool>,
    visible: &[(TerminalId, bool)],
) -> bool {
    let mut exited = false;
    for (tid, now) in visible {
        let was = prev.insert(tid.clone(), *now).unwrap_or(false);
        if was && !*now {
            exited = true;
        }
    }
    prev.retain(|tid, _| visible.iter().any(|(v, _)| v == tid));
    exited
}

/// Normalize two selection endpoints into `(start, end)` in reading order (row, then column).
fn sel_bounds(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// The inclusive selected column range `[c0, c1]` for viewport `row` in a LINEAR selection
/// `start..=end` over `cols`-wide rows (first row runs to the right edge, last row from the
/// left edge, middle rows full-width). `None` if the row is outside the selection.
fn sel_cols(start: (u16, u16), end: (u16, u16), cols: u16, row: u16) -> Option<(u16, u16)> {
    if cols == 0 || row < start.1 || row > end.1 {
        return None;
    }
    let last = cols - 1;
    let c0 = if row == start.1 { start.0 } else { 0 };
    let c1 = if row == end.1 { end.0 } else { last };
    Some((c0.min(last), c1.min(last)))
}

/// Extract the text of a linear selection from a pane snapshot (tmux-style copy). Skips
/// wide-glyph trailing spacers (the grapheme lives on the leading cell), trims trailing
/// whitespace per visual line, and joins rows with `\n` EXCEPT at a soft-wrap seam (a row
/// alacritty marked `WRAPLINE` and selected to its right edge continues the same logical line).
/// If `start` lands on a wide glyph's trailing spacer (the glyph's right half), snap it left to
/// the leading cell — so a drag begun there includes AND highlights the whole glyph. Guarded on
/// the previous cell being a genuine wide leading glyph, so a right-margin `LEADING_WIDE_CHAR_SPACER`
/// (whose glyph is on the NEXT row) doesn't wrongly pull in an unrelated preceding cell. Shared by
/// extraction and the render highlight so the tinted span and the copied text agree exactly.
fn sel_start_norm(snap: &crate::term::Snapshot, start: (u16, u16)) -> (u16, u16) {
    if start.0 > 0
        && let Some(row) = snap.cells.get(start.1 as usize)
        && row.get(start.0 as usize).is_some_and(|c| c.spacer)
        && row
            .get((start.0 - 1) as usize)
            .is_some_and(|c| !c.spacer && UnicodeWidthStr::width(c.sym.as_str()) >= 2)
    {
        (start.0 - 1, start.1)
    } else {
        start
    }
}

fn extract_selection(snap: &crate::term::Snapshot, anchor: (u16, u16), head: (u16, u16)) -> String {
    let (start, end) = sel_bounds(anchor, head);
    let start = sel_start_norm(snap, start);
    let mut out = String::new();
    let mut row = start.1;
    while row <= end.1 {
        let Some(cells) = snap.cells.get(row as usize) else {
            break;
        };
        let cols = cells.len() as u16;
        if let Some((c0, c1)) = sel_cols(start, end, cols, row) {
            let mut line = String::new();
            for c in c0..=c1 {
                let cell = &cells[c as usize];
                if cell.spacer {
                    continue; // wide-glyph right half: the grapheme is on the leading cell
                }
                line.push_str(&cell.sym);
            }
            // A soft-wrap seam = this row (not the last) wraps into the next AND the selection
            // reached its right edge. At a seam the row continues the SAME logical line, so keep
            // its trailing space (trimming would fuse "word " + "next" → "wordnext") and emit no
            // newline. Everywhere else, trim trailing whitespace and break the line.
            let soft = row < end.1
                && snap.wrapped.get(row as usize).copied().unwrap_or(false)
                && c1 + 1 >= cols;
            if soft {
                out.push_str(&line);
            } else {
                out.push_str(line.trim_end());
                if row < end.1 {
                    out.push('\n');
                }
            }
        }
        row += 1;
    }
    out
}

fn list_window_start(total: usize, active: usize, visible: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    active.saturating_sub(visible / 2).min(total - visible)
}

/// Choose a contiguous window of tab chips to show when they don't all fit in `avail`
/// columns, ALWAYS including `active` and expanding around it. Returns `(start, end,
/// show_left_marker, show_right_marker)` — the `[start, end)` index range plus whether a
/// `‹`/`›` overflow marker should flag hidden tabs on each side.
fn tab_window(widths: &[u16], active: usize, avail: u16) -> (usize, usize, bool, bool) {
    let n = widths.len();
    if n == 0 || avail == 0 {
        return (0, 0, false, false);
    }
    // u32 accumulation so a huge tab count can't overflow u16.
    let total: u32 = widths.iter().map(|&w| w as u32).sum();
    if total <= avail as u32 {
        return (0, n, false, false); // everything fits — no window, no markers
    }
    let inner = avail.saturating_sub(2) as u32; // reserve ~2 cols for the ‹ › markers
    let mut lo = active.min(n - 1);
    let mut hi = lo;
    let mut used = (widths[lo] as u32).min(inner);
    loop {
        let mut grew = false;
        // Prefer growing right, then left, so the active tab keeps some trailing context.
        if hi + 1 < n && used + widths[hi + 1] as u32 <= inner {
            hi += 1;
            used += widths[hi] as u32;
            grew = true;
        }
        if lo > 0 && used + widths[lo - 1] as u32 <= inner {
            lo -= 1;
            used += widths[lo] as u32;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    (lo, hi + 1, lo > 0, hi + 1 < n)
}

/// Case-insensitive SUBSEQUENCE match (fzf-style): every char of `needle` appears in
/// `hay` in order. An empty needle matches everything.
fn fuzzy_match(needle: &str, hay: &str) -> bool {
    let hay = hay.to_ascii_lowercase();
    let mut chars = hay.chars();
    needle
        .to_ascii_lowercase()
        .chars()
        .all(|nc| chars.any(|hc| hc == nc))
}

/// Resolve a saved cwd to a concrete directory for a restored shell: the saved path if it
/// still exists, else `$HOME` (never `None` when a home is known, so the shell doesn't
/// silently inherit the SERVER's cwd).
fn resolve_cwd(cwd: Option<&str>) -> Option<PathBuf> {
    if let Some(s) = cwd
        && Path::new(s).is_dir()
    {
        return Some(PathBuf::from(s));
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Local `HH:MM` for the status-bar clock (via libc `localtime_r`, no chrono dep).
fn local_hhmm() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `secs` is a valid time_t; `tm` is a zeroed, correctly-sized out-param.
    let r = unsafe { libc::localtime_r(&secs as *const libc::time_t, &mut tm) };
    if r.is_null() {
        return "--:--".to_string();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// Local minute-of-day (`hour*60 + minute`), for the idle render loop to detect an
/// `HH:MM` rollover. Falls back to a monotonic-ish value on error (never panics).
fn local_minute() -> u32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `secs` is a valid time_t; `tm` is a zeroed, correctly-sized out-param.
    let r = unsafe { libc::localtime_r(&secs as *const libc::time_t, &mut tm) };
    if r.is_null() {
        return (secs / 60) as u32;
    }
    (tm.tm_hour.max(0) as u32) * 60 + tm.tm_min.max(0) as u32
}

/// The short hostname (before the first dot) for the status bar.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: writing at most `buf.len()` bytes into our own buffer.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r != 0 {
        return "host".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let full = String::from_utf8_lossy(&buf[..end]);
    full.split('.').next().unwrap_or("host").to_string()
}

fn to_color(c: CellColor) -> Color {
    match c {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(i) => Color::Indexed(i),
        CellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Translate a key event into the bytes a PTY expects.
fn key_to_bytes(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let esc = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, 0x1b);
        }
        v
    };

    let bytes = match code {
        KeyCode::Char(ch) => {
            let utf8 = |ch: char| {
                let mut s = [0u8; 4];
                ch.encode_utf8(&mut s).as_bytes().to_vec()
            };
            if ctrl {
                // Only the ASCII control combinations map to control bytes;
                // Ctrl with digits / non-ASCII passes the char through unchanged.
                match ch {
                    ' ' | '@' => vec![0], // Ctrl-Space / Ctrl-@ → NUL
                    'a'..='z' | 'A'..='Z' => vec![(ch.to_ascii_lowercase() as u8) & 0x1f],
                    '[' | '\\' | ']' | '^' | '_' => vec![(ch as u8) & 0x1f],
                    _ => utf8(ch),
                }
            } else {
                utf8(ch)
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        _ => return None,
    };
    Some(esc(bytes))
}

#[cfg(test)]
mod tests {
    use super::{
        CAT_GREEN, CAT_RED, CAT_YELLOW, Menu, MenuAction, build_command_line,
        detect_alt_screen_exit, extract_selection, filter_env, fuzzy_match, list_window_start,
        menu_origin, merge_env, sel_bounds, sel_cols, shell_quote, tab_window, usage_should_roll,
        usage_threshold_color, wrap_page,
    };
    use crate::model::TerminalId;
    use crate::term::{CellColor, CellSnap, Snapshot};
    use std::collections::HashMap;

    fn tid(s: &str) -> TerminalId {
        TerminalId::new(s)
    }

    /// Build a snapshot from row strings (one cell per char, no wide chars, no wrap).
    fn snap(rows: &[&str]) -> Snapshot {
        let cells: Vec<Vec<CellSnap>> = rows
            .iter()
            .map(|r| {
                r.chars()
                    .map(|ch| CellSnap {
                        sym: ch.to_string(),
                        spacer: false,
                        fg: CellColor::Default,
                        bg: CellColor::Default,
                        bold: false,
                        reverse: false,
                    })
                    .collect()
            })
            .collect();
        let cols = cells.iter().map(|r| r.len()).max().unwrap_or(0) as u16;
        Snapshot {
            cols,
            rows: cells.len() as u16,
            wrapped: vec![false; cells.len()],
            cells,
            cursor: (0, 0),
        }
    }

    #[test]
    fn usage_should_roll_respects_interval_and_off() {
        use std::time::Duration;
        assert!(!usage_should_roll(0, Duration::from_secs(999))); // 0 = off, never rolls
        assert!(!usage_should_roll(10, Duration::from_secs(9))); // interval not reached
        assert!(usage_should_roll(10, Duration::from_secs(10))); // exactly due
        assert!(usage_should_roll(10, Duration::from_secs(11))); // past due
    }

    #[test]
    fn wrap_page_wraps_both_directions() {
        assert_eq!(wrap_page(0, 1, 3), 1);
        assert_eq!(wrap_page(2, 1, 3), 0); // forward past the end wraps to 0
        assert_eq!(wrap_page(0, -1, 3), 2); // backward past 0 wraps to the end
        assert_eq!(wrap_page(5, 1, 3), 0); // stale index clamped to 2, then +1 → 0
        assert_eq!(wrap_page(1, 0, 3), 1);
        assert_eq!(wrap_page(0, 1, 1), 0); // a single page stays put
        assert_eq!(wrap_page(0, 1, 0), 0); // defensive: no pages
    }

    #[test]
    fn sel_bounds_orders_by_row_then_col() {
        assert_eq!(sel_bounds((5, 2), (1, 0)), ((1, 0), (5, 2)));
        assert_eq!(sel_bounds((1, 0), (5, 2)), ((1, 0), (5, 2)));
        assert_eq!(sel_bounds((5, 1), (2, 1)), ((2, 1), (5, 1))); // same row → by col
    }

    #[test]
    fn sel_cols_linear_spans() {
        // Single-row selection: just the [c0,c1] on that row.
        assert_eq!(sel_cols((2, 0), (5, 0), 10, 0), Some((2, 5)));
        assert_eq!(sel_cols((2, 0), (5, 0), 10, 1), None); // outside
        // Multi-row: first row runs to the right edge, middle full, last from the left.
        assert_eq!(sel_cols((3, 0), (2, 2), 10, 0), Some((3, 9)));
        assert_eq!(sel_cols((3, 0), (2, 2), 10, 1), Some((0, 9)));
        assert_eq!(sel_cols((3, 0), (2, 2), 10, 2), Some((0, 2)));
        assert_eq!(sel_cols((0, 0), (0, 0), 0, 0), None); // zero-width guard
    }

    #[test]
    fn extract_selection_multiline_trims_trailing_ws() {
        let s = snap(&["hi   ", "yo   "]);
        // Full both rows: trailing spaces trimmed, joined with a newline.
        assert_eq!(extract_selection(&s, (0, 0), (4, 1)), "hi\nyo");
    }

    #[test]
    fn extract_selection_single_row_substring() {
        let s = snap(&["hello world"]);
        assert_eq!(extract_selection(&s, (6, 0), (10, 0)), "world");
    }

    #[test]
    fn extract_selection_skips_wide_char_spacer() {
        // A wide glyph occupies a leading cell + a trailing spacer; extraction keeps the
        // leading grapheme and skips the spacer, so "가X" comes out intact.
        let cells = vec![vec![
            CellSnap {
                sym: "가".into(),
                spacer: false,
                fg: CellColor::Default,
                bg: CellColor::Default,
                bold: false,
                reverse: false,
            },
            CellSnap {
                sym: " ".into(),
                spacer: true,
                fg: CellColor::Default,
                bg: CellColor::Default,
                bold: false,
                reverse: false,
            },
            CellSnap {
                sym: "X".into(),
                spacer: false,
                fg: CellColor::Default,
                bg: CellColor::Default,
                bold: false,
                reverse: false,
            },
        ]];
        let s = Snapshot {
            cols: 3,
            rows: 1,
            wrapped: vec![false],
            cells,
            cursor: (0, 0),
        };
        assert_eq!(extract_selection(&s, (0, 0), (2, 0)), "가X");
        // Starting the drag on the glyph's RIGHT half (the spacer at col 1) must still include
        // the leading 가 — a normalization, not the declared oracle edge case.
        assert_eq!(extract_selection(&s, (1, 0), (2, 0)), "가X");
    }

    #[test]
    fn extract_selection_softwrap_joins_without_newline() {
        // Row 0 is soft-wrapped (fills to the edge) and selected to its right edge → the two
        // rows form one logical line, so no '\n' at the seam.
        let mut s = snap(&["abcde", "fgh  "]);
        s.wrapped[0] = true;
        assert_eq!(extract_selection(&s, (0, 0), (4, 1)), "abcdefgh");
        // But a NON-wrapped row keeps the newline.
        s.wrapped[0] = false;
        assert_eq!(extract_selection(&s, (0, 0), (4, 1)), "abcde\nfgh");
    }

    #[test]
    fn extract_selection_softwrap_keeps_meaningful_seam_space() {
        // A wrapped row ending in a MEANINGFUL space must NOT be trimmed at the seam, else
        // "word " + "next" fuses into "wordnext". (The trailing space is a real word boundary.)
        let mut s = snap(&["word ", "next "]);
        s.wrapped[0] = true;
        assert_eq!(extract_selection(&s, (0, 0), (4, 1)), "word next");
    }

    #[test]
    fn alt_screen_exit_fires_only_on_true_to_false_edge() {
        let mut prev = HashMap::new();
        let t = tid("t1");
        // First observation while already in alt screen: seeds true, not an exit.
        assert!(!detect_alt_screen_exit(&mut prev, &[(t.clone(), true)]));
        // Still in alt screen: no edge.
        assert!(!detect_alt_screen_exit(&mut prev, &[(t.clone(), true)]));
        // Leaves the alt screen: the true→false edge fires exactly once.
        assert!(detect_alt_screen_exit(&mut prev, &[(t.clone(), false)]));
        // Staying on the primary screen: no repeat fire.
        assert!(!detect_alt_screen_exit(&mut prev, &[(t.clone(), false)]));
        // Entering alt screen (false→true) is NOT an exit.
        assert!(!detect_alt_screen_exit(&mut prev, &[(t, true)]));
    }

    #[test]
    fn alt_screen_exit_drops_hidden_panes_and_never_fires_spuriously() {
        let mut prev = HashMap::new();
        let (a, b) = (tid("a"), tid("b"));
        // Pane a is in alt screen while visible; pane b is not.
        assert!(!detect_alt_screen_exit(
            &mut prev,
            &[(a.clone(), true), (b.clone(), false)]
        ));
        // Pane a becomes hidden (only b visible): a is dropped from the store, and b staying on
        // the primary screen must NOT be read as an exit.
        assert!(!detect_alt_screen_exit(&mut prev, &[(b.clone(), false)]));
        assert!(!prev.contains_key(&a));
        // Pane a reappears STILL on the primary screen (it left alt while hidden): because it was
        // dropped, it re-seeds as false and does not spuriously fire — the tab switch that
        // revealed it recomposes the whole frame, so there is no residue to clear here.
        assert!(!detect_alt_screen_exit(
            &mut prev,
            &[(a, false), (b, false)]
        ));
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn filter_env_keeps_only_whitelisted_last_wins() {
        let wl = vec!["DISPLAY".to_string(), "SSH_CONNECTION".to_string()];
        let got = filter_env(
            kv(&[("DISPLAY", ":0"), ("SECRET", "x"), ("DISPLAY", ":1")]),
            &wl,
        );
        // SECRET dropped; DISPLAY kept with the LAST value; SSH_CONNECTION absent (not added).
        assert_eq!(got, kv(&[("DISPLAY", ":1")]));
    }

    #[test]
    fn merge_env_overrides_then_appends() {
        let base = kv(&[("COPAD_MUX", "1"), ("DISPLAY", ":0")]);
        let over = kv(&[("DISPLAY", ":9"), ("SSH_CONNECTION", "1.2.3.4")]);
        // DISPLAY overridden in place; SSH_CONNECTION appended; COPAD_MUX untouched.
        assert_eq!(
            merge_env(&base, &over),
            kv(&[
                ("COPAD_MUX", "1"),
                ("DISPLAY", ":9"),
                ("SSH_CONNECTION", "1.2.3.4"),
            ])
        );
    }

    #[test]
    fn usage_threshold_colors_by_severity() {
        assert_eq!(usage_threshold_color(0.0), CAT_GREEN);
        assert_eq!(usage_threshold_color(69.0), CAT_GREEN);
        assert_eq!(usage_threshold_color(70.0), CAT_YELLOW); // 70% boundary → yellow
        assert_eq!(usage_threshold_color(89.0), CAT_YELLOW);
        assert_eq!(usage_threshold_color(90.0), CAT_RED); // 90% boundary → red
        assert_eq!(usage_threshold_color(100.0), CAT_RED);
    }

    #[test]
    fn tab_window_shows_all_when_they_fit() {
        let w = [4u16, 4, 4];
        assert_eq!(tab_window(&w, 1, 100), (0, 3, false, false));
    }

    #[test]
    fn tab_window_narrow_and_huge_are_safe() {
        assert_eq!(tab_window(&[4, 4, 4], 1, 0), (0, 0, false, false)); // no room
        assert_eq!(tab_window(&[], 0, 50), (0, 0, false, false)); // no tabs
        // A huge tab count must not overflow the width accumulator (u32 internally).
        let many = vec![9u16; 40000];
        let (s, e, _, _) = tab_window(&many, 20000, 30);
        assert!(s <= 20000 && 20000 < e);
    }

    #[test]
    fn tab_window_keeps_active_visible_with_overflow() {
        let w = [4u16; 10]; // 10 tabs, 4 cols each = 40; only ~12 cols available
        let (start, end, ml, mr) = tab_window(&w, 8, 12);
        assert!(start <= 8 && 8 < end, "active 8 must be in [{start},{end})");
        assert!(ml, "tabs before the window → left marker");
        // active is near the end, so nothing hidden on the right
        assert!(!mr || end < 10);
    }

    #[test]
    fn list_window_start_centers_active_and_clamps() {
        assert_eq!(list_window_start(3, 1, 5), 0); // all fit
        assert_eq!(list_window_start(20, 0, 6), 0); // active at top
        assert_eq!(list_window_start(20, 19, 6), 14); // active at bottom → clamp
        assert_eq!(list_window_start(20, 10, 6), 7); // centered (10 - 3)
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_subsequence() {
        assert!(fuzzy_match("", "anything")); // empty matches all
        assert!(fuzzy_match("api", "api-server"));
        assert!(fuzzy_match("api", "API-Server")); // case-insensitive
        assert!(fuzzy_match("aps", "api-server")); // subsequence (a..p..s)
        assert!(fuzzy_match("cld", "claude")); // c..l..d
        assert!(!fuzzy_match("xyz", "claude"));
        assert!(!fuzzy_match("sa", "api-server")); // order matters (no 's' before 'a')
    }

    #[test]
    fn shell_quote_neutralizes_metacharacters() {
        assert_eq!(shell_quote("claude"), "'claude'");
        // A `;` inside an arg stays inside the quotes — not a command separator.
        assert_eq!(shell_quote("a; rm -rf /"), "'a; rm -rf /'");
        // An embedded single quote is escaped.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn build_command_line_quotes_each_arg_and_preserves_boundaries() {
        // `claude "review; touch x"` stays TWO argv → one quoted arg, never re-split.
        let argv = vec!["claude".to_string(), "review; touch x".to_string()];
        assert_eq!(
            build_command_line(&argv).as_deref(),
            Some("'claude' 'review; touch x'")
        );
        assert_eq!(build_command_line(&[]), None);
    }

    #[test]
    fn menu_origin_flips_above_at_the_status_bar_and_clamps() {
        // Plenty of room below → opens below-right of the pointer.
        assert_eq!(menu_origin(10, 2, 14, 5, 100, 24), (10, 3));
        // Status-bar click (bottom row): no room below → flips fully above.
        assert_eq!(menu_origin(10, 23, 14, 5, 100, 24), (10, 18));
        // Near the right edge → clamped left so the box fits.
        assert_eq!(menu_origin(95, 2, 14, 5, 100, 24), (86, 3));
        // Tiny frame: clamps into bounds instead of overflowing.
        let (x0, y0) = menu_origin(0, 0, 14, 5, 10, 4);
        assert_eq!((x0, y0), (0, 0));
    }

    #[test]
    fn menu_item_at_maps_interior_rows_only() {
        let ws = crate::model::WorkspaceId::new("w");
        let m = Menu {
            title: "tab 1".into(),
            items: vec![
                ("rename tab", MenuAction::NewTab(ws.clone())),
                ("close tab", MenuAction::NewTab(ws.clone())),
                ("new tab", MenuAction::NewTab(ws)),
            ],
            x0: 10,
            y0: 18,
            w: 14,
            h: 5,
            sel: None,
            owner: crate::model::ClientId(1),
            held: true,
        };
        // Interior rows y0+1..y0+3 hit items 0..2; borders and outside miss.
        assert_eq!(m.item_at(12, 19), Some(0));
        assert_eq!(m.item_at(12, 21), Some(2));
        assert_eq!(m.item_at(12, 18), None, "top border row");
        assert_eq!(m.item_at(12, 22), None, "bottom border row");
        assert_eq!(m.item_at(10, 19), None, "left border column");
        assert_eq!(m.item_at(23, 19), None, "right border column");
        assert_eq!(m.item_at(30, 19), None, "outside");
    }
}
