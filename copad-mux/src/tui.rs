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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agentstate;
use crate::config::{self, Action, Chord, MuxConfig, SortBy, TabLabels, UsageLayout, UsageStyle};
use crate::control;
use crate::gitinfo;
use crate::gitpoll;
use crate::model::{
    ClientId, Dir, LayoutSpec, PaneId, PaneRect, Rect, Role, SplitTree, TabId, TerminalId,
    WorkspaceId,
};
use crate::navhist::{self, Loc};
use crate::notify;
use crate::persist::{self, MAX_LEAVES_PER_TAB, MAX_TOTAL_PANES, PLayout};
use crate::selector;
// The `Ctrl-f` switcher and the CLI's fuzzy picker share one filter, so typing the
// same query narrows both lists identically.
use crate::agentpoll;
use crate::agentsessions;
use crate::hostmetrics;
use crate::picker::fuzzy_match;
use crate::procinfo;
use crate::proto::MouseKind;
use crate::state::{Command, Event, MuxError, Origin, RestoredTab, State};
use crate::term::{self, CellColor, PaneTerm};
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
    /// The TAB it came from, titled by the same rule the sidebar uses
    /// ([`tab_display_title`]). A space can hold many agents, so the space name alone does
    /// not answer "which one is asking me" — the question a notification exists to raise.
    tab: String,
    body: String,
    /// The pane it came from — `Enter` in the center jumps here.
    terminal: TerminalId,
    /// Seen: by the same evidence that acknowledges a bell (a key or click while that pane
    /// is focused), or by jumping to it. Drives the `✉ N` chip and `Ctrl-b u`.
    read: bool,
}

/// A tab's display title: its custom name when it has a meaningful one, else its 1-based
/// INDEX (`tab 2` — the `Ctrl-b <n>` that jumps there).
///
/// One rule, one place. The sidebar's agent rows and the notifications now both title a tab
/// with it; two copies would drift, and a toast that names a tab differently from the row
/// you then go looking for is worse than a toast that names no tab at all.
fn tab_display_title(name: Option<&str>, index: usize) -> String {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("tab {}", index + 1))
}

/// Where a notification came from: `tool · space/tab`.
///
/// Each part is dropped when it is unknown rather than rendered as an empty segment — a
/// pane that could not be located must not produce a title ending in a bare `/`, which
/// reads as a real but nameless tab.
fn notify_title(tool: &str, space: &str, tab: &str) -> String {
    let place = match (space.is_empty(), tab.is_empty()) {
        (false, false) => format!("{space}/{tab}"),
        (false, true) => space.to_string(),
        (true, false) => tab.to_string(),
        (true, true) => String::new(),
    };
    if place.is_empty() {
        tool.to_string()
    } else {
        format!("{tool} · {place}")
    }
}

/// One `Ctrl-f` agent row: `codex · working 4m — running: cargo test  (space · tab 2)`.
///
/// The detail is what the switcher is opened for — "working" does not distinguish an agent
/// compiling for 40 seconds from one stuck in a retry loop. It is omitted entirely when there
/// is no reading, so a row never carries a placeholder standing in for information we do not
/// have.
fn agent_popup_text(
    tool: &str,
    status: &str,
    for_secs: u64,
    detail: Option<&str>,
    place: &str,
) -> String {
    let age = fmt_elapsed(for_secs);
    let head = if age.is_empty() {
        format!("{tool} · {status}")
    } else {
        format!("{tool} · {status} {age}")
    };
    match detail {
        Some(d) if !d.is_empty() => format!("{head} — {d}  ({place})"),
        _ => format!("{head}  ({place})"),
    }
}

/// One labelled reading on the top bar.
pub struct HostPart {
    pub label: String,
    pub value: String,
    pub color: Color,
}

/// The host readout, as label/value pairs. A field with no reading produces NO part — the bar
/// then shows less, rather than showing a zero that claims something about the machine.
///
/// Split out of the renderer so the composition is testable: `App` cannot be constructed in a
/// unit test, and "does an unreadable GPU disappear" is exactly the kind of thing that would
/// otherwise only be checked by looking at it.
pub fn host_parts(m: &hostmetrics::HostMetrics) -> Vec<HostPart> {
    let mut out = Vec::new();
    let mut pct = |label: &str, v: Option<f64>| {
        if let Some(v) = v {
            out.push(HostPart {
                label: label.to_string(),
                value: format!("{v:.0}%"),
                color: usage_threshold_color(v),
            });
        }
    };
    pct("cpu ", m.cpu);
    pct("mem ", m.mem_pct());
    pct("gpu ", m.gpu);
    if let Some(l) = m.load1 {
        // Load is NOT thresholded: what counts as high depends on the core count, and a
        // colour implying otherwise would be worse than no colour.
        out.push(HostPart {
            label: "load ".to_string(),
            value: format!("{l:.2}"),
            color: CAT_TEXT,
        });
    }
    out
}

/// A divider being dragged: which branch, and by whom.
///
/// The branch is held by PATH rather than by an adjacent pane, because `SplitTree::resize`
/// resolves deepest-first and a pane in an outer branch's `first` child can also sit inside a
/// nested branch on the same axis — dragging the outer divider would move the inner one.
#[derive(Debug, Clone)]
struct ResizeDrag {
    /// Where the branch lives. A drag applied to whatever tab happens to be active is how a
    /// gesture started in one tab silently resized another.
    ws: WorkspaceId,
    tab: TabId,
    path: Vec<bool>,
    dir: Dir,
    /// The branch's identity at grab time (see `DividerRect::ends`); the path alone is only a
    /// position, and a tree edit can leave a different branch at it.
    ends: (TerminalId, TerminalId),
    /// The view generation when it was grabbed. Compared, not the view itself: another client
    /// cycling A → B → A restores every view field while the pointer has moved elsewhere.
    view_gen: u64,
    owner: ClientId,
}

/// Where a pointer sits inside a branch, as a 0..1 share of its extent.
///
/// The divider itself occupies a cell, so the usable span is `extent - 1`; dividing by the
/// full extent would make the pane always a hair narrower than where the pointer is, and the
/// error grows as panes get small.
fn drag_ratio(pos: u16, origin: u16, extent: u16) -> Option<f32> {
    let span = extent.checked_sub(1)?;
    if span == 0 {
        return None;
    }
    Some(pos.saturating_sub(origin) as f32 / span as f32)
}

/// Max notifications retained in the center.
const NOTIFY_LOG_CAP: usize = 100;

/// Max hook-ownership records kept before pruning to live panes (see
/// [`App::claim_hook_kind`]).
const HOOK_OWNED_CAP: usize = 512;

/// Max attached-client pids remembered for window raising.
const MAX_CLIENT_PIDS: usize = 16;

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

/// A sidebar half scrolled BY HAND (wheel), plus the auto-anchor that was in effect at the
/// time. When the anchor moves on its own — you switch session, or the focused agent
/// changes — the override is ignored and the half snaps back to following the focus; a
/// panel that silently stopped tracking you would be worse than one that never scrolled.
#[derive(Debug, Clone)]
struct SidebarScroll {
    /// Index the half was scrolled to (a session index for `spaces`, an item index for
    /// `agents` — the two halves index different lists).
    start: usize,
    anchor: Option<String>,
}

/// What the last composed frame actually did with a sidebar half. A wheel event needs the
/// current start and how far it may go, and neither is derivable outside the render pass:
/// both depend on the MEASURED split (decision #94) and, for the agents half, on mixed row
/// heights. Written by `render_sidebar`, read by `scroll_sidebar`.
#[derive(Debug, Clone, Default)]
struct HalfView {
    start: usize,
    /// Highest start that still fills the window (0 when everything fits — nothing to scroll).
    max_start: usize,
    /// Stable IDENTITY of the row the window auto-anchors on — a `WorkspaceId` for `spaces`,
    /// a `TerminalId` for `agents` — and NOT its index. Under `sort_by = recent` the active
    /// session sits at display index 0 permanently, so an index-keyed pin would compare
    /// equal after every switch and never expire [codex S3a/C1].
    anchor: Option<String>,
}

/// An agent pane's rolled-up status plus WHEN it entered it. The stamp is what lets the
/// sidebar say `blocked 4m` rather than `blocked` — the difference between "needs me" and
/// "has been waiting on me for a while", which is the whole reason to scan the list.
///
/// comux tracks the transition ITSELF rather than reading Claude's `statusUpdatedAt`, so
/// Codex and anything else resolved by the screen-text fallback get the same readout.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AgentState {
    status: agentstate::AgentStatus,
    /// Monotonic instant the pane entered `status`; carried across refreshes while it holds.
    since: std::time::Instant,
    /// The mux-wide change counter ([`App::agent_seq`]) at the moment `status` was entered;
    /// carried forward with `since`. What `wait-agent --after-seq` compares against.
    seq: u64,
    /// The agent process the reading is for. A different pid in the same pane is a different
    /// agent, so it is a change even when the status happens to match.
    pid: u32,
}

/// One sidebar/switcher agent row: where the agent pane lives + what it runs.
struct AgentRow {
    /// Display title: the pane's TAB custom name when set (so multiple agents in one
    /// space stay distinguishable), else the session (space) name.
    title: String,
    /// The session (space) name — kept alongside `title` so the switcher can filter on
    /// it and show it even when a tab name overrides the title.
    space: String,
    /// What the agent was last seen DOING (`Bash: run the tests`), from `agentpoll`.
    /// `None` means "no reading" — the row then looks exactly as it did before the poller
    /// existed, rather than showing a placeholder or a stale value.
    detail: Option<String>,
    /// The session the agent lives in, so a sidebar space GROUP HEADER can click through
    /// to it (the name alone is ambiguous — two sessions may share one).
    space_id: WorkspaceId,
    /// The agent's foreground command (e.g. `claude` / `codex`).
    tool: String,
    /// Rolled-up status: `working` / `ready` / `blocked` / `idle`.
    status: &'static str,
    /// Whole seconds the agent has held `status` — rendered as `blocked 4m`.
    for_secs: u64,
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
#[derive(Clone, PartialEq, Eq)]
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

/// A status-bar outcome line ([`App::flash`]).
struct StatusMessage {
    text: String,
    error: bool,
    until: std::time::Instant,
}

/// The smallest frame the `Ctrl-f` switcher shows a pane preview in; below it, list only.
const POPUP_PREVIEW_MIN_COLS: u16 = 110;
const POPUP_PREVIEW_MIN_ROWS: u16 = 16;

/// How long a status message stays up. Errors linger: they are the ones worth reading.
const MESSAGE_SECS: u64 = 5;
const ERROR_MESSAGE_SECS: u64 = 10;

/// The `Ctrl-f` fuzzy switcher state: which tab, the typed filter, the selected row.
struct PopupState {
    tab: PopupTab,
    filter: String,
    sel: usize,
    /// What `sel` pointed at when the user last chose it. The rows re-sort live (agents by
    /// status) while the switcher is open, so an index alone would silently retarget the
    /// preview AND Enter; [`App::reconcile_popup`] follows this instead.
    pinned: Option<PopupTarget>,
}

/// A leading status operator in the switcher filter (agent-deck style): `!` working,
/// `@` blocked, `#` ready or idle. Only the FIRST character is an operator; the rest of the
/// filter is fuzzy-matched as before, so `@api` and `@ api` both mean "blocked, matching api".
fn split_status_op(filter: &str) -> (Option<&'static [&'static str]>, &str) {
    let op: Option<&'static [&'static str]> = match filter.chars().next() {
        Some('!') => Some(&["working"]),
        Some('@') => Some(&["blocked"]),
        Some('#') => Some(&["ready", "idle"]),
        _ => None,
    };
    match op {
        Some(op) => (Some(op), filter[1..].trim_start()),
        None => (None, filter),
    }
}

/// The row the switcher is ON: the pinned target wherever it sits now, else `sel` kept in
/// range. Everything that reads the selection — render, preview, Enter, ^R, moving — goes
/// through here, so a re-sort between the last reconcile and this read (the status sweep
/// runs after it in the server loop) can never retarget it.
fn pinned_sel(p: &PopupState, items: &[PopupRow]) -> usize {
    p.pinned
        .as_ref()
        .and_then(|t| items.iter().position(|r| r.target == *t))
        .unwrap_or_else(|| p.sel.min(items.len().saturating_sub(1)))
}

/// Switcher order for agents: the ones waiting on you first. Within a status the sidebar's
/// order (session, tab, pane) is kept.
fn status_rank(status: &str) -> u8 {
    match status {
        "blocked" => 0,
        "working" => 1,
        "ready" => 2,
        _ => 3,
    }
}

/// Keyboard-focus state for the always-on left sidebar (`Ctrl-b e`): which group
/// (spaces/agents) + the selected row. Arrows navigate, Enter selects, Esc exits.
struct SidebarFocus {
    tab: PopupTab,
    sel: usize,
}

/// The `Ctrl-b R` resume picker: past Claude/Codex conversations found on disk
/// (`agentsessions`), newest first, fuzzy-filtered.
struct ResumeState {
    /// Fuzzy filter typed so far — matched against tool, prompt and path together.
    filter: String,
    sel: usize,
    /// `Ctrl-a`: also list non-interactive runs (`claude -p` / SDK, `codex exec`), which
    /// are the overwhelming majority and almost never what the user is looking for.
    all: bool,
    /// Conversation id → the pane already running it, sampled when the picker opened.
    /// Selecting a live conversation JUMPS to that pane instead of resuming a second copy.
    /// Best-effort and local: it only sees this server's panes, so a conversation open in
    /// another comux server or a bare terminal is invisible here, exactly as it is to a
    /// hand-typed `claude --resume`.
    live: HashMap<String, TerminalId>,
}

/// One rendered row of the resume picker (post-filter).
#[derive(Clone)]
struct ResumeRow {
    tool: agentsessions::Tool,
    id: String,
    cwd: Option<PathBuf>,
    /// The prompt that opened the conversation (may be empty).
    title: String,
    /// `3h` / `2d` — how long ago the transcript was last written.
    age: String,
    /// The pane already running this conversation, if any.
    live: Option<TerminalId>,
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

/// Rows the optional top bar occupies.
const TOP_H: u16 = 1;

/// Below this many rows the top bar is suppressed whatever the config says — see
/// [`App::top_h`].
const MIN_ROWS_FOR_TOP: u16 = 8;

/// How long a completed transcript scan is reused before the resume picker asks for a new
/// one. Long enough that flipping the `Ctrl-a` scope or reopening the picker right away is
/// free, short enough that a conversation you just closed shows up.
const SCAN_TTL: Duration = Duration::from_secs(5);

/// How long an outstanding scan blocks a new one. Past this it is presumed stuck (a stalled
/// filesystem) and a replacement is allowed; its late result is dropped by request id.
const SCAN_STUCK: Duration = Duration::from_secs(30);

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
    /// Pids of currently attached clients, oldest first — the walk-up start point for
    /// raising the right terminal window after a notification jump.
    client_pids: Vec<u32>,
    /// `(client pid, COPAD_SOCKET, COPAD_PANEL_ID)` for clients running inside a copad tab.
    client_copad: Vec<(u32, String, String)>,
    /// Bumped by [`Self::note_view_moved`] on every active-view transition. A divider drag
    /// records it at grab time and dies if it moves — see that method for why a counter and
    /// not a comparison.
    view_gen: u64,
    /// Bells this server has ACKNOWLEDGED per pane, against `PaneTerm::bell_count`.
    /// Unacknowledged = `count > seen`. See `MuxListener::bells` for why both sides count
    /// rather than flag.
    bell_seen: HashMap<TerminalId, u64>,
    /// The logged agent-turn notifications (newest first), shown in the center.
    notifications: VecDeque<Notification>,
    /// `(pane, agent pid, kind)` triples whose notifications the agent's own hooks push
    /// (`comux notify`), so the status sweep must not toast them a second time. Keyed by
    /// pid, not pane alone: a later agent in the same pane is a different owner.
    hook_owned: HashSet<(TerminalId, u32, &'static str)>,
    /// When `Some(sel)`, the notification center (`Ctrl-b a`) is open at row `sel`.
    center: Option<usize>,
    /// When `Some`, the resume picker (`Ctrl-b R`) is open.
    resume: Option<ResumeState>,
    /// Where the detached transcript scanner publishes; read by [`Self::poll_agent_sessions`].
    sessions: agentsessions::Shared,
    /// The scan the picker renders. Copied out of `sessions` once per scan so rendering
    /// never touches the scanner's mutex, and kept across a rescan so reopening the picker
    /// shows the previous list instead of blanking to "scanning…".
    sessions_shown: Vec<agentsessions::Entry>,
    /// Request id of the newest scan asked for, the request id already displayed, whether
    /// that request covered headless titles, and when it was issued (a reopen within
    /// [`SCAN_TTL`] reuses the result instead of re-walking two directory trees).
    scan_req: u64,
    scan_shown: Option<u64>,
    scan_deep: bool,
    scan_at: Option<std::time::Instant>,
    /// Why the most recent pane spawn failed, recorded by [`Self::report_spawn_failure`]
    /// so the control API can report the SAME reason the TUI toasted. The CLI handlers
    /// detect failure by pane count and would otherwise only be able to say "could not
    /// spawn a shell", hiding the one detail that makes it fixable (fd exhaustion).
    /// Taken (not read) by whoever reports it, so a stale reason can't attach itself
    /// to a later, unrelated failure.
    last_spawn_error: Option<String>,
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
    /// When the labels were last refreshed (throttle the sweep to ~2 Hz).
    last_labels: std::time::Instant,
    /// Cumulative count of process sweeps that failed outright (see
    /// [`Self::label_sweeps_failed`]). Diagnostic only — never resets.
    label_sweeps_failed: u64,
    /// Per-agent rolled-up status (working/ready/blocked/idle) + when it was entered,
    /// refreshed at the label cadence from Claude's session file + a screen-text fallback
    /// (`agentstate`).
    agent_statuses: HashMap<TerminalId, AgentState>,
    /// Mux-wide count of observed agent status changes; stamps each [`AgentState::seq`].
    /// Monotonic for the server's life, so a reading never repeats.
    agent_seq: u64,
    /// Which pane made each tab (`new-tab`/`spawn --owner`), by its terminal. Saved with the
    /// layout and re-attached on restore. An entry whose tab or owner pane is gone means
    /// nothing; [`App::prune_mailboxes`] drops them.
    tab_owner: HashMap<TabId, TerminalId>,
    /// Each pane's inbox (`notify --to`). In memory only: a restart loses unread messages.
    inboxes: HashMap<TerminalId, Mailbox>,
    /// The whole-minute bucket of each agent's time-in-status as last REPORTED to the
    /// render loop, so a roll can be turned into a repaint (see `refresh_agent_statuses`).
    agent_minutes: HashMap<TerminalId, u64>,
    /// What `agentpoll` last reported each agent pane was DOING, as last reported to the
    /// render loop. Compared — not just rendered — because `agent_rows` reads the poller
    /// live: a hidden agent that changes tool while staying `Working` moves no other
    /// compared value, so without this its sidebar detail would freeze once
    /// `drain_pane_dirty` stopped repainting for hidden panes.
    agent_details: HashMap<TerminalId, String>,
    /// Panes with an UNACKNOWLEDGED bell, as last reported to the render loop. Same reason
    /// as `agent_details`: `bell()` / `bell_count()` are live reads that drive the `!`
    /// markers and the mux-wide `! N`, and a bell rung in a hidden pane is precisely the
    /// case that used to repaint only because that pane dirtied the frame.
    bells_reported: std::collections::HashSet<TerminalId>,
    /// The top bar's rendered host readings, as last reported to the render loop. Compared
    /// as what is DRAWN rather than raw floats, so a change below the printed precision does
    /// not cost a repaint — but the threshold COLOUR is part of that, not just the text:
    /// 69.8% and 70.2% both print `70%` while crossing green→yellow, and comparing the
    /// strings alone would leave that severity stale.
    host_reported: Vec<(String, String, Color)>,
    /// Per-session git branch (its focused pane's cwd) for the `spaces` subtitle,
    /// refreshed at the label cadence.
    branches: HashMap<WorkspaceId, String>,
    /// Monotonic counter for minting unique session (workspace) ids (`local` is the
    /// first, so new sessions start at `s1`).
    next_session: u64,
    /// Per-session last-activated tick: the session MRU behind `sort_by = recent`, the
    /// switcher's preselection and the kill/exit fallback. Bumped by [`Self::track_nav`]
    /// whenever the attached session differs from the last one it saw, so EVERY path that
    /// moves the view counts — not just the ones that happen to call `switch_session`.
    session_activity: HashMap<WorkspaceId, u64>,
    /// Monotonic tick source for `session_activity`.
    activity_clock: u64,
    /// The session `track_nav` last saw attached, so it bumps `session_activity` once per
    /// arrival instead of on every input.
    activity_ws: Option<WorkspaceId>,
    /// Back / forward / last navigation (decisions #140). Global, like the view it records.
    nav: navhist::NavHistory,
    /// When `Some`, an inline text prompt (new-session name / rename) is open and
    /// captures keystrokes until Enter (commit) or Esc (cancel).
    prompt: Option<Prompt>,
    /// When `Some`, a `y`/`n` confirm modal (e.g. kill-session) is open.
    confirm: Option<Confirm>,
    /// When `Some`, a right-click context menu (tmux `display-menu`) is open.
    menu: Option<Menu>,
    /// A short outcome line for something the user just did (a failed spawn, a worktree
    /// warning, `Ctrl-b <` with no history), shown in the status bar in place of the tab
    /// chips until it expires. Replaces the desktop toast these used to need, which never
    /// appeared at all under `COPAD_MUX_NOTIFY=0` or on a machine without a notifier.
    message: Option<StatusMessage>,
    /// When `Some(scroll)`, the `Ctrl-b ?` key help is open, scrolled `scroll` rows down
    /// (clamped at render, so a resize or a reload that shortens the list can't strand it).
    help: Option<usize>,
    /// Per-half manual scroll (wheel over the sidebar), each dropped as soon as its
    /// auto-anchor moves. `None` = follow the focus, which is the default behaviour.
    spaces_scroll: Option<SidebarScroll>,
    agents_scroll: Option<SidebarScroll>,
    /// What the last composed frame did with each half, so a wheel event can pick up from
    /// where the render left off. Interior mutability for the same reason `click_zones` has
    /// it: rendering takes `&self`.
    spaces_view: RefCell<HalfView>,
    agents_view: RefCell<HalfView>,
    /// Row at which the `agents` half started in the last composed frame — how a wheel
    /// event tells which half the pointer is over.
    sidebar_mid: Cell<u16>,
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
    /// Shared "what is each agent doing" readings, written by the `agentpoll` thread and
    /// read (never blocked on) when building agent rows.
    agent_poll: agentpoll::Shared,
    /// Working-tree state per repo for the sidebar subtitle (`gitpoll`); idle unless started.
    git_poll: gitpoll::Shared,
    /// Shared host CPU/memory/GPU/load reading, written by the `hostmetrics` poller.
    host_poll: hostmetrics::Shared,
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
    /// Whether SOME attached client currently has the prefix armed — drives the `^b`
    /// mode indicator in the status bar. The prefix itself stays strictly per-client
    /// (a `Ctrl-b` on one client must not be completable by another's key); this is a
    /// render-only summary the server recomputes each frame, because the composed frame
    /// is shared by every client. Same precedent as an open context menu or a drag
    /// selection: client-OWNED, but visible in everyone's view.
    prefix_armed: bool,
    /// When the carousel last advanced — a manual page OR an auto-rotate tick resets
    /// it, so `usage_rotate_secs` counts from the last change, not a fixed cadence
    /// (a manual scroll never jumps again immediately after).
    usage_rolled_at: std::time::Instant,
    /// Last-seen alternate-screen bit per VISIBLE pane, polled every render by
    /// [`Self::take_alt_screen_transition`]. Watching EITHER edge (primary↔alt) lets the
    /// render loop force a full repaint whenever a full-screen app (nvim, less, …) enters
    /// OR exits, wiping the incremental-diff residue the wholesale grid swap leaves behind
    /// — and, importantly, giving a clean baseline to an OUTER terminal (Windows Terminal
    /// over SSH, nested emulators) that can drop cells on such a large transition.
    alt_screen: HashMap<TerminalId, bool>,
    /// The in-progress mouse drag-selection, if any (tmux-style copy). `None` except between
    /// a button-down and its button-up; the highlight only shows during the drag.
    selection: Option<Sel>,
    /// An in-progress divider drag. Client-OWNED like `selection`, and for the same reason:
    /// input is multi-client, so only the client that grabbed a divider may move or release
    /// it. At most one at a time — two clients dragging the same divider would fight.
    resize_drag: Option<ResizeDrag>,
    /// Highest clipboard sequence already relayed out of a pane (OSC 52 passthrough). Guards
    /// against a write that reached its pane's slot late being broadcast after a newer one.
    /// `0` = nothing relayed yet; sequences are 1-based (see `term::CLIPBOARD_SEQ`).
    last_clipboard_seq: u64,
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

        // The whitelist a client may refresh. `config::from_raw` already keeps
        // `never_inherit` out of `update_environment`, but this is the list that decides what
        // a client can push back INTO a pane, so re-subtract here rather than let a future
        // refactor of the config quietly turn a scrubbed session marker into a refreshed one.
        let env_update: Vec<String> = cfg
            .update_environment
            .iter()
            .filter(|n| !cfg.never_inherit.iter().any(|d| d == *n))
            .cloned()
            .collect();
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
        let (ws, next_session, owners) = match restored {
            Some(r) => r,
            None => {
                let ws = WorkspaceId::new("local");
                let (_tab, _pane, term0) =
                    state.create_workspace(ws.clone(), None, Rect { cols, rows });
                let pt = PaneTerm::spawn_with_env(
                    cols.max(1),
                    rows.max(1),
                    None,
                    None,
                    &with_pane_token(boot_env.clone()),
                )
                .map_err(|e| io::Error::other(format!("failed to spawn shell PTY: {e}")))?;
                panes.insert(term0, pt);
                (ws, 1, HashMap::new())
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
            client_pids: Vec::new(),
            client_copad: Vec::new(),
            view_gen: 0,
            notifications: VecDeque::new(),
            hook_owned: HashSet::new(),
            center: None,
            resume: None,
            sessions: agentsessions::idle(),
            sessions_shown: Vec::new(),
            scan_req: 0,
            scan_shown: None,
            scan_deep: false,
            scan_at: None,
            last_spawn_error: None,
            sock_env,
            client_env,
            env_update,
            labels: HashMap::new(),
            last_labels: std::time::Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(std::time::Instant::now),
            label_sweeps_failed: 0,
            agent_statuses: HashMap::new(),
            agent_seq: 0,
            tab_owner: owners,
            inboxes: HashMap::new(),
            agent_minutes: HashMap::new(),
            agent_details: HashMap::new(),
            bells_reported: std::collections::HashSet::new(),
            host_reported: Vec::new(),
            branches: HashMap::new(),
            next_session,
            session_activity: HashMap::new(),
            activity_clock: 0,
            activity_ws: None,
            nav: navhist::NavHistory::default(),
            prompt: None,
            confirm: None,
            menu: None,
            help: None,
            message: None,
            spaces_scroll: None,
            agents_scroll: None,
            spaces_view: RefCell::new(HalfView::default()),
            agents_view: RefCell::new(HalfView::default()),
            sidebar_mid: Cell::new(0),
            sidebar_focus: None,
            cfg,
            click_zones: RefCell::new(Vec::new()),
            // Idle until `start_usage_poll` (called by the server); tests that build an
            // App without a server never spawn the poller thread.
            usage_poll: usagepoll::idle(),
            agent_poll: agentpoll::idle(),
            git_poll: gitpoll::idle(),
            host_poll: hostmetrics::idle(),
            version_poll: versionpoll::idle(),
            version_shown: None,
            usage_shown: None,
            usage_page: 0,
            prefix_armed: false,
            bell_seen: HashMap::new(),
            usage_rolled_at: std::time::Instant::now(),
            alt_screen: HashMap::new(),
            selection: None,
            resize_drag: None,
            last_clipboard_seq: 0,
        };
        app.reflow();
        app.track_nav();
        Ok(app)
    }

    /// The environment injected into a NEWLY spawned pane: the socket markers plus the
    /// active client's refreshed session vars (tmux `update-environment`). The daemon's
    /// own env was scrubbed of these names at startup, so this is the sole source of a
    /// pane's SSH/display session.
    fn pane_env(&self) -> Vec<(String, String)> {
        with_pane_token(merge_env(&self.sock_env, &self.client_env))
    }

    /// The session variables to hand any desktop command we spawn (notifier, and the
    /// click action it carries). The server scrubbed these out of its OWN environment at
    /// startup, so without this overlay a `notify-send` from a long-lived daemon has no
    /// `DBUS_SESSION_BUS_ADDRESS` to publish to and the toast silently never appears.
    fn desktop_env(&self) -> Vec<(String, String)> {
        let mut env = self.client_env.clone();
        // The click action re-enters through the CLI, which must find THIS server.
        if let Some((k, v)) = self.sock_env.iter().find(|(k, _)| k == "COPAD_MUX_SOCK") {
            env.push((k.clone(), v.clone()));
        }
        env
    }

    /// The shell command a toast click should run to land in `term`'s pane, or `None`
    /// when the pane has no token (it predates the feature) or our own path is unknown.
    fn jump_action(&self, term: &TerminalId) -> Option<String> {
        let token = self.panes.get(term)?.pane_token()?;
        let exe = std::env::current_exe().ok()?;
        Some(notify::action_command(
            &exe.to_string_lossy(),
            token,
            &self.desktop_env(),
        ))
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
                            s.status,
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
        self.top_h()
    }

    /// Rows the top bar occupies: `TOP_H` when it is enabled AND the terminal can spare them.
    ///
    /// The floor is not cosmetic. Every chrome row comes out of the pane grid, and at three
    /// rows total a top bar plus the status bar would leave one row for the shell — so a
    /// short terminal silently keeps its panes instead of honouring the config.
    fn top_h(&self) -> u16 {
        if self.cfg.top_bar && self.rows >= MIN_ROWS_FOR_TOP {
            TOP_H
        } else {
            0
        }
    }

    /// Height available to the pane grid, between the top bar and the bottom status bar.
    fn content_rows(&self) -> u16 {
        self.rows
            .saturating_sub(STATUS_H)
            .saturating_sub(self.top_h())
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

    /// The active tab's dividers in screen coordinates — the grab targets for a resize drag.
    fn dividers(&self) -> Vec<crate::model::DividerRect> {
        let mut out = Vec::new();
        if let Some(w) = self.state.workspace(&self.ws)
            && let Some(tab) = w.tab(&w.active_tab)
        {
            tab.layout.derive_dividers(
                self.content_x(),
                self.content_y(),
                self.content_cols(),
                self.content_rows(),
                &mut Vec::new(),
                &mut out,
            );
        }
        out
    }

    /// The divider under `(x, y)`, if any. Innermost first: nested dividers are emitted after
    /// their ancestor and can only ever be BESIDE it, never on top, so order is not load-
    /// bearing — but taking the last match keeps it deterministic if that ever changes.
    fn divider_at(&self, x: u16, y: u16) -> Option<crate::model::DividerRect> {
        self.dividers()
            .into_iter()
            .rfind(|d| x >= d.x && x < d.x + d.cols && y >= d.y && y < d.y + d.rows)
    }

    /// Move the dragged divider so it lands under the pointer. Returns whether anything
    /// changed, so a drag that stays within one cell does not churn the revision.
    fn drag_divider(&mut self, x: u16, y: u16) -> bool {
        let Some(drag) = self.resize_drag.clone() else {
            return false;
        };
        // Everything below cancels rather than guesses. A drag that can no longer be proved to
        // address the branch it grabbed must do NOTHING — the failure it replaces is moving a
        // divider the user cannot see.
        let same_view = drag.view_gen == self.view_gen
            && drag.ws == self.ws
            && self
                .state
                .workspace(&self.ws)
                .is_some_and(|w| w.active_tab == drag.tab);
        if !same_view {
            self.resize_drag = None;
            return false;
        }
        // Re-derive from the CURRENT layout: the branch's origin and extent move when the
        // window is resized or another client splits mid-drag.
        let Some(d) = self
            .dividers()
            .into_iter()
            .find(|d| d.path == drag.path && d.dir == drag.dir && d.ends == drag.ends)
        else {
            self.resize_drag = None; // gone, or a different branch now sits at that path
            return false;
        };
        let pos = if drag.dir == Dir::Right { x } else { y };
        let Some(want) = drag_ratio(pos, d.origin, d.extent) else {
            return false;
        };
        // Through the state machine, not into the tree: mutations are authorized, revved and
        // re-derived in one place (single-writer, spec §1). Addressed with the drag's OWN
        // workspace, so the target is what the gesture started on by construction.
        let changed = self
            .state
            .apply(Command::SetSplitRatio {
                origin: Origin::Client(self.client),
                workspace: drag.ws,
                path: drag.path,
                ratio: want,
            })
            .is_ok_and(|evs| !evs.is_empty());
        if changed {
            self.sync_sizes();
        }
        changed
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
        let ws = self.ws.clone();
        self.split_at(&ws, pane, dir, true);
    }

    /// Split `pane` of `ws` (any tab, any session), spawning the new pane's shell in the
    /// source's cwd (tmux `-c '#{pane_current_path}'`). `focus` moves focus to the new pane
    /// — only ever in the tab on screen; a split elsewhere moves nothing anyone sees.
    /// Returns the new pane's terminal, or `None` with the split rolled back.
    fn split_at(
        &mut self,
        ws: &WorkspaceId,
        pane: PaneId,
        dir: Dir,
        focus: bool,
    ) -> Option<TerminalId> {
        let cwd = self
            .state
            .workspace(ws)
            .and_then(|w| w.tab_of_pane(&pane))
            .and_then(|t| t.layout.terminal_of(&pane))
            .and_then(|t| self.terminal_cwd(t));
        let origin = self.origin_for(ws);
        let events = self
            .state
            .apply(Command::SplitPane {
                origin: origin.clone(),
                workspace: ws.clone(),
                pane,
                dir,
                if_rev: None,
            })
            .ok()?;
        let (nt, np) = events.iter().find_map(|e| match e {
            Event::PaneSplit {
                new_terminal,
                new_pane,
                ..
            } => Some((new_terminal.clone(), new_pane.clone())),
            _ => None,
        })?;
        let pane_env = self.pane_env();
        let made = match PaneTerm::spawn_with_env(
            self.cols.max(1),
            self.rows.max(1),
            None,
            cwd,
            &pane_env,
        ) {
            Ok(pt) => {
                self.panes.insert(nt.clone(), pt);
                // `sync_sizes` below only reaches the session on screen; a split in another
                // session's active tab re-divides ITS geometry, which the PTYs must follow.
                self.apply_resized_events(&events);
                if focus {
                    // tmux-style: focus the freshly created pane. Refused (and ignored) when
                    // it is not in the tab on screen.
                    let _ = self.state.apply(Command::FocusPane {
                        client: self.client,
                        pane: np,
                    });
                }
                Some(nt)
            }
            Err(reason) => {
                // PTY spawn failed — roll the split back so authoritative state
                // never holds a pane without a matching PaneTerm (which would
                // render blank + silently drop input). Closing `np` collapses the
                // branch and refocuses the original pane.
                let _ = self.state.apply(Command::ClosePane {
                    origin,
                    workspace: ws.clone(),
                    pane: np,
                    if_rev: None,
                });
                self.report_spawn_failure("split", &reason);
                None
            }
        };
        self.sync_sizes();
        made
    }

    /// The authority a mutation of `ws` runs under: this client's lease for the session it
    /// is attached to, the API's for any other (a CLI verb reaching a background session).
    fn origin_for(&self, ws: &WorkspaceId) -> Origin {
        if *ws == self.ws {
            Origin::Client(self.client)
        } else {
            Origin::Api
        }
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
            let ws = self.ws.clone();
            let _ = self.close_pane(&ws, pane);
        }
    }

    /// Close a specific pane of `ws` (used by the `x` key on the focused pane and by the
    /// control API's `close <index>`, which can reach a caller's background tab).
    fn close_pane(&mut self, ws: &WorkspaceId, pane: PaneId) -> Result<(), MuxError> {
        // Propagate the actor's verdict (e.g. `CannotCloseLastPane`) so the
        // control API reports a real failure instead of a false `ok`.
        let events = self.state.apply(Command::ClosePane {
            origin: self.origin_for(ws),
            workspace: ws.clone(),
            pane,
            if_rev: None,
        })?;
        for e in &events {
            if let Event::PaneClosed { terminal, .. } = e {
                self.panes.remove(terminal);
            }
        }
        // `sync_sizes` only reaches the session on screen.
        self.apply_resized_events(&events);
        self.sync_sizes();
        Ok(())
    }

    /// Create a new tab (a fresh single-pane layout) and switch to it, spawning its
    /// shell. Rolls the tab back if the PTY can't spawn (never leave a blank tab).
    fn new_tab(&mut self) -> Option<TerminalId> {
        // Inherit the current pane's cwd into the new tab's shell (tmux-style).
        let cwd = self.focused_cwd();
        let ws = self.ws.clone();
        self.new_tab_in(
            &ws,
            TabLaunch {
                cwd,
                activate: true,
                ..TabLaunch::default()
            },
        )
    }

    /// Create a tab in `ws` running what `launch` says, returning its terminal. A tab in the
    /// attached session runs under this client's lease, one anywhere else under the API's.
    /// Rolls the tab back and toasts if the PTY can't spawn, so state never holds a tab with
    /// no live terminal.
    fn new_tab_in(&mut self, ws: &WorkspaceId, launch: TabLaunch) -> Option<TerminalId> {
        let TabLaunch {
            cwd,
            activate,
            argv,
            env,
        } = launch;
        // Creating a tab makes it active, so it is a view transition like any other. Missing
        // it left a hole that survived a round trip: create a tab and close it again while a
        // divider is held, and every identity check passes on the way back.
        if activate && *ws == self.ws {
            self.note_view_moved();
        }
        let origin = self.origin_for(ws);
        let events = match self.state.apply(Command::NewTab {
            origin: origin.clone(),
            workspace: ws.clone(),
            activate,
        }) {
            Ok(e) => e,
            Err(_) => return None,
        };
        let mut created: Option<(TabId, TerminalId)> = None;
        for e in &events {
            if let Event::TabCreated { tab, terminal, .. } = e {
                created = Some((tab.clone(), terminal.clone()));
            }
        }
        let (tab_id, term) = created?;
        let mut pane_env = self.pane_env();
        pane_env.extend(env);
        let (cols, rows) = (self.cols.max(1), self.rows.max(1));
        let spawned = match &argv {
            Some(argv) => PaneTerm::spawn_argv(cols, rows, argv, cwd, &pane_env),
            None => PaneTerm::spawn_with_env(cols, rows, None, cwd, &pane_env),
        };
        let made = match spawned {
            Ok(pt) => {
                self.panes.insert(term.clone(), pt);
                Some(term)
            }
            Err(reason) => {
                // Spawn failed — close the just-created tab so state never holds a
                // tab whose pane has no live terminal (blank render + dropped input).
                let _ = self.state.apply(Command::CloseTab {
                    origin,
                    workspace: ws.clone(),
                    tab: tab_id,
                });
                self.report_spawn_failure("new tab", &reason);
                None
            }
        };
        // A new tab in the attached session may make the tab bar appear (1→2 tabs),
        // shrinking the content height → re-derive the viewport, not just PTY sizes.
        if *ws == self.ws {
            self.reflow();
        }
        made
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
        let ws = self.ws.clone();
        self.select_tab(ws, next);
        self.sync_sizes();
    }

    /// Switch to the tab at 0-based `index` (the tab bar shows 1-based labels, so
    /// Make `tab` active in `ws`. THE one place a tab selection happens.
    ///
    /// Every caller funnels here so `note_view_moved` cannot be forgotten: the eager
    /// cancellation of a divider drag has to cover EVERY successful view transition, and the
    /// four sites that used to apply `SelectTab` by hand were exactly how a `cycle_tab` slipped
    /// past it.
    fn select_tab(&mut self, ws: WorkspaceId, tab: TabId) {
        let _ = self.state.apply(Command::SelectTab {
            origin: Origin::Client(self.client),
            workspace: ws,
            tab,
        });
        self.note_view_moved();
    }

    /// Record that what the user is looking at has changed.
    ///
    /// A MONOTONIC counter, not a comparison of the current view: another client can cycle
    /// A → B → A while you hold a divider, and every "are we still on the tab we grabbed in"
    /// check passes on the way back — while your pointer has moved somewhere unrelated, so a
    /// release would snap the divider to it.
    /// EVERY path that changes which tab or session is on screen must call this: selecting a
    /// tab, switching session, creating a tab (which activates it) and closing one (which
    /// promotes a neighbour). Adding a fifth without calling it reopens the round-trip hole.
    fn note_view_moved(&mut self) {
        self.view_gen = self.view_gen.wrapping_add(1);
        // A drag-selection is already invalidated by a view change; a divider drag now is too.
        self.selection = None;
        self.resize_drag = None;
    }

    /// `Ctrl-b 1` → index 0). Out-of-range is ignored.
    fn select_tab_index(&mut self, index: usize) {
        if let Some(tab) = self.tab_ids().get(index).cloned() {
            let ws = self.ws.clone();
            self.select_tab(ws, tab);
            self.sync_sizes();
        }
    }

    /// Close the active tab (and reap its shells). Rejected when it is the last tab.
    fn close_active_tab(&mut self) {
        let Some(active) = self.active_tab_id() else {
            return;
        };
        let _ = self.close_tab(&self.ws.clone(), active);
    }

    /// Close a specific tab of `ws` (context menu can target a non-active one) and reap
    /// its shells. Rejected when it is the workspace's last tab — the key/menu callers
    /// discard that `Err` (nothing to say), while the control API turns it into the
    /// response's error string.
    fn close_tab(&mut self, ws: &WorkspaceId, tab: TabId) -> Result<(), MuxError> {
        // Closing the active tab promotes a neighbour — another view transition. Bumped BEFORE
        // the apply so a refusal (the last tab) still cancels: over-cancelling a drag is a
        // shrug, moving the wrong divider is not.
        self.note_view_moved();
        // `?` on the last-tab refusal: the tab is kept and the caller decides whether
        // that is worth reporting.
        // The lease only covers the attached session; a tab in a background session (a
        // `close-tab s2:t1` from the CLI) is closed on the API's authority instead.
        let origin = if *ws == self.ws {
            Origin::Client(self.client)
        } else {
            Origin::Api
        };
        let events = self.state.apply(Command::CloseTab {
            origin,
            workspace: ws.clone(),
            tab,
        })?;
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
        Ok(())
    }

    /// Make `wid` the active session, re-attaching the local client as its controller
    /// (takeover releases the previous session's lease → it freezes, G3; shells keep
    /// running in the background). Reflows the new session to the current size.
    fn switch_session(&mut self, wid: WorkspaceId) {
        if self.ws == wid || self.state.workspace(&wid).is_none() {
            return;
        }
        self.note_view_moved();
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

    // --- navigation history (decisions #140) ---

    /// Where the view is right now: the attached session, its active tab, the focused pane.
    fn nav_loc(&self) -> Option<Loc> {
        let w = self.state.workspace(&self.ws)?;
        Some(Loc {
            ws: self.ws.clone(),
            tab: w.active_tab.clone(),
            term: self.focused_terminal(),
        })
    }

    /// Observe the landed view once per handled input (the public entry points wrap their
    /// bodies so every early return passes through here). Prunes dead history and recency
    /// first, which is what keeps reaping and background kills — paths that never call a
    /// navigation helper — from leaving stale entries behind.
    fn track_nav(&mut self) {
        let live: HashSet<WorkspaceId> = self.session_ids().into_iter().collect();
        self.session_activity.retain(|w, _| live.contains(w));
        let state = &self.state;
        self.nav.prune(|l| loc_live(state, l));
        if live.contains(&self.ws) {
            self.note_arrival();
        }
        if let Some(now) = self.nav_loc() {
            self.nav.observe(now);
        }
    }

    /// Bump the attached session's recency, once per arrival.
    fn note_arrival(&mut self) {
        if self.activity_ws.as_ref() == Some(&self.ws) {
            return;
        }
        self.activity_clock += 1;
        self.session_activity
            .insert(self.ws.clone(), self.activity_clock);
        self.activity_ws = Some(self.ws.clone());
    }

    /// Live sessions, most recently attached first; never-attached ones (restored, or made
    /// in the background) follow in creation order so every session stays reachable.
    fn mru_sessions(&self) -> Vec<WorkspaceId> {
        let mut ids = self.session_ids();
        // Stable: ties (all the never-visited zeros) keep creation order.
        ids.sort_by_key(|w| std::cmp::Reverse(self.session_activity.get(w).copied().unwrap_or(0)));
        ids
    }

    /// The session to land on when the attached one goes away: the most recent survivor.
    fn fallback_session(&self, dying: &WorkspaceId) -> Option<WorkspaceId> {
        self.mru_sessions().into_iter().find(|w| w != dying)
    }

    /// [`Self::navigate`] from a key: an empty direction says so instead of doing nothing.
    fn navigate_or_say(&mut self, dir: control::NavDir) {
        if self.navigate(dir).is_none() {
            let what = match dir {
                control::NavDir::Back | control::NavDir::Last => {
                    "no earlier session/tab to go back to"
                }
                control::NavDir::Forward => "nothing to go forward to",
            };
            self.flash(what.to_string(), false);
        }
    }

    /// `session/tab` for a location, by display name — what `comux back` prints.
    fn loc_label(&self, loc: &Loc) -> String {
        let w = self.state.workspace(&loc.ws);
        let index = w
            .and_then(|w| w.tabs.iter().position(|t| t.id == loc.tab))
            .unwrap_or(0);
        let tab = w
            .and_then(|w| w.tab(&loc.tab))
            .and_then(|t| t.name.as_deref());
        format!(
            "{}/{}",
            self.session_display_name(&loc.ws),
            tab_display_title(tab, index)
        )
    }

    /// Put the view at `loc`: its session, its tab, and its pane if that is still there.
    fn go_to_loc(&mut self, loc: &Loc) {
        self.switch_session(loc.ws.clone());
        let active = self.active_tab_id();
        if active.as_ref() != Some(&loc.tab) {
            self.select_tab(loc.ws.clone(), loc.tab.clone());
        }
        if let Some(term) = &loc.term
            && let Some(pane) = self.pane_of_terminal(term)
        {
            let _ = self.state.apply(Command::FocusPane {
                client: self.client,
                pane,
            });
        }
        self.reflow();
    }

    /// Move along the history. Returns where the view landed, or `None` when there is
    /// nowhere to go in that direction.
    fn navigate(&mut self, dir: control::NavDir) -> Option<Loc> {
        // Settle anything the input that led here did first (a click, then the key).
        self.track_nav();
        let state = &self.state;
        let valid = |l: &Loc| loc_live(state, l);
        let target = match dir {
            control::NavDir::Back => self.nav.back(valid),
            control::NavDir::Forward => self.nav.forward(valid),
            control::NavDir::Last => self.nav.last(valid),
        }?;
        self.go_to_loc(&target);
        // The entry's pane may have died; record where the view ACTUALLY is.
        let landed = self.nav_loc()?;
        self.nav.settle(landed.clone());
        self.note_arrival();
        Some(landed)
    }

    /// The working directory of the focused pane's shell, for inheriting into a new
    /// session/tab/split (tmux `-c '#{pane_current_path}'`). `None` if unreadable.
    fn focused_cwd(&self) -> Option<PathBuf> {
        self.focused_terminal().and_then(|t| self.terminal_cwd(&t))
    }

    /// The working directory of `term`'s shell. `None` if unreadable.
    fn terminal_cwd(&self, term: &TerminalId) -> Option<PathBuf> {
        self.panes
            .get(term)
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
            Ok(pt) => {
                self.panes.insert(term0, pt);
                self.switch_session(id);
            }
            Err(reason) => {
                // Spawn failed — drop the just-created session so state never holds a
                // session whose pane has no live terminal.
                if let Some(terms) = self.state.remove_workspace(&id) {
                    for t in &terms {
                        self.panes.remove(t);
                    }
                }
                self.report_spawn_failure("new session", &reason);
            }
        }
    }

    /// Tell the user why a pane they just asked for did not appear: on the status-bar
    /// message line (it used to be a desktop toast, which `COPAD_MUX_NOTIFY=0` or a machine
    /// without a notifier silently swallowed), and in `last_spawn_error` for the control
    /// API to report the same reason.
    fn report_spawn_failure(&mut self, what: &str, reason: &str) {
        self.last_spawn_error = Some(reason.to_string());
        self.flash(format!("{what} failed: {reason}"), true);
    }

    /// Tell the user something about an action they just took, on the message line.
    fn report_note(&mut self, body: &str) {
        self.flash(body.to_string(), false);
    }

    /// Show `text` in the status bar for a few seconds (see [`App::message`]).
    fn flash(&mut self, text: String, error: bool) {
        let secs = if error {
            ERROR_MESSAGE_SECS
        } else {
            MESSAGE_SECS
        };
        self.message = Some(StatusMessage {
            // One line: a multi-line reason would otherwise draw control characters.
            text: text.split_whitespace().collect::<Vec<_>>().join(" "),
            error,
            until: std::time::Instant::now() + Duration::from_secs(secs),
        });
    }

    /// Drop an expired status message. Returns whether the bar changed, for the render loop —
    /// nothing else repaints an otherwise idle screen when a message times out.
    pub fn maybe_expire_message(&mut self) -> bool {
        if self
            .message
            .as_ref()
            .is_some_and(|m| std::time::Instant::now() >= m.until)
        {
            self.message = None;
            return true;
        }
        false
    }

    /// Drop any recorded spawn reason before attempting a new pane-creating mutation.
    ///
    /// Without this, a reason left behind by a TUI keypress sits in the field
    /// indefinitely, and the next control request that fails for an entirely different
    /// cause — one the actor rejected before any spawn was attempted — would take it and
    /// report a descriptor problem that had nothing to do with it.
    fn clear_spawn_error(&mut self) {
        self.last_spawn_error = None;
    }

    /// The reason the last pane spawn failed, consumed for a control-API error line.
    /// Falls back to the generic wording when a failure had no recorded reason (the
    /// actor refused the mutation before any spawn was attempted).
    fn take_spawn_error(&mut self, what: &str) -> String {
        match self.last_spawn_error.take() {
            Some(reason) => format!("{what} failed: {reason}"),
            None => format!("{what} failed (could not spawn a shell)"),
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
            message: format!("kill session '{name}'? (y = kill, any other key cancels)"),
            action: ConfirmAction::KillSession(ws),
        });
    }

    fn perform_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::KillSession(wid) => {
                self.kill_session(&wid);
            }
        }
    }

    /// Remove a session (workspace), reap its PTYs, and switch to a survivor if it was
    /// active. Revalidates the target still exists and isn't the last session (it may
    /// have changed between opening the confirm and pressing `y`). Returns whether the
    /// session was actually removed — the key/menu paths ignore it (the confirm modal
    /// already vetted the target), the control API reports it.
    fn kill_session(&mut self, wid: &WorkspaceId) -> bool {
        if self.state.workspace(wid).is_none() || self.state.workspace_count() <= 1 {
            return false;
        }
        let killing_active = &self.ws == wid;
        if let Some(terms) = self.state.remove_workspace(wid) {
            for t in &terms {
                self.panes.remove(t);
            }
        }
        if killing_active && let Some(surv) = self.fallback_session(wid) {
            // `self.ws` still names the removed session, so `switch_session` proceeds.
            self.switch_session(surv);
        }
        true
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
                // The CLI returns errors/warnings directly; here a failure OR a non-fatal
                // hook warning goes to the status-bar message line. Success needs no note —
                // the session visibly switched.
                match self.create_worktree_session(&branch, "", self.focused_cwd()) {
                    Ok(c) => {
                        if let Some(w) = c.warning {
                            self.flash(format!("worktree: {w}"), false);
                        }
                    }
                    Err(e) => self.flash(format!("worktree: {e}"), true),
                }
            }
            PromptKind::RenameSession(ws) => self.rename_session(&ws, name),
            PromptKind::RenameTab(ws, tab) => self.rename_tab(&ws, &tab, name),
        }
    }

    /// Switch to the next/previous session (wrapping): `+1` next, `-1` previous.
    fn cycle_session(&mut self, delta: i32) {
        // Cycle in the order the sidebar SHOWS when that order is stable, so `)` moves to the
        // row below. `recent` and `activity` reorder as you go (cycling `recent` would
        // ping-pong between two sessions and never reach the rest), so those cycle in
        // creation order, which always visits every session.
        let ids = match self.cfg.sort_by {
            SortBy::Created | SortBy::Alphabetical => self.sorted_session_ids(),
            SortBy::Recent | SortBy::Activity => self.session_ids(),
        };
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
            .filter(|(_, s)| s.status == agentstate::AgentStatus::Blocked)
            .map(|(tid, _)| tid.clone())
            .collect();
        if blocked.is_empty() {
            return;
        }
        // Longest-waiting first: the queue order an attention key should walk. Ties (and the
        // ordering among equally old ones) fall back to the id so the cycle is stable.
        blocked.sort_by_key(|t| (self.agent_statuses.get(t).map(|s| s.since), t.to_string()));
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

    /// Resolve a pane addressed by identity (not position): a `$COPAD_MUX_PANE` token
    /// first, then a raw terminal id.
    ///
    /// Tokens are qualified by server incarnation, so one minted by a PREVIOUS server —
    /// e.g. sitting in a toast that survived a restart — resolves to nothing here and the
    /// caller reports "unknown pane". That is the point: terminal ids restart at `term0`
    /// with the server, so accepting a bare id from a stale notification would jump
    /// confidently to an unrelated pane.
    fn resolve_pane_target(&self, target: &str) -> Option<TerminalId> {
        if let Some((tid, _)) = self
            .panes
            .iter()
            .find(|(_, p)| p.pane_token() == Some(target))
        {
            return Some(tid.clone());
        }
        let id = TerminalId::new(target);
        self.panes.contains_key(&id).then_some(id)
    }

    /// Where a terminal sits, for listings: `(tab id, tab name, tab index, pane index)`.
    fn tab_location(&self, term: &TerminalId) -> Option<(TabId, String, usize, usize)> {
        let (ws, tab, pane) = self.locate_terminal(term)?;
        let w = self.state.workspace(&ws)?;
        let ti = w.tabs.iter().position(|t| t.id == tab)?;
        let t = &w.tabs[ti];
        let pi = t.layout.panes().iter().position(|p| *p == pane)?;
        Some((tab, t.name.clone().unwrap_or_default(), ti, pi))
    }

    /// Sessions → tabs → panes, for `list-tree`. `only` narrows it to one session, `owner` to
    /// the tabs that pane owns (dropping sessions left with none).
    fn tree(
        &self,
        only: Option<&WorkspaceId>,
        owner: Option<&TerminalId>,
    ) -> Vec<control::TreeSession> {
        self.session_ids()
            .into_iter()
            .enumerate()
            .filter(|(_, wid)| only.is_none_or(|o| o == wid))
            .filter_map(|(si, wid)| {
                let w = self.state.workspace(&wid)?;
                let tabs: Vec<control::TreeTab> = w
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| {
                        owner.is_none_or(|o| {
                            self.tab_owner.get(&t.id) == Some(o) && self.panes.contains_key(o)
                        })
                    })
                    .map(|(ti, t)| control::TreeTab {
                        index: ti,
                        id: t.id.to_string(),
                        name: t.name.clone().unwrap_or_default(),
                        active: t.id == w.active_tab,
                        owner: self.owner_token(&t.id),
                        panes: t
                            .layout
                            .panes()
                            .iter()
                            .enumerate()
                            .filter_map(|(pi, p)| {
                                let tid = t.layout.terminal_of(p)?;
                                let label = self.labels.get(tid);
                                let kind = match label.map(|l| l.kind) {
                                    Some(procinfo::Kind::Agent) => "agent",
                                    Some(procinfo::Kind::Shell) => "shell",
                                    _ => "other",
                                };
                                Some(control::TreePane {
                                    index: pi,
                                    token: self
                                        .panes
                                        .get(tid)
                                        .and_then(|pt| pt.pane_token())
                                        .unwrap_or_default()
                                        .to_string(),
                                    terminal: tid.to_string(),
                                    focused: *p == t.focused,
                                    label: label.map(|l| l.text.clone()).unwrap_or_default(),
                                    kind: kind.to_string(),
                                    status: if kind == "agent" {
                                        self.agent_status(tid).0.to_string()
                                    } else {
                                        String::new()
                                    },
                                })
                            })
                            .collect(),
                    })
                    .collect();
                if owner.is_some() && tabs.is_empty() {
                    return None;
                }
                Some(control::TreeSession {
                    index: si,
                    id: wid.to_string(),
                    name: w.name.clone().unwrap_or_default(),
                    active: wid == self.ws,
                    tabs,
                })
            })
            .collect()
    }

    /// The response for a create verb: where the new pane landed (session + tab, by id and
    /// name) and its token — so a script never has to find what it made by diffing listings.
    fn created(&self, term: &TerminalId) -> control::Resp {
        let token = self
            .panes
            .get(term)
            .and_then(|p| p.pane_token())
            .unwrap_or_default()
            .to_string();
        let Some((ws, tab, _)) = self.locate_terminal(term) else {
            return control::Resp::split(token);
        };
        let w = self.state.workspace(&ws);
        let session = control::NodeRef {
            id: ws.to_string(),
            name: w.and_then(|w| w.name.clone()).unwrap_or_default(),
        };
        let tab = control::NodeRef {
            name: w
                .and_then(|w| w.tab(&tab))
                .and_then(|t| t.name.clone())
                .unwrap_or_default(),
            id: tab.to_string(),
        };
        control::Resp::created(session, tab, token)
    }

    /// Every session and its tabs as the selector resolver sees them, in `session_ids` order.
    fn selector_snapshot(&self) -> SelectorSnap {
        self.session_ids()
            .into_iter()
            .filter_map(|wid| {
                let w = self.state.workspace(&wid)?;
                let tabs = w
                    .tabs
                    .iter()
                    .map(|t| (t.id.clone(), t.name.clone()))
                    .collect();
                Some((wid.clone(), w.name.clone(), tabs))
            })
            .collect()
    }

    /// Resolve a session address: an index (creation order) or a selector. Both, or neither,
    /// is `invalid`; a miss is `not_found`; a name matching several sessions is `ambiguous`.
    fn session_addr(
        &self,
        index: Option<usize>,
        target: Option<&str>,
    ) -> Result<WorkspaceId, AddrErr> {
        let snap = self.selector_snapshot();
        match (index, target) {
            (Some(_), Some(_)) => Err(addr_err("invalid", "give an index or a target, not both")),
            (None, None) => Err(addr_err("invalid", "no session given")),
            (Some(i), None) => snap
                .get(i)
                .map(|(w, _, _)| w.clone())
                .ok_or_else(|| addr_err("not_found", format!("no session at index {i}"))),
            (None, Some(sel)) => {
                let views = selector_views(&snap);
                selector::session(&views, sel)
                    .map(|i| snap[i].0.clone())
                    .map_err(|e| (e.code(), e.message().to_string()))
            }
        }
    }

    /// Resolve a tab address: an index into the ACTIVE session's tabs, or a selector (which
    /// may name a tab in any session).
    fn tab_addr(
        &self,
        index: Option<usize>,
        target: Option<&str>,
    ) -> Result<(WorkspaceId, TabId), AddrErr> {
        let snap = self.selector_snapshot();
        let active = snap.iter().position(|(w, _, _)| *w == self.ws).unwrap_or(0);
        let pick = match (index, target) {
            (Some(_), Some(_)) => {
                return Err(addr_err("invalid", "give an index or a target, not both"));
            }
            (None, None) => return Err(addr_err("invalid", "no tab given")),
            (Some(i), None) => selector::tab(&selector_views(&snap), active, &i.to_string()),
            (None, Some(sel)) => selector::tab(&selector_views(&snap), active, sel),
        };
        pick.map(|(si, ti)| (snap[si].0.clone(), snap[si].2[ti].0.clone()))
            .map_err(|e| (e.code(), e.message().to_string()))
    }

    /// Where the caller's pane lives, for a verb that defaults to "the caller's tab/session".
    /// Token-only, like every write: a raw terminal id can name an unrelated pane after a
    /// server restart.
    fn caller_location(&self, token: &str) -> Result<(WorkspaceId, TabId, PaneId), String> {
        self.resolve_pane_token(token)
            .and_then(|tid| self.locate_terminal(&tid))
            .ok_or_else(|| {
                format!("unknown pane '{token}' (the calling pane is not on this server)")
            })
    }

    /// The tab an index counts in: the CALLER's (named by its pane token), wherever it is, else
    /// the tab on screen. Counting in the caller's tab is what keeps `list` and `send N` /
    /// `focus N` / `close N` pointing at the same pane for an agent in a background tab.
    fn index_tab(&self, caller: Option<&str>) -> Result<(WorkspaceId, TabId), AddrErr> {
        match caller {
            Some(t) => self
                .caller_location(t)
                .map(|(ws, tab, _)| (ws, tab))
                .map_err(|m| addr_err("not_found", m)),
            None => self
                .state
                .workspace(&self.ws)
                .map(|w| (self.ws.clone(), w.active_tab.clone()))
                .ok_or_else(|| addr_err("error", "no workspace")),
        }
    }

    /// The pane at `index` of [`Self::index_tab`]'s tab: `(workspace, pane, its terminal)`.
    fn pane_at(
        &self,
        caller: Option<&str>,
        index: usize,
    ) -> Result<(WorkspaceId, PaneId, Option<TerminalId>), AddrErr> {
        let (ws, tab) = self.index_tab(caller)?;
        let t = self
            .state
            .workspace(&ws)
            .and_then(|w| w.tab(&tab))
            .ok_or_else(|| addr_err("not_found", "the tab is gone"))?;
        let pane = t
            .layout
            .panes()
            .get(index)
            .cloned()
            .ok_or_else(|| addr_err("not_found", format!("no pane at index {index}")))?;
        let term = t.layout.terminal_of(&pane).cloned();
        Ok((ws, pane, term))
    }

    /// Resolve a pane by its `$COPAD_MUX_PANE` TOKEN only — never a raw terminal id.
    ///
    /// Terminal ids restart at `term0` on every server incarnation, so a recorded one can
    /// address a completely different pane after a restart. A read that lands on the wrong
    /// pane returns a wrong answer; a WRITE types into someone's live shell. Writes therefore
    /// use this, and only the incarnation-qualified token resolves.
    fn resolve_pane_token(&self, token: &str) -> Option<TerminalId> {
        self.panes
            .iter()
            .find(|(_, p)| p.pane_token() == Some(token))
            .map(|(tid, _)| tid.clone())
    }

    /// A live pane's token.
    fn token_of(&self, term: &TerminalId) -> Option<String> {
        self.panes
            .get(term)
            .and_then(|p| p.pane_token())
            .map(str::to_string)
    }

    /// `(tab, owner terminal)` for every owned tab that still exists and whose owner pane is
    /// still alive, in tab order.
    fn live_owners(&self) -> Vec<(TabId, TerminalId)> {
        self.state
            .workspace_ids()
            .iter()
            .filter_map(|w| self.state.workspace(w))
            .flat_map(|w| w.tabs.iter())
            .filter_map(|t| {
                let owner = self.tab_owner.get(&t.id)?;
                self.panes
                    .contains_key(owner)
                    .then(|| (t.id.clone(), owner.clone()))
            })
            .collect()
    }

    /// The token of the pane that owns `tab`, while it lives.
    fn owner_token(&self, tab: &TabId) -> Option<String> {
        self.tab_owner.get(tab).and_then(|o| self.token_of(o))
    }

    /// Forget owners of closed tabs / dead panes, and the inboxes of dead panes. Lazy (on
    /// every owner/inbox write) rather than on each teardown path, which are many.
    fn prune_mailboxes(&mut self) {
        let live: HashMap<TabId, TerminalId> = self.live_owners().into_iter().collect();
        self.tab_owner = live;
        let panes = &self.panes;
        self.inboxes.retain(|tid, _| panes.contains_key(tid));
    }

    /// The terminal-emulator process an attached client is running inside, for the CLI to
    /// activate after a jump. Best-effort and app-level only: a pid identifies the
    /// emulator, never which of its windows or native tabs holds the client.
    ///
    /// The most recently attached client wins — with several attached, that is the one the
    /// user is most likely sitting at. A pid that no longer resolves to a terminal (it
    /// exited, or its client was removed by a path that skipped bookkeeping) is skipped
    /// rather than returned as `None`: the raise must not go dark because a stale entry
    /// happens to sit on top.
    fn raise_target(&self) -> Option<(u32, String)> {
        self.client_pids
            .iter()
            .rev()
            .find_map(|pid| crate::winfocus::terminal_ancestor_live(*pid))
    }

    /// Record that the client with `pid` is running inside a copad tab, and which one.
    ///
    /// This is what makes the notification jump land on the right TAB rather than merely
    /// bringing copad forward. Keyed by client pid so it is dropped by the same
    /// `forget_client_pid` that drops the raise target — a departed client must not leave a
    /// panel id behind for a later jump to focus.
    pub fn set_client_copad(&mut self, pid: u32, sock: String, panel: String) {
        self.client_copad.retain(|(p, _, _)| *p != pid);
        self.client_copad.push((pid, sock, panel));
        if self.client_copad.len() > MAX_CLIENT_PIDS {
            self.client_copad.remove(0);
        }
    }

    /// The copad tab hosting the most recently attached client, if any.
    ///
    /// Most-recent-first, matching [`Self::raise_target`]: with several clients attached the
    /// one you just used is the one you meant.
    fn copad_target(&self) -> Option<(String, String)> {
        let live: Vec<&(u32, String, String)> = self
            .client_copad
            .iter()
            .filter(|(p, _, _)| self.client_pids.contains(p))
            .collect();
        live.last().map(|(_, s, panel)| (s.clone(), panel.clone()))
    }

    /// Record an attaching client's pid (most recent last) so [`Self::raise_target`] can
    /// find its terminal. Called by the server on attach.
    pub fn set_client_pid(&mut self, pid: u32) {
        self.client_pids.retain(|p| *p != pid);
        self.client_pids.push(pid);
        if self.client_pids.len() > MAX_CLIENT_PIDS {
            self.client_pids.remove(0);
        }
    }

    /// Forget a client's pid on detach — a departed client's terminal must not be raised.
    pub fn forget_client_pid(&mut self, pid: u32) {
        self.client_pids.retain(|p| *p != pid);
        self.client_copad.retain(|(p, _, _)| *p != pid);
    }

    /// The attached-client pids currently considered for a window raise, oldest first.
    /// Observability for the detach paths (a stale entry here shadows the live client).
    pub fn raise_candidates(&self) -> &[u32] {
        &self.client_pids
    }

    // --- control API (spec §3): applied on the main loop (single writer) ---

    /// Handle one control request, mutating state as the sole writer. Returns the
    /// wire response. Never blocks.
    pub fn handle_control(&mut self, req: &control::Req) -> control::Resp {
        let resp = self.handle_control_inner(req);
        self.track_nav();
        resp
    }

    fn handle_control_inner(&mut self, req: &control::Req) -> control::Resp {
        use control::{PaneInfo, Req, Resp};
        match req {
            Req::Host => {
                let m = hostmetrics::read(&self.host_poll);
                Resp {
                    host: Some(control::HostInfo {
                        cpu: m.cpu,
                        mem_used: m.mem_used,
                        mem_total: m.mem_total,
                        gpu: m.gpu,
                        load1: m.load1,
                    }),
                    ..Resp::ok()
                }
            }
            Req::Health => {
                let fd = crate::fdlimit::snapshot().ok();
                Resp::health(control::HealthInfo {
                    panes: self.panes.len(),
                    labeled: self.labels.len(),
                    label_sweeps_failed: self.label_sweeps_failed,
                    fd_soft: fd.map(|b| b.soft),
                    fd_open: fd.and_then(|b| b.open),
                    caps: control::SERVER_CAPS.iter().map(|c| c.to_string()).collect(),
                })
            }
            Req::List { caller } => {
                let (ws, tab) = match self.index_tab(caller.as_deref()) {
                    Ok(v) => v,
                    Err((c, m)) => return Resp::err_code(c, m),
                };
                let Some(t) = self.state.workspace(&ws).and_then(|w| w.tab(&tab)) else {
                    return Resp::err_code("not_found", "the tab is gone");
                };
                let on_screen = ws == self.ws
                    && self
                        .state
                        .workspace(&ws)
                        .is_some_and(|w| w.active_tab == tab);
                // On screen: the rect each pane is drawn in. Elsewhere: the size its terminal
                // was last given — what its program sees — rather than a fresh derivation that
                // a background tab's PTYs have not been resized to.
                let rects = if on_screen { self.layout() } else { Vec::new() };
                let order = t.layout.panes();
                let panes = order
                    .iter()
                    .enumerate()
                    .map(|(index, p)| {
                        let term = t.layout.terminal_of(p).cloned();
                        let (cols, rows) = term
                            .as_ref()
                            .and_then(|tid| {
                                if on_screen {
                                    rects
                                        .iter()
                                        .find(|r| &r.terminal == tid)
                                        .map(|r| (r.cols, r.rows))
                                } else {
                                    self.state.terminal(tid).map(|g| (g.cols, g.rows))
                                }
                            })
                            .unwrap_or((0, 0));
                        let label = term.as_ref().and_then(|tid| self.labels.get(tid));
                        let kind = match label.map(|l| l.kind) {
                            Some(procinfo::Kind::Agent) => "agent",
                            Some(procinfo::Kind::Shell) => "shell",
                            _ => "other",
                        };
                        let status = match (kind, term.as_ref()) {
                            ("agent", Some(tid)) => self.agent_status(tid).0.to_string(),
                            _ => String::new(),
                        };
                        let token = term
                            .as_ref()
                            .and_then(|tid| self.panes.get(tid))
                            .and_then(|pt| pt.pane_token())
                            .unwrap_or_default()
                            .to_string();
                        let (title, bell) = term
                            .as_ref()
                            .map(|tid| {
                                (
                                    self.panes
                                        .get(tid)
                                        .and_then(|pt| pt.title())
                                        .unwrap_or_default(),
                                    self.bell(tid),
                                )
                            })
                            .unwrap_or_default();
                        PaneInfo {
                            index,
                            id: p.to_string(),
                            token,
                            focused: *p == t.focused,
                            cols,
                            rows,
                            label: label
                                .map(|l| l.text.clone())
                                .unwrap_or_else(|| format!("pane {index}")),
                            kind: kind.to_string(),
                            status,
                            title,
                            bell,
                        }
                    })
                    .collect();
                let fi = order.iter().position(|p| *p == t.focused).unwrap_or(0);
                Resp::list(panes, fi)
            }
            Req::Split {
                dir,
                from,
                no_focus,
            } => {
                let d = match dir.as_str() {
                    "down" => Dir::Down,
                    "right" => Dir::Right,
                    other => {
                        return Resp::err_code(
                            "invalid",
                            format!("bad dir '{other}' (right|down)"),
                        );
                    }
                };
                // `from` pins the split SOURCE by identity, in any tab of any session. Without
                // it the focused pane is split, and focus is mutable: a script that lists, then
                // splits, can have the pane created somewhere else entirely — and since a new
                // pane inherits the source's cwd, the command it then runs executes in the
                // wrong directory.
                let (ws, pane) = match from {
                    Some(t) => match self
                        .resolve_pane_token(t)
                        .and_then(|tid| self.locate_terminal(&tid))
                    {
                        Some((ws, _, pane)) => (ws, pane),
                        None => return Resp::err_code("not_found", format!("unknown pane '{t}'")),
                    },
                    None => match self.focused_pane() {
                        Some(p) => (self.ws.clone(), p),
                        None => return Resp::err_code("not_found", "no focused pane"),
                    },
                };
                self.clear_spawn_error();
                // Name the pane we created, so no caller ever has to infer it by diffing
                // listings (which a concurrent focus or tab change can defeat).
                match self.split_at(&ws, pane, d, !no_focus) {
                    Some(term) => self.created(&term),
                    None => Resp::err_code("spawn_failed", self.take_spawn_error("split")),
                }
            }
            Req::ResizePane { index, dir, caller } => {
                let (axis, grow) = match dir.as_str() {
                    "right" => (Dir::Right, true),
                    "left" => (Dir::Right, false),
                    "down" => (Dir::Down, true),
                    "up" => (Dir::Down, false),
                    other => {
                        return Resp::err_code(
                            "invalid",
                            format!("bad dir '{other}' (left|right|up|down)"),
                        );
                    }
                };
                match self.pane_at(caller.as_deref(), *index) {
                    Ok((ws, pane, _)) => {
                        if let Ok(events) = self.state.apply(Command::ResizePane {
                            origin: self.origin_for(&ws),
                            workspace: ws,
                            pane,
                            axis,
                            grow,
                        }) {
                            self.apply_resized_events(&events);
                        }
                        self.sync_sizes();
                        Resp::ok()
                    }
                    Err((c, m)) => Resp::err_code(c, m),
                }
            }
            // In a background tab this focuses the pane within that tab; the view never moves.
            Req::Focus { index, caller } => match self.pane_at(caller.as_deref(), *index) {
                Ok((ws, pane, _)) => {
                    let _ = self.state.apply(Command::FocusPaneIn {
                        origin: self.origin_for(&ws),
                        workspace: ws,
                        pane,
                    });
                    Resp::ok()
                }
                Err((c, m)) => Resp::err_code(c, m),
            },
            Req::Close { index, caller } => match self.pane_at(caller.as_deref(), *index) {
                Ok((ws, pane, _)) => match self.close_pane(&ws, pane) {
                    Ok(()) => Resp::ok(),
                    Err(e) => Resp::err(e.to_string()),
                },
                Err((c, m)) => Resp::err_code(c, m),
            },
            Req::SendKeys {
                target,
                index,
                text,
                paste,
                enter,
                caller,
            } => {
                let tid = match (target, index) {
                    (Some(_), Some(_)) => {
                        return Resp::err_code(
                            "invalid",
                            "give a pane token or an index, not both",
                        );
                    }
                    // A write never defaults to the focused pane: focus is mutable, and
                    // typing into the wrong pane cannot be undone.
                    (None, None) => return Resp::err_code("invalid", "no pane given"),
                    (Some(t), None) => match self.resolve_pane_token(t) {
                        Some(tid) => tid,
                        None => return Resp::err_code("not_found", format!("unknown pane '{t}'")),
                    },
                    (None, Some(i)) => match self.pane_at(caller.as_deref(), *i) {
                        Ok((_, _, Some(tid))) => tid,
                        Ok((_, _, None)) => return Resp::err("pane has no live terminal"),
                        Err((c, m)) => return Resp::err_code(c, m),
                    },
                };
                match self.panes.get(&tid) {
                    Some(pt) => {
                        let mut bytes = if *paste {
                            crate::term::paste_bytes(text, pt.bracketed_paste())
                        } else {
                            text.as_bytes().to_vec()
                        };
                        if *enter {
                            bytes.push(b'\r');
                        }
                        pt.input(&bytes);
                        Resp::ok()
                    }
                    None => Resp::err("pane has no live terminal"),
                }
            }
            Req::ListTabs { session } => {
                let ws = match session {
                    Some(sel) => match self.session_addr(None, Some(sel)) {
                        Ok(ws) => ws,
                        Err((c, m)) => return Resp::err_code(c, &m),
                    },
                    None => self.ws.clone(),
                };
                let Some(w) = self.state.workspace(&ws) else {
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
            Req::NewTab {
                session,
                from,
                cwd,
                name,
                no_focus,
                command,
                env,
                keep,
                owner,
            } => {
                let owner = match owner {
                    Some(t) => match self.resolve_pane_token(t) {
                        Some(tid) => Some(tid),
                        None => {
                            return Resp::err_code(
                                "not_found",
                                format!("unknown owner pane '{t}'"),
                            );
                        }
                    },
                    None => None,
                };
                // Validate a spawn before anything is created: a refusal must leave no tab.
                let mut extra_env = Vec::with_capacity(env.len());
                for kv in env {
                    match control::env_pair(kv) {
                        Ok((k, v)) => extra_env.push((k.to_string(), v.to_string())),
                        Err(e) => return Resp::err_code("invalid", e),
                    }
                }
                if let Some(e) = command.iter().find_map(|a| control::spawn_arg(a).err()) {
                    return Resp::err_code("invalid", e);
                }
                let argv = (!command.is_empty())
                    .then(|| spawn_argv(std::env::var("SHELL").ok().as_deref(), command, *keep));
                let caller = from
                    .as_deref()
                    .and_then(|t| self.resolve_pane_token(t))
                    .and_then(|tid| self.locate_terminal(&tid).map(|(ws, ..)| (ws, tid)));
                // Placement. Naming a session, or asking for no focus, means nothing anyone is
                // looking at moves (decision #134): the tab is created behind that session's
                // active tab, and the user's view stays where it is.
                let ws = match (session, no_focus, &caller) {
                    (Some(sel), ..) => match self.session_addr(None, Some(sel)) {
                        Ok(ws) => ws,
                        Err((c, m)) => return Resp::err_code(c, &m),
                    },
                    (None, true, Some((ws, _))) => ws.clone(),
                    _ => self.ws.clone(),
                };
                let activate = session.is_none() && !no_focus;
                let cwd = match cwd {
                    Some(c) if !Path::new(c).is_dir() => {
                        return Resp::err_code("invalid", format!("not a directory: {c}"));
                    }
                    Some(c) => Some(PathBuf::from(c)),
                    None => match &caller {
                        Some((_, tid)) => self.terminal_cwd(tid),
                        None => self.focused_cwd(),
                    },
                };
                self.clear_spawn_error();
                let launch = TabLaunch {
                    cwd,
                    activate,
                    argv,
                    env: extra_env,
                };
                let Some(term) = self.new_tab_in(&ws, launch) else {
                    return Resp::err_code("spawn_failed", self.take_spawn_error("new-tab"));
                };
                if let Some((ws, tab, _)) = self.locate_terminal(&term) {
                    if let Some(n) = name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                        self.rename_tab(&ws, &tab, Some(n.to_string()));
                    }
                    if let Some(o) = owner {
                        self.prune_mailboxes();
                        self.tab_owner.insert(tab, o);
                    }
                }
                self.created(&term)
            }
            Req::SelectTab { index, target } => {
                let (ws, tab) = match self.tab_addr(*index, target.as_deref()) {
                    Ok(a) => a,
                    Err((c, m)) => return Resp::err_code(c, &m),
                };
                // A tab in another session is reached the way a jump reaches a pane: switch
                // to its session, then select it.
                self.switch_session(ws.clone());
                self.select_tab(ws, tab);
                self.sync_sizes();
                Resp::ok()
            }
            Req::CloseTab { index, target } => {
                let (ws, id) = match self.tab_addr(*index, target.as_deref()) {
                    Ok(a) => a,
                    Err((c, m)) => return Resp::err_code(c, &m),
                };
                // Read the tab's identity + pane count BEFORE closing so the outcome
                // message can name what went away (the tab is gone afterwards).
                let Some((pos, tab)) = self
                    .state
                    .workspace(&ws)
                    .and_then(|w| w.tabs.iter().enumerate().find(|(_, t)| t.id == id))
                else {
                    return Resp::err_code("not_found", format!("no tab '{id}'"));
                };
                let label = tab.name.clone().unwrap_or_else(|| id.to_string());
                let panes = tab.layout.panes().len();
                match self.close_tab(&ws, id) {
                    Ok(()) => Resp::message(format!(
                        "closed tab {pos} '{label}' ({panes} pane(s) reaped)"
                    )),
                    Err(MuxError::CannotCloseLastTab) => Resp::err_code(
                        "refused",
                        "cannot close the last tab of a session — use `comux kill-session` instead",
                    ),
                    Err(e) => Resp::err(e.to_string()),
                }
            }
            Req::RenameTab {
                index,
                pane,
                target,
                name,
            } => {
                // An index addresses the active session's tabs. With no index, `pane` (the
                // caller's own pane) names the tab — in whatever session it lives, so an agent
                // renaming its own tab never touches the one the user is looking at. Neither
                // (an older client) keeps the legacy "active tab" meaning.
                let target = match (index, target, pane) {
                    (_, Some(sel), _) => match self.tab_addr(*index, Some(sel)) {
                        Ok(a) => Some(a),
                        Err((c, m)) => return Resp::err_code(c, &m),
                    },
                    (Some(i), None, _) => self
                        .tab_ids()
                        .get(*i)
                        .cloned()
                        .map(|t| (self.ws.clone(), t)),
                    (None, None, Some(p)) => match self.caller_location(p) {
                        Ok((ws, tab, _)) => Some((ws, tab)),
                        Err(e) => return Resp::err_code("not_found", e),
                    },
                    (None, None, None) => self.active_tab_id().map(|t| (self.ws.clone(), t)),
                };
                match target {
                    Some((ws, tab)) => {
                        let new = (!name.trim().is_empty()).then(|| name.trim().to_string());
                        self.rename_tab(&ws, &tab, new);
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
                self.clear_spawn_error();
                self.new_session(name.clone(), dir);
                if self.session_count() > before {
                    // A successful `new_session` switched to it, so its one pane is the
                    // focused one.
                    match self.focused_terminal() {
                        Some(term) => self.created(&term),
                        None => Resp::ok(),
                    }
                } else {
                    Resp::err_code("spawn_failed", self.take_spawn_error("new-session"))
                }
            }
            Req::RenameSession {
                index,
                pane,
                target,
                name,
            } => {
                // Same addressing as `rename-tab`: index, else the caller's session, else
                // (an older client) the active one.
                let wid = match (index, target, pane) {
                    (_, Some(sel), _) => match self.session_addr(*index, Some(sel)) {
                        Ok(ws) => Some(ws),
                        Err((c, m)) => return Resp::err_code(c, &m),
                    },
                    (Some(i), None, _) => self.session_ids().get(*i).cloned(),
                    (None, None, Some(p)) => match self.caller_location(p) {
                        Ok((ws, _, _)) => Some(ws),
                        Err(e) => return Resp::err_code("not_found", e),
                    },
                    (None, None, None) => Some(self.ws.clone()),
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
            Req::SelectSession { index, target } => {
                match self.session_addr(*index, target.as_deref()) {
                    Ok(ws) => {
                        self.switch_session(ws);
                        Resp::ok()
                    }
                    Err((c, m)) => Resp::err_code(c, &m),
                }
            }
            Req::Nav { dir } => match self.navigate(*dir) {
                Some(landed) => {
                    let mut r = Resp::ok();
                    r.message = Some(self.loc_label(&landed));
                    r
                }
                None => Resp::err_code("not_found", "no navigation history in that direction"),
            },
            Req::Jump { target } => match self.resolve_pane_target(target) {
                Some(term) => {
                    self.jump_to_terminal(&term);
                    Resp::jump(self.raise_target(), self.copad_target())
                }
                None => Resp::err_code("not_found", format!("unknown pane: {target}")),
            },
            Req::Notify { target, kind, body } => {
                let kind = match kind.as_str() {
                    "done" => "done",
                    "blocked" => "blocked",
                    other => {
                        return Resp::err_code(
                            "invalid",
                            format!("unknown kind: {other} (done|blocked)"),
                        );
                    }
                };
                let Some(term) = self.resolve_pane_target(target) else {
                    return Resp::err_code("not_found", format!("unknown pane: {target}"));
                };
                // Claim FIRST: it re-sweeps the process table, so the label read below
                // names the agent that is actually running rather than a stale one.
                self.claim_hook_kind(&term, kind);
                let tool = self
                    .labels
                    .get(&term)
                    .map(|l| l.text.clone())
                    .unwrap_or_else(|| "agent".to_string());
                let body = if body.trim().is_empty() {
                    match kind {
                        "blocked" => "awaiting input".to_string(),
                        _ => "turn finished".to_string(),
                    }
                } else {
                    body.clone()
                };
                // Echo the attribution: a pushing hook otherwise has no way to tell a
                // mis-addressed pane from a correctly addressed one, since the toast is
                // the only other evidence and it is transient.
                Resp::message(self.raise_notification(term, tool, kind, body))
            }
            Req::KillSession { index, target } => {
                let wid = match self.session_addr(*index, target.as_deref()) {
                    Ok(ws) => ws,
                    Err((c, m)) => return Resp::err_code(c, &m),
                };
                let index = self
                    .session_ids()
                    .iter()
                    .position(|w| w == &wid)
                    .unwrap_or_default();
                // Same floor the TUI's `Ctrl-b X` enforces — report it rather than
                // no-op silently, which is what a script would otherwise see.
                if self.state.workspace_count() <= 1 {
                    return Resp::err_code(
                        "refused",
                        "cannot kill the last session (the mux always keeps one)",
                    );
                }
                let (label, tabs, panes) = self
                    .state
                    .workspace(&wid)
                    .map(|w| {
                        (
                            w.name.clone().unwrap_or_else(|| wid.to_string()),
                            w.tabs.len(),
                            w.tabs.iter().map(|t| t.layout.panes().len()).sum::<usize>(),
                        )
                    })
                    .unwrap_or_else(|| (wid.to_string(), 0, 0));
                if self.kill_session(&wid) {
                    Resp::message(format!(
                        "killed session {index} '{label}' ({tabs} tab(s), {panes} pane(s) reaped)"
                    ))
                } else {
                    Resp::err(format!("could not kill session {index}"))
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
            Req::ReloadConfig => {
                // Re-read mux.toml and swap the whole config (tmux `source-file`). Every
                // live-read field takes effect on the next frame because rendering/input
                // read straight off `self.cfg`: keymap, mouse, sidebar_width,
                // sidebar_density, all usage_*, tab_labels, notify, scroll_step, sort_by,
                // worktree — and ALSO
                // restore_processes/restore_agent_sessions, which are read at SAVE time
                // (see `snapshot_*` at ~3300), so a reloaded restore list governs the next
                // autosave/shutdown-save. The genuinely frozen fields are the ones whose
                // runtime copy lives OUTSIDE `self.cfg`: update_environment (a destructive
                // one-time daemon env-scrub + per-connection frozen env) and the
                // persistence toggle/cadence (persist/autosave_secs captured into run()
                // loop vars) — hence the restart hint in the message. The live
                // sidebar-visibility toggle (`self.sidebar`) is intentionally left alone so
                // a reload never yanks a sidebar the user toggled with Ctrl-b s.
                let (cfg, warnings) = crate::config::MuxConfig::load();
                self.cfg = cfg;
                Resp::message(reload_note(
                    &crate::config::MuxConfig::config_path(),
                    &warnings,
                ))
            }
            // Intercepted by the server before it reaches here; answered ok for a
            // (hypothetical) direct call so the match stays exhaustive.
            Req::CapturePane {
                target,
                index,
                lines,
                caller,
            } => {
                if target.is_some() && index.is_some() {
                    return Resp::err_code("invalid", "give a target or --index, not both");
                }
                if *lines == Some(0) {
                    return Resp::err_code("invalid", "--lines must be at least 1");
                }
                // Precedence: explicit target (anywhere in the mux) > index (active tab) >
                // focused pane. Resolved to a TerminalId either way, because that is what
                // owns the grid.
                let tid = match target {
                    Some(t) => match self.resolve_pane_target(t) {
                        Some(tid) => tid,
                        None => return Resp::err_code("not_found", format!("unknown pane '{t}'")),
                    },
                    // An index counts in the caller's tab when there is a caller.
                    None => match index {
                        Some(i) => match self.pane_at(caller.as_deref(), *i) {
                            Ok((_, _, Some(tid))) => tid,
                            Ok((_, _, None)) => return Resp::err("pane has no live terminal"),
                            Err((c, m)) => return Resp::err_code(c, m),
                        },
                        None => match self.focused_terminal() {
                            Some(tid) => tid,
                            None => return Resp::err("no focused pane"),
                        },
                    },
                };
                match self.panes.get(&tid) {
                    Some(pt) => {
                        let token = pt.pane_token().unwrap_or_default().to_string();
                        Resp::capture(pt.capture(*lines), token)
                    }
                    None => Resp::err("pane has no live terminal"),
                }
            }
            Req::ListAgents { target } => {
                // Resolve the target on EVERY request, not once: the pane can close during
                // a wait, and "gone" must be distinguishable from "not an agent yet".
                let want = match target {
                    Some(t) => match self.resolve_pane_target(t) {
                        Some(tid) => Some(tid),
                        // The pane does not exist. Distinct from an existing pane that the
                        // cached classification has not caught up with — which answers with
                        // an empty listing so a caller can keep waiting through the sweep
                        // interval (500ms attached, 5s detached).
                        None => return Resp::err_code("not_found", format!("unknown pane '{t}'")),
                    },
                    None => None,
                };
                let agents = self
                    .agent_rows()
                    .into_iter()
                    .filter(|r| want.as_ref().is_none_or(|w| &r.term == w))
                    .map(|r| {
                        let conv = self
                            .labels
                            .get(&r.term)
                            .and_then(|l| agentpoll::conversation(&self.agent_poll, l.pid));
                        (self.tab_location(&r.term), conv, r)
                    })
                    .map(|(loc, conv, r)| control::AgentInfo {
                        token: self
                            .panes
                            .get(&r.term)
                            .and_then(|p| p.pane_token())
                            .unwrap_or_default()
                            .to_string(),
                        terminal: r.term.to_string(),
                        space: r.space,
                        space_id: r.space_id.to_string(),
                        title: r.title,
                        tool: r.tool,
                        status: r.status.to_string(),
                        for_secs: r.for_secs,
                        seq: self.agent_statuses.get(&r.term).map_or(0, |s| s.seq),
                        detail: r.detail,
                        tab_id: loc.as_ref().map(|l| l.0.to_string()).unwrap_or_default(),
                        tab_name: loc.as_ref().map(|l| l.1.clone()).unwrap_or_default(),
                        tab_index: loc.as_ref().map_or(0, |l| l.2),
                        pane_index: loc.as_ref().map_or(0, |l| l.3),
                        agent_session_id: conv.as_ref().map(|c| c.0.clone()),
                        transcript_path: conv.as_ref().map(|c| c.1.display().to_string()),
                    })
                    .collect();
                Resp::agents(agents)
            }
            Req::ListTree { session, owner } => {
                let only = match session {
                    Some(sel) => match self.session_addr(None, Some(sel)) {
                        Ok(ws) => Some(ws),
                        Err((c, m)) => return Resp::err_code(c, &m),
                    },
                    None => None,
                };
                let owner = match owner {
                    Some(t) => match self.resolve_pane_token(t) {
                        Some(tid) => Some(tid),
                        None => return Resp::err_code("not_found", format!("unknown pane '{t}'")),
                    },
                    None => None,
                };
                Resp {
                    tree: Some(self.tree(only.as_ref(), owner.as_ref())),
                    ..Resp::ok()
                }
            }
            Req::CloseOwned {
                owner,
                caller,
                dry_run,
            } => {
                let Some(owner) = self.resolve_pane_token(owner) else {
                    return Resp::err_code("not_found", format!("unknown pane '{owner}'"));
                };
                let targets: Vec<(WorkspaceId, TabId, control::NodeRef)> = self
                    .live_owners()
                    .into_iter()
                    .filter(|(_, o)| *o == owner)
                    .filter_map(|(tab, _)| {
                        let ws = self.state.workspace_ids().into_iter().find(|w| {
                            self.state
                                .workspace(w)
                                .is_some_and(|w| w.tab(&tab).is_some())
                        })?;
                        let name = self
                            .state
                            .workspace(&ws)
                            .and_then(|w| w.tab(&tab))
                            .and_then(|t| t.name.clone())
                            .unwrap_or_default();
                        let node = control::NodeRef {
                            id: tab.to_string(),
                            name,
                        };
                        Some((ws, tab, node))
                    })
                    .collect();
                // A cleanup must never close the pane running it: it would kill its own CLI
                // before the answer arrives.
                if let Some(c) = caller.as_deref()
                    && let Ok((_, tab, _)) = self.caller_location(c)
                    && targets.iter().any(|(_, t, _)| *t == tab)
                {
                    return Resp::err_code(
                        "refused",
                        format!("tab {tab} is owned by that pane but holds the calling pane"),
                    );
                }
                let mut closed = Vec::new();
                let mut skipped = Vec::new();
                // A dry run plays the same rule the close enforces (a session keeps its last
                // tab), so the preview names exactly what `--yes` would do.
                let mut left: HashMap<WorkspaceId, usize> = HashMap::new();
                for (ws, tab, node) in targets {
                    if *dry_run {
                        let n = left.entry(ws.clone()).or_insert_with(|| {
                            self.state.workspace(&ws).map_or(0, |w| w.tabs.len())
                        });
                        if *n > 1 {
                            *n -= 1;
                            closed.push(node);
                        } else {
                            skipped.push(node);
                        }
                        continue;
                    }
                    match self.close_tab(&ws, tab) {
                        Ok(()) => closed.push(node),
                        Err(_) => skipped.push(node),
                    }
                }
                self.prune_mailboxes();
                let verb = if *dry_run { "would close" } else { "closed" };
                Resp {
                    message: Some(format!(
                        "{verb} {} tab(s){}",
                        closed.len(),
                        if skipped.is_empty() {
                            String::new()
                        } else {
                            format!(", kept {} (a session's last tab)", skipped.len())
                        }
                    )),
                    closed: Some(closed),
                    skipped: Some(skipped),
                    ..Resp::ok()
                }
            }
            Req::Deliver {
                to,
                from,
                kind,
                body,
                result,
            } => {
                if !matches!(kind.as_str(), "done" | "blocked" | "failed") {
                    return Resp::err_code(
                        "invalid",
                        format!("unknown kind: {kind} (done|blocked|failed)"),
                    );
                }
                if body.len() > INBOX_BODY_MAX {
                    return Resp::err_code(
                        "invalid",
                        format!(
                            "the message is over {INBOX_BODY_MAX} bytes — pass a --result file"
                        ),
                    );
                }
                if let Some(r) = result
                    && (r.len() > INBOX_RESULT_MAX || !Path::new(r).is_absolute())
                {
                    return Resp::err_code("invalid", "--result must be an absolute path");
                }
                let sender = match from {
                    Some(t) => match self.resolve_pane_token(t) {
                        Some(tid) => Some((t.clone(), tid)),
                        None => return Resp::err_code("not_found", format!("unknown pane '{t}'")),
                    },
                    None => None,
                };
                let dest = if to == "owner" {
                    let Some((_, tid)) = &sender else {
                        return Resp::err_code("invalid", "--to owner needs the sending pane");
                    };
                    let owner = self
                        .locate_terminal(tid)
                        .and_then(|(_, tab, _)| self.tab_owner.get(&tab).cloned())
                        .filter(|o| self.panes.contains_key(o));
                    match owner {
                        Some(o) => o,
                        None => {
                            return Resp::err_code(
                                "not_found",
                                "your tab has no owner (or its owner pane is gone)",
                            );
                        }
                    }
                } else {
                    match self.resolve_pane_token(to) {
                        Some(tid) => tid,
                        None => return Resp::err_code("not_found", format!("unknown pane '{to}'")),
                    }
                };
                self.prune_mailboxes();
                let box_ = self.inboxes.entry(dest.clone()).or_default();
                box_.next_seq += 1;
                if box_.msgs.len() >= INBOX_CAP {
                    box_.msgs.pop_front();
                    box_.dropped += 1;
                }
                box_.msgs.push_back(control::InboxMsg {
                    seq: box_.next_seq,
                    ts: now_secs(),
                    from: sender.map(|(t, _)| t),
                    kind: kind.clone(),
                    body: body.clone(),
                    result: result.clone(),
                });
                let token = self.token_of(&dest).unwrap_or_default();
                Resp {
                    message: Some(format!("delivered to {token}")),
                    pane: Some(token),
                    ..Resp::ok()
                }
            }
            Req::Inbox { pane, ack } => {
                let Some(tid) = self.resolve_pane_token(pane) else {
                    return Resp::err_code("not_found", format!("unknown pane '{pane}'"));
                };
                let (messages, dropped, more) = match self.inboxes.get_mut(&tid) {
                    Some(b) => {
                        if let Some(a) = ack {
                            while b.msgs.front().is_some_and(|m| m.seq <= *a) {
                                b.msgs.pop_front();
                            }
                            b.dropped = 0;
                        }
                        // One batch stays well under the client's response cap however the
                        // bodies escape; the rest come on the next read, after this one's ack.
                        let mut size = 0;
                        let batch: Vec<control::InboxMsg> = b
                            .msgs
                            .iter()
                            .take_while(|m| {
                                size += serde_json::to_string(m).map_or(0, |j| j.len());
                                size <= INBOX_BATCH_BYTES
                            })
                            .cloned()
                            .collect();
                        let batch = if batch.is_empty() {
                            b.msgs.iter().take(1).cloned().collect()
                        } else {
                            batch
                        };
                        let more = batch.len() < b.msgs.len();
                        (batch, b.dropped, more)
                    }
                    None => (Vec::new(), 0, false),
                };
                Resp {
                    pane: Some(pane.clone()),
                    messages: Some(messages),
                    dropped: Some(dropped),
                    truncated: more.then_some(true),
                    ..Resp::ok()
                }
            }
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

    /// Record whether any attached client has the prefix armed, for the status-bar
    /// mode indicator. Called by the server each frame from the clients' own per-client
    /// prefix flags — the render can't reach those, and the frame is shared anyway.
    /// Has this pane rung the bell since it was last acknowledged?
    pub fn bell(&self, tid: &TerminalId) -> bool {
        self.panes
            .get(tid)
            .is_some_and(|p| p.bell_count() > self.bell_seen.get(tid).copied().unwrap_or(0))
    }

    /// Acknowledge the focused pane's bells — the user just acted on this view.
    ///
    /// The evidence is a CLIENT ACTION (a key or a mouse event), not a frame we hope arrived.
    /// That distinction is the whole design: earlier drafts tried to acknowledge when a frame
    /// was composed, then when it was queued, then when a writer thread had flushed it, and
    /// each boundary had a window where a bell could be cleared without anyone seeing it —
    /// because "the server emitted a frame" is simply not evidence that a human looked. A
    /// keypress is. It also gives the behaviour people expect: a bell clears when you go to
    /// the pane, and one rung while you were away is still there when you come back.
    ///
    /// A pane that rings while you are typing in it (ambiguous tab-completion, which is most
    /// bells) clears on your next keystroke, which is immediate in practice.
    pub fn ack_focused_bell(&mut self) {
        let Some(tid) = self.focused_pane().and_then(|p| {
            self.state
                .workspace(&self.ws)
                .and_then(|w| w.tab(&w.active_tab))
                .and_then(|t| t.layout.terminal_of(&p).cloned())
        }) else {
            return;
        };
        if let Some(count) = self.panes.get(&tid).map(|p| p.bell_count()) {
            self.bell_seen.insert(tid.clone(), count);
        }
        // A notification from the pane being acted on has been seen, for the same reason.
        for n in self.notifications.iter_mut().filter(|n| n.terminal == tid) {
            n.read = true;
        }
        // Pane ids are never reused, so a stale entry is pure growth on a server that lives
        // for weeks. Pruned only when it can actually have grown.
        if self.bell_seen.len() > self.panes.len() {
            self.bell_seen.retain(|tid, _| self.panes.contains_key(tid));
        }
    }

    /// How many panes anywhere in the mux have an unacknowledged bell.
    ///
    /// Rendered in the status bar because the per-tab and per-session markers cannot be a
    /// guarantee: the ringing session's sidebar row can be outside the visible window, and
    /// the sidebar can be hidden entirely on a narrow terminal. Same reasoning as `⚑N`.
    pub fn bell_count(&self) -> usize {
        self.panes.keys().filter(|tid| self.bell(tid)).count()
    }

    pub fn set_prefix_armed(&mut self, armed: bool) {
        self.prefix_armed = armed;
    }

    /// What the last composed frame said about the prefix. The server compares this to a
    /// freshly-pruned client list to notice that a frame it already sent is now a lie.
    pub fn prefix_armed(&self) -> bool {
        self.prefix_armed
    }

    /// The label the status bar shows while the prefix is armed (`^b`), from the
    /// CONFIGURED prefix chord rather than a hardcoded `Ctrl-b`.
    fn prefix_label(&self) -> String {
        self.cfg.keymap.prefix_chord.label()
    }

    /// Route one key event through the mux (popup → prefix → direct-nav → shell).
    /// Returns [`KeyAction::Detach`] on the detach chord. The `Ctrl-b` prefix state is
    /// PER-CLIENT (`prefix`, owned by the caller) so a chord can't span connections
    /// when several clients share input; the popup is shared workspace state.
    pub fn feed_key(&mut self, k: KeyEvent, prefix: &mut bool) -> KeyAction {
        let action = self.feed_key_inner(k, prefix);
        self.track_nav();
        action
    }

    fn feed_key_inner(&mut self, k: KeyEvent, prefix: &mut bool) -> KeyAction {
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

        // The key help scrolls with j/k/arrows/PgUp/PgDn/g/G; any other key closes it.
        if let Some(scroll) = self.help {
            *prefix = false;
            let page = self.rows.saturating_sub(4).max(1) as usize;
            self.help = match k.code {
                KeyCode::Down | KeyCode::Char('j') => Some(scroll + 1),
                KeyCode::Up | KeyCode::Char('k') => Some(scroll.saturating_sub(1)),
                KeyCode::PageDown | KeyCode::Char(' ') => Some(scroll + page),
                KeyCode::PageUp => Some(scroll.saturating_sub(page)),
                KeyCode::Char('g') | KeyCode::Home => Some(0),
                KeyCode::Char('G') | KeyCode::End => Some(usize::MAX),
                _ => None,
            };
            if let Some(s) = self.help {
                self.help = Some(s.min(self.help_max_scroll()));
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

        // The resume picker (`Ctrl-b R`) captures all input while open, for the same
        // shared-state reason as the switcher above.
        if self.resume.is_some() {
            *prefix = false;
            match k.code {
                KeyCode::Down => self.resume_move(1),
                KeyCode::Up => self.resume_move(-1),
                KeyCode::Char('n') if ctrl => self.resume_move(1),
                KeyCode::Char('p') if ctrl => self.resume_move(-1),
                KeyCode::Enter => self.resume_select(),
                // Ctrl-a, not `a`: every printable char belongs to the filter.
                KeyCode::Char('a') if ctrl => self.resume_toggle_all(),
                KeyCode::Char('r') if ctrl => self.resume_rescan(),
                KeyCode::Esc => self.resume = None,
                KeyCode::Backspace => self.resume_edit(None),
                KeyCode::Char(c) if !ctrl => self.resume_edit(Some(c)),
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
            NewTab => {
                self.new_tab();
            }
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
            JumpUnread => self.jump_unread(),
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
            ResumePicker => self.open_resume(),
            Redraw => return KeyAction::Redraw,
            FocusSidebar => self.open_sidebar_focus(),
            NavBack => self.navigate_or_say(control::NavDir::Back),
            NavForward => self.navigate_or_say(control::NavDir::Forward),
            NavLast => self.navigate_or_say(control::NavDir::Last),
            KeyHelp => self.help = Some(0),
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
        let copied = self.mouse_at_inner(client, x, y, kind);
        self.track_nav();
        copied
    }

    fn mouse_at_inner(
        &mut self,
        client: ClientId,
        x: u16,
        y: u16,
        kind: MouseKind,
    ) -> Option<String> {
        if !self.cfg.mouse {
            return None; // mouse disabled in config (client also skips capture)
        }
        // The key help is modal: the wheel scrolls it, a click (either button) closes it,
        // and nothing reaches the panes, the sidebar or a pane app underneath.
        if let Some(scroll) = self.help {
            match kind {
                MouseKind::ScrollUp => self.help = Some(scroll.saturating_sub(3)),
                MouseKind::ScrollDown => {
                    self.help = Some((scroll + 3).min(self.help_max_scroll()));
                }
                MouseKind::Click | MouseKind::RightClick => self.help = None,
                _ => {}
            }
            return None;
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
                // A wheel over the sidebar scrolls THAT half rather than falling through
                // to the focused pane's scrollback, which is what it used to do — the
                // pointer was nowhere near that pane. `content_x` is the first column
                // right of the strip's border, so `x < content_x` is "inside the sidebar".
                if self.sidebar_visible()
                    && x < self.content_x()
                    && y >= self.content_y()
                    && y < self.content_y() + self.content_rows()
                {
                    self.scroll_sidebar(y >= self.sidebar_mid.get(), if up { -1 } else { 1 });
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
                    // A divider lives in the 1-cell gap BETWEEN panes, so it is never inside
                    // a pane rect and only reachable here. Grabbing one starts a resize drag
                    // instead of a text selection — the two cannot overlap.
                    // Never take a divider another client is already holding: input is
                    // shared, and `other_dragging` above only guards the text selection, so
                    // without this a second click re-owned and re-targeted a live gesture.
                    let held_by_other =
                        self.resize_drag.as_ref().is_some_and(|d| d.owner != client);
                    if !held_by_other
                        && let Some(d) = self.divider_at(x, y)
                        && let Some(tab) =
                            self.state.workspace(&self.ws).map(|w| w.active_tab.clone())
                    {
                        self.resize_drag = Some(ResizeDrag {
                            ws: self.ws.clone(),
                            tab,
                            path: d.path,
                            dir: d.dir,
                            ends: d.ends,
                            view_gen: self.view_gen,
                            owner: client,
                        });
                    }
                }
                None
            }
            MouseKind::Drag => {
                // A divider drag takes precedence: if this client grabbed one, every Drag is
                // a resize until it releases.
                if self.resize_drag.as_ref().map(|d| d.owner) == Some(client) {
                    self.drag_divider(x, y);
                    return None;
                }
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
                // Release the divider first: a resize drag never produced a selection, so the
                // clause below would return early and leave the drag armed — the next click
                // anywhere would then move the divider to it.
                if self.resize_drag.as_ref().map(|d| d.owner) == Some(client) {
                    self.drag_divider(x, y);
                    self.resize_drag = None;
                    return None;
                }
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

    /// Drop the selection — and any divider drag — belonging to `client` (its owner
    /// detached).
    ///
    /// The drag matters as much as the selection: client ids are reused, so a drag left armed
    /// by a client that vanished mid-gesture would be INHERITED by whoever next takes that id,
    /// and their first click would yank a divider to it.
    pub fn clear_selection_of(&mut self, client: ClientId) {
        if self.selection.as_ref().is_some_and(|s| s.owner == client) {
            self.selection = None;
        }
        if self.resize_drag.as_ref().is_some_and(|d| d.owner == client) {
            self.resize_drag = None;
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
            self.select_tab(wid, tab);
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
            MenuAction::CloseTab(ws, tab) => {
                let _ = self.close_tab(&ws, tab);
            }
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
    /// Scroll one sidebar half by `delta` ITEMS (a session row, or an agent entry/group
    /// header). Picks up from what the last frame actually rendered — the window start and
    /// its ceiling both depend on the measured split, so only the render pass knows them.
    /// A no-op when the half already fits, so a wheel there is not silently swallowed.
    fn scroll_sidebar(&mut self, agents_half: bool, delta: i32) {
        let view = if agents_half {
            self.agents_view.borrow().clone()
        } else {
            self.spaces_view.borrow().clone()
        };
        if view.max_start == 0 {
            return; // everything fits — nothing to scroll
        }
        let next = (view.start as i32 + delta).clamp(0, view.max_start as i32) as usize;
        let pinned = Some(SidebarScroll {
            start: next,
            anchor: view.anchor,
        });
        if agents_half {
            self.agents_scroll = pinned;
        } else {
            self.spaces_scroll = pinned;
        }
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar = !self.sidebar;
        // The content width changes → re-derive the viewport, not just PTY sizes.
        self.reflow();
    }

    /// Open the `Ctrl-f` fuzzy switcher on the Sessions tab with the PREVIOUS session
    /// selected, so `Ctrl-f Enter` toggles between two sessions like Alt-Tab.
    fn open_popup(&mut self) {
        self.popup = Some(PopupState {
            tab: PopupTab::Sessions,
            filter: String::new(),
            sel: self.popup_default_sel(PopupTab::Sessions, ""),
            pinned: None,
        });
        self.popup_pin();
    }

    /// The row a fresh (unfiltered) list starts on. Sessions: the most recently attached
    /// session other than this one, found BY IDENTITY because the rows follow `sort_by` and
    /// a positional guess (row 1) lands on the current or an unrelated session under any
    /// order but `recent`. A filter means the user is searching: the best match, row 0.
    fn popup_default_sel(&self, tab: PopupTab, filter: &str) -> usize {
        if tab != PopupTab::Sessions || !filter.is_empty() {
            return 0;
        }
        let Some(prev) = self.mru_sessions().into_iter().find(|w| *w != self.ws) else {
            return 0;
        };
        self.popup_items(tab, filter)
            .iter()
            .position(|r| matches!(&r.target, PopupTarget::Session(w) if *w == prev))
            .unwrap_or(0)
    }

    /// The switcher rows for `tab` matching `filter`: SESSIONS (name + git branch) or
    /// AGENTS (tool · status · space), fuzzy-filtered (subsequence, case-insensitive).
    /// Left/Right switch the tab (owner's tmux `Ctrl-f` session switch + `prefix g` agents).
    fn popup_items(&self, tab: PopupTab, filter: &str) -> Vec<PopupRow> {
        let mut rows = Vec::new();
        let (op, filter) = split_status_op(filter);
        match tab {
            PopupTab::Sessions => {
                let agents = op.map(|_| self.agent_rows()).unwrap_or_default();
                for wid in self.sorted_session_ids() {
                    if let Some(op) = op
                        && !agents
                            .iter()
                            .any(|a| a.space_id == wid && op.contains(&a.status))
                    {
                        continue;
                    }
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
                let mut agents = self.agent_rows();
                agents.sort_by_key(|a| status_rank(a.status)); // stable
                for row in agents {
                    if op.is_some_and(|op| !op.contains(&row.status)) {
                        continue;
                    }
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
                    // The detail is part of the haystack: "which pane is running webpack"
                    // is the question the switcher is opened to answer, and the tool name
                    // and status alone cannot answer it.
                    if fuzzy_match(
                        filter,
                        &format!(
                            "{} {} {} {} {}",
                            row.space,
                            row.title,
                            row.tool,
                            row.status,
                            row.detail.as_deref().unwrap_or("")
                        ),
                    ) {
                        rows.push(PopupRow {
                            target: PopupTarget::Agent(row.term),
                            glyph,
                            color,
                            text: agent_popup_text(
                                &row.tool,
                                row.status,
                                row.for_secs,
                                row.detail.as_deref(),
                                &place,
                            ),
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
        let items = self.popup_items(tab, &filter);
        let n = items.len();
        let Some(p) = &mut self.popup else {
            return false;
        };
        // Follow the pinned row if it is still listed; else stay in range and re-pin.
        let found = p
            .pinned
            .as_ref()
            .and_then(|t| items.iter().position(|r| r.target == *t));
        let want = found.unwrap_or(if n == 0 { 0 } else { p.sel.min(n - 1) });
        let moved = want != p.sel;
        p.sel = want;
        if found.is_none() {
            p.pinned = items.get(want).map(|r| r.target.clone());
        }
        moved
    }

    /// Record the row now under `sel` as the one the user chose (see `PopupState::pinned`).
    fn popup_pin(&mut self) {
        let Some((tab, filter, sel)) = self
            .popup
            .as_ref()
            .map(|p| (p.tab, p.filter.clone(), p.sel))
        else {
            return;
        };
        let target = self
            .popup_items(tab, &filter)
            .get(sel)
            .map(|r| r.target.clone());
        if let Some(p) = &mut self.popup {
            p.pinned = target;
        }
    }

    /// Switch the switcher tab (Left/Right), resetting the selection to the top.
    fn popup_tab(&mut self, to: PopupTab) {
        if let Some(p) = &mut self.popup {
            p.tab = to;
        }
        if let Some(sel) = self
            .popup
            .as_ref()
            .map(|p| self.popup_default_sel(p.tab, &p.filter))
            && let Some(p) = &mut self.popup
        {
            p.sel = sel;
        }
        self.popup_pin();
    }

    /// Move the switcher selection by `delta`, wrapping over the filtered rows.
    fn popup_move(&mut self, delta: i32) {
        let Some(p) = self.popup.as_ref() else {
            return;
        };
        let items = self.popup_items(p.tab, &p.filter);
        let n = items.len() as i32;
        if n == 0 {
            return;
        }
        let from = pinned_sel(p, &items) as i32;
        if let Some(p) = &mut self.popup {
            p.sel = (from + delta).rem_euclid(n) as usize;
        }
        self.popup_pin();
    }

    /// Append `c` to the switcher filter (resets the selection to the top).
    fn popup_type(&mut self, c: char) {
        if let Some(p) = &mut self.popup {
            p.filter.push(c);
            p.sel = 0;
        }
        self.popup_pin();
    }

    /// Delete the last filter char.
    fn popup_backspace(&mut self) {
        if let Some(p) = &mut self.popup {
            p.filter.pop();
        }
        if let Some(sel) = self
            .popup
            .as_ref()
            .map(|p| self.popup_default_sel(p.tab, &p.filter))
            && let Some(p) = &mut self.popup
        {
            p.sel = sel;
        }
        self.popup_pin();
    }

    /// Act on the selected row (switch session / jump to agent) and close the switcher.
    /// With no matching row (over-narrow filter) Enter is a no-op — the switcher stays
    /// open showing the empty state rather than closing on nothing.
    fn popup_select(&mut self) {
        let Some(p) = self.popup.as_ref() else {
            return;
        };
        let items = self.popup_items(p.tab, &p.filter);
        let Some(row) = items.get(pinned_sel(p, &items)) else {
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
        }) = items.get(pinned_sel(p, &items))
        else {
            return;
        };
        let wid = wid.clone();
        self.popup = None;
        self.switch_session(wid);
        self.open_rename_prompt();
    }

    // --- resume picker (Ctrl-b R) ---

    /// Open the resume picker and ask for a scan. The list renders from the previous scan
    /// (if any) while the new one runs, so reopening never blanks to "scanning…".
    fn open_resume(&mut self) {
        let live = self.live_agent_sessions();
        self.resume = Some(ResumeState {
            filter: String::new(),
            sel: 0,
            all: false,
            live,
        });
        self.request_scan(false, false);
    }

    /// Ask the detached scanner for a fresh transcript listing.
    ///
    /// `deep` also reads titles for headless transcripts (only the `Ctrl-a` view shows
    /// them, and they cost most of the scan). `force` bypasses the freshness reuse — a
    /// completed scan younger than [`SCAN_TTL`] that already covers what we need is reused
    /// so flipping the scope back and forth doesn't re-walk two directory trees.
    fn request_scan(&mut self, deep: bool, force: bool) {
        // Already scanning: adding another thread would just put two 256 MB reads in each
        // other's way (`Ctrl-r` held down, or a scope flip mid-scan). The outstanding one is
        // abandoned only if it looks stuck.
        let outstanding = self.scan_shown != Some(self.scan_req);
        if outstanding && self.scan_at.is_some_and(|t| t.elapsed() < SCAN_STUCK) {
            return;
        }
        let covered = self.scan_deep || !deep;
        let fresh = self.scan_at.is_some_and(|t| t.elapsed() < SCAN_TTL);
        if !force && fresh && covered {
            return;
        }
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        self.scan_req += 1;
        self.scan_deep = deep;
        self.scan_at = Some(std::time::Instant::now());
        agentsessions::spawn_scan(self.sessions.clone(), self.scan_req, home, deep);
    }

    /// Adopt a completed scan that matches the newest request. Returns whether the picker
    /// needs a repaint. Called from the server loop, never from the key path — the scan
    /// itself runs on its own thread and may take a second.
    pub fn poll_agent_sessions(&mut self) -> bool {
        if self.resume.is_none() {
            return false;
        }
        let Some(scan) = self.sessions.lock().ok().and_then(|g| g.as_ref().cloned()) else {
            return false;
        };
        // Not what we asked for (a stale scan), or already on screen.
        if scan.req != self.scan_req || self.scan_shown == Some(scan.req) {
            return false;
        }
        self.sessions_shown = scan.entries;
        self.scan_shown = Some(scan.req);
        self.clamp_resume();
        true
    }

    /// `Ctrl-a`: include non-interactive runs. Their titles are only read by a deep scan,
    /// so entering the "all" view asks for one (the rows appear immediately regardless).
    fn resume_toggle_all(&mut self) {
        let Some(st) = &mut self.resume else { return };
        st.all = !st.all;
        st.sel = 0;
        let deep = st.all;
        if deep {
            self.request_scan(true, false);
        }
    }

    /// `Ctrl-r`: re-read the transcript directories now (a conversation that ended while
    /// the picker was open is otherwise missing until the freshness window expires).
    fn resume_rescan(&mut self) {
        let deep = self.resume.as_ref().is_some_and(|st| st.all);
        let live = self.live_agent_sessions();
        if let Some(st) = &mut self.resume {
            st.live = live;
        }
        self.request_scan(deep, true);
    }

    /// Edit the fuzzy filter: `Some(c)` appends, `None` backspaces. Resets the selection.
    fn resume_edit(&mut self, c: Option<char>) {
        if let Some(st) = &mut self.resume {
            match c {
                Some(c) => st.filter.push(c),
                None => {
                    st.filter.pop();
                }
            }
            st.sel = 0;
        }
    }

    /// Move the selection by `delta`, wrapping over the filtered rows.
    fn resume_move(&mut self, delta: i32) {
        let n = self
            .resume
            .as_ref()
            .map(|st| self.resume_rows(st).len())
            .unwrap_or(0) as i32;
        if n == 0 {
            return;
        }
        if let Some(st) = &mut self.resume {
            st.sel = (st.sel as i32 + delta).rem_euclid(n) as usize;
        }
    }

    /// Keep the selection in range after the row set changes under it (a scan landing).
    fn clamp_resume(&mut self) {
        let n = self
            .resume
            .as_ref()
            .map(|st| self.resume_rows(st).len())
            .unwrap_or(0);
        if let Some(st) = &mut self.resume {
            st.sel = if n == 0 { 0 } else { st.sel.min(n - 1) };
        }
    }

    /// The rows to show: scan entries in recency order, hidden headless runs dropped unless
    /// `all`, fuzzy-filtered over tool + prompt + path together (so `copad codex` narrows by
    /// project AND tool).
    fn resume_rows(&self, st: &ResumeState) -> Vec<ResumeRow> {
        let now = std::time::SystemTime::now();
        // Two tiers: rows whose text CONTAINS the query verbatim, then rows it merely
        // scatters through. A subsequence match over a whole prompt plus a path hits almost
        // everything (typing `copad` matched 26 of 61 rows), so without the split a typed
        // word buries its own hits under coincidences. Recency order holds within each tier.
        let mut exact = Vec::new();
        let mut fuzzy = Vec::new();
        for e in self.sessions_shown.iter().filter(|e| st.all || !e.headless) {
            let path = e.cwd.as_deref().map(home_short).unwrap_or_default();
            let Some(rank) =
                resume_rank(&st.filter, &format!("{} {} {path}", e.tool.bin(), e.title))
            else {
                continue;
            };
            let secs = now
                .duration_since(e.mtime)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let row = ResumeRow {
                tool: e.tool,
                id: e.id.clone(),
                cwd: e.cwd.clone(),
                title: e.title.clone(),
                age: agentsessions::fmt_age(secs),
                live: st.live.get(&e.id).cloned(),
            };
            if rank == 0 {
                exact.push(row)
            } else {
                fuzzy.push(row)
            }
        }
        exact.extend(fuzzy);
        exact
    }

    /// Conversation id → the pane running it, for every agent pane this server knows about.
    /// Sampled when the picker opens (each lookup reads the agent's own state file).
    fn live_agent_sessions(&self) -> HashMap<String, TerminalId> {
        let mut out = HashMap::new();
        for (tid, label) in &self.labels {
            if label.kind != procinfo::Kind::Agent {
                continue;
            }
            if let Some(id) = agentstate::agent_session_id(&label.text, label.pid) {
                out.insert(id, tid.clone());
            }
        }
        out
    }

    /// Act on the selected row: jump to the pane already running that conversation, else
    /// open it in a new tab. An empty selection (over-narrow filter) keeps the picker open.
    fn resume_select(&mut self) {
        let Some(row) = self
            .resume
            .as_ref()
            .and_then(|st| self.resume_rows(st).get(st.sel).cloned())
        else {
            return;
        };
        self.resume = None;
        match row.live.clone() {
            Some(term) => self.jump_to_terminal(&term),
            None => self.open_resumed(row),
        }
    }

    /// Open `row`'s conversation in a new tab: in the space already working in that
    /// directory when there is one, else in the current space.
    ///
    /// The space switch happens BEFORE the tab is created (a client may only mutate the
    /// workspace it is attached to) and is undone if the pane fails to spawn, so a failed
    /// resume leaves the user exactly where they were.
    fn open_resumed(&mut self, row: ResumeRow) {
        let argv = agentsessions::resume_argv(row.tool, &row.id);
        let Some(line) = build_command_line(&argv) else {
            return;
        };
        // A recorded cwd that no longer exists (deleted worktree) would fail the spawn, so
        // fall back to the focused pane's directory rather than refusing to resume at all.
        let gone = row.cwd.as_ref().is_some_and(|c| !c.is_dir());
        let cwd = row
            .cwd
            .clone()
            .filter(|c| c.is_dir())
            .or_else(|| self.focused_cwd());
        let prev = self.ws.clone();
        let target = self.resume_target_space(cwd.as_deref());
        self.switch_session(target.clone());
        let launch = TabLaunch {
            cwd: cwd.clone(),
            activate: true,
            ..TabLaunch::default()
        };
        match self.new_tab_in(&target, launch) {
            Some(term) => {
                if let Some(pt) = self.panes.get(&term) {
                    pt.input(format!("{line}\n").as_bytes());
                }
                if gone {
                    let where_ = cwd
                        .as_ref()
                        .map(|c| c.display().to_string())
                        .unwrap_or_else(|| "the default directory".into());
                    self.report_note(&format!(
                        "resumed elsewhere: the recorded directory is gone — opened in {where_}"
                    ));
                }
            }
            None => self.switch_session(prev),
        }
        self.reflow();
    }

    /// The space to resume into: the one already working in `cwd`. Scored so the CLOSEST
    /// relationship wins — an exact cwd match first, then the deepest space containing
    /// `cwd`, then the shallowest space nested inside it (a repo-root conversation should
    /// find the space sitting in `repo/src`). Ties prefer the current space, so a resume
    /// never moves the user for no reason. No relationship at all → the current space.
    fn resume_target_space(&self, cwd: Option<&std::path::Path>) -> WorkspaceId {
        let Some(cwd) = cwd else {
            return self.ws.clone();
        };
        let target = crate::worktree::canonical_or_lexical(cwd);
        let mut best: Option<((u8, usize), WorkspaceId)> = None;
        for wid in self.session_ids() {
            for pane_cwd in self.session_cwds(&wid) {
                let Some(score) =
                    cwd_affinity(&crate::worktree::canonical_or_lexical(&pane_cwd), &target)
                else {
                    continue;
                };
                let better = match &best {
                    None => true,
                    // On a tie, keep the user where they are rather than moving them.
                    Some((s, w)) => score > *s || (score == *s && wid == self.ws && w != &self.ws),
                };
                if better {
                    best = Some((score, wid.clone()));
                }
            }
        }
        best.map(|(_, w)| w).unwrap_or_else(|| self.ws.clone())
    }

    /// Every pane cwd in `wid` — the live `process_cwd`, falling back to the recorded
    /// spawn cwd when the process read fails (same fallback as the worktree lookup).
    fn session_cwds(&self, wid: &WorkspaceId) -> Vec<PathBuf> {
        let Some(w) = self.state.workspace(wid) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for t in &w.tabs {
            for p in t.layout.panes() {
                if let Some(tid) = t.layout.terminal_of(&p)
                    && let Some(pane) = self.panes.get(tid)
                    && let Some(cwd) = pane
                        .pid()
                        .and_then(procinfo::process_cwd)
                        .or_else(|| pane.spawn_cwd().cloned())
                {
                    out.push(cwd);
                }
            }
        }
        out
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
            self.mark_read(&note.terminal);
            self.jump_to_terminal(&note.terminal);
        }
    }

    /// Mark every notification from `term` read.
    fn mark_read(&mut self, term: &TerminalId) {
        for n in self
            .notifications
            .iter_mut()
            .filter(|n| n.terminal == *term)
        {
            n.read = true;
        }
    }

    /// Unread notifications whose pane still exists.
    fn unread_count(&self) -> usize {
        self.notifications
            .iter()
            .filter(|n| !n.read && self.panes.contains_key(&n.terminal))
            .count()
    }

    /// `Ctrl-b u`: go to the pane of the NEWEST unread notification and mark it read. A
    /// notification whose pane is gone is marked read on the way (there is nothing to see).
    fn jump_unread(&mut self) {
        let target = loop {
            let Some(n) = self.notifications.iter_mut().find(|n| !n.read) else {
                break None;
            };
            n.read = true;
            let term = n.terminal.clone();
            if self.panes.contains_key(&term) {
                break Some(term);
            }
        };
        match target {
            Some(term) => {
                self.mark_read(&term);
                self.jump_to_terminal(&term);
            }
            None => self.flash("no unread notifications".into(), false),
        }
    }

    /// Close panes whose shell has exited (in ANY tab). Returns true if the app is
    /// now empty (last pane of the last tab exited → quit). A tab whose last pane
    /// exits is closed whole; other tabs keep running in the background.
    /// Reap any exited shells, collapsing panes/tabs/sessions as needed. Returns
    /// whether it CHANGED visible state (so the render loop repaints the removed pane);
    /// the caller checks [`is_empty`](Self::is_empty) separately to decide whether to quit.
    pub fn reap_exited(&mut self) -> bool {
        let changed = self.reap_exited_inner();
        if changed {
            self.track_nav();
        }
        changed
    }

    fn reap_exited_inner(&mut self) -> bool {
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
                        if was_active && let Some(next) = self.fallback_session(&wid) {
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
        let content_y = self.content_y();
        let content_rows = self.content_rows();
        for yy in 0..area.height {
            for xx in 0..area.width {
                // The sidebar owns the left strip and the status bar owns the bottom
                // row; don't paint dividers over either.
                // Skip anything outside the pane grid: the top bar's rows, the sidebar's
                // strip, the status bar. `yy < content_y` matters only because the grid no
                // longer starts at row 0 — the top bar repaints over it today, so this is a
                // correctness fix for TOP_H > 1 rather than a visible one.
                if yy < content_y
                    || yy >= content_y + content_rows
                    || xx < content_x
                    || covered[yy as usize][xx as usize]
                {
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

        // 4) the optional top bar (host machine readout), over the reserved top row.
        if self.top_h() > 0 {
            self.render_top_bar(buf);
        }

        // 5) the always-on bottom status bar (session · tabs · scroll/agents/clock/host).
        self.render_status_bar(buf);

        // 6) the Ctrl-f switcher popup / notification center, over everything.
        if self.popup.is_some() {
            self.render_popup(buf);
        }
        if self.center.is_some() {
            self.render_center(buf);
        }
        if self.resume.is_some() {
            self.render_resume(buf);
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
        // On top of every other overlay: it is the one you open to find out how to leave them.
        if self.help.is_some() {
            self.render_help(buf);
        }
        // Last, over everything every widget wrote: no cell may carry DIM and BOLD at once.
        resolve_dim_bold(buf);

        // The shell cursor shows only when nothing modal is capturing input.
        if self.popup.is_none()
            && self.scroll_pane.is_none()
            && self.center.is_none()
            && self.resume.is_none()
            && self.prompt.is_none()
            && self.confirm.is_none()
            && self.menu.is_none()
            && self.help.is_none()
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
    /// Collect any OSC 52 clipboard write a program in ANY pane made since the last call
    /// (tmux `set-clipboard`). The clipboard has one slot, so on the rare tick where two
    /// panes both wrote, only the latest survives — chosen by the write's sequence number,
    /// NOT by `HashMap` traversal order, which is arbitrary.
    ///
    /// An empty payload is kept: OSC 52 with no data means "clear the clipboard", which is
    /// a real operation a pane program can ask for.
    ///
    /// Every pane is drained even when the feature is off, so flipping `osc52` back on via
    /// `comux reload` can't suddenly paste something a background pane wrote hours ago.
    /// Read live off `self.cfg` for that same reason.
    pub fn drain_pane_clipboard(&mut self) -> Option<(u64, String)> {
        let mut latest: Option<(u64, String)> = None;
        for pt in self.panes.values() {
            if let Some((seq, text)) = pt.take_clipboard()
                && latest.as_ref().is_none_or(|(prev, _)| seq > *prev)
            {
                latest = Some((seq, text));
            }
        }
        let (seq, text) = latest?;
        // A write that committed to its pane's slot only AFTER a newer one had already been
        // broadcast must not resurrect — the clipboard would regress to older content a tick
        // later. The high-water mark is advanced even when the feature is off, so re-enabling
        // it can't replay anything either.
        if seq <= self.last_clipboard_seq {
            return None;
        }
        self.last_clipboard_seq = seq;
        self.cfg.osc52.then_some((seq, text))
    }

    /// Clear every pane's dirty flag, and report whether a VISIBLE one had advanced.
    ///
    /// The flag must be taken from every pane — a hidden pane that kept its flag would
    /// report stale output the moment it was revealed — but only a pane in `self.layout()`
    /// is on screen, and only those can justify recomposing the frame. This used to be
    /// "any pane anywhere", which on a mux with dozens of panes meant a single background
    /// agent's spinner drove the visible screen to recompose (new `Buffer`, per-client
    /// diff, clone) at the full frame rate, forever.
    ///
    /// `layout()` is the visibility source of truth — `take_alt_screen_transition` uses
    /// exactly it — and revealing a hidden pane goes through a mutation that dirties the
    /// frame on its own, so nothing is lost on the switch.
    ///
    /// This is only sound because every VISIBLE value that is a live function of a hidden
    /// pane is compared on the label cadence instead: agent status and detail
    /// (`refresh_agent_statuses`), bells (`refresh_bells`), host metrics
    /// (`refresh_host_metrics`), labels, branches, usage and version. Adding a new live
    /// read of `self.panes` to a render path means adding its comparison there too.
    pub fn drain_pane_dirty(&self) -> bool {
        let mut visible: std::collections::HashSet<TerminalId> =
            self.layout().into_iter().map(|r| r.terminal).collect();
        // The switcher's preview shows a pane that may be in a background session.
        visible.extend(self.popup_preview_terminal());
        let mut dirty = false;
        for (tid, pt) in &self.panes {
            // No short-circuit: every flag has to be taken, visible or not.
            if pt.take_dirty() && visible.contains(tid) {
                dirty = true;
            }
        }
        dirty
    }

    /// Poll the VISIBLE panes' alternate-screen bits and report whether any just CROSSED the
    /// primary↔alt boundary in EITHER direction — a full-screen app (nvim, less, htop, …)
    /// entered or exited. The render loop reacts by forcing a full repaint so both the app's
    /// first screen AND the restored primary screen are clean: during the app's life the
    /// client's unicode-width render can desync from alacritty's grid on wide graphemes
    /// (e.g. VS16 emoji), and either grid swap is where that residue shows; a full frame is
    /// also the clean baseline an OUTER terminal (Windows Terminal over SSH, a nested
    /// emulator) needs when it drops cells on such a large wholesale change. This updates the
    /// stored per-pane state.
    ///
    /// Scoped to the active tab's panes (a background pane toggling alt-screen doesn't affect
    /// the composed frame; when its tab is next shown the whole buffer recomposes anyway).
    /// Detection is a per-render poll — alacritty applies `?1049h`/`?1049l` on its own reader
    /// thread with no callback we can hook — so an app that enters AND exits within a single
    /// render interval (e.g. `nvim +q` from a script) can be missed. That isn't the interactive
    /// full-screen case the user hits, and the wholesale content change usually clears it.
    pub fn take_alt_screen_transition(&mut self) -> bool {
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
        detect_alt_screen_transition(&mut self.alt_screen, &visible)
    }

    /// Wall-clock minute-of-day (`hour*60 + minute`), for the render loop to force a
    /// repaint when the status-bar `HH:MM` rolls over while otherwise idle.
    pub fn clock_minute(&self) -> u32 {
        local_minute()
    }

    /// Build the on-disk snapshot of the current session layout (names + tabs + BSP split
    /// trees + per-leaf cwd). Read-only — safe to call on the render loop for autosave.
    pub fn snapshot(&self) -> persist::PersistState {
        // Owner panes get a snapshot-local ref their tabs point at (tokens die with the server).
        let mut refs: HashMap<TerminalId, u32> = HashMap::new();
        for (_, owner) in self.live_owners() {
            let next = refs.len() as u32;
            refs.entry(owner).or_insert(next);
        }
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
                    layout: self.layout_to_persist(&t.layout, &refs),
                    owner: self.tab_owner.get(&t.id).and_then(|o| refs.get(o)).copied(),
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
    fn layout_to_persist(&self, tree: &SplitTree, refs: &HashMap<TerminalId, u32>) -> PLayout {
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
                PLayout::Leaf {
                    cwd,
                    command,
                    pane_ref: refs.get(terminal).copied(),
                }
            }
            SplitTree::Branch {
                dir,
                ratio,
                first,
                second,
            } => PLayout::Branch {
                dir: *dir,
                ratio: *ratio,
                first: Box::new(self.layout_to_persist(first, refs)),
                second: Box::new(self.layout_to_persist(second, refs)),
            },
        }
    }

    /// Returns whether any chrome-affecting data (labels / agent statuses / branches)
    /// actually CHANGED, so the render loop only recomposes when the sidebar/status bar
    /// would differ (unchanged refreshes are free of a repaint).
    ///
    /// `min_interval` is the caller's throttle: this sweep enumerates every process on
    /// the machine, so it — not the frame cadence — is what a long-lived server
    /// actually spends its CPU on. The render loop passes a wider interval while
    /// detached, where the only consumer left is the agent-turn notifier and nobody is
    /// looking at the chrome it feeds.
    /// Force the next [`Self::maybe_refresh_labels`] to actually sweep, whatever its
    /// throttle. Called when a client attaches: while detached the sweep runs on a wide
    /// interval, so the first frame would otherwise render tab labels and agent
    /// statuses that are seconds stale.
    pub fn invalidate_labels(&mut self) {
        self.last_labels = std::time::Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(std::time::Instant::now);
    }

    pub fn maybe_refresh_labels(&mut self, min_interval: Duration) -> bool {
        if self.last_labels.elapsed() >= min_interval {
            let a = self.refresh_labels();
            let b = self.refresh_agent_statuses();
            let c = self.refresh_branches();
            let d = self.refresh_usage();
            let e = self.refresh_version();
            let f = self.refresh_bells();
            let g = self.refresh_host_metrics();
            a || b || c || d || e || f || g
        } else {
            false
        }
    }

    /// Report whether the set of panes with an unacknowledged bell has moved.
    ///
    /// `bell()` and `bell_count()` are live reads off each `PaneTerm`'s atomic counter, so
    /// nothing else on this cadence would notice a bell. It used to repaint anyway —
    /// ringing produces PTY output, and `drain_pane_dirty` treated ANY pane's output as a
    /// reason to recompose. Now that only visible panes do that, a bell rung in a hidden
    /// pane needs its own comparison to reach the `!` markers and the mux-wide `! N`.
    ///
    /// The comparison is the per-pane SET, not the count: one pane ringing while another is
    /// acknowledged in the same window leaves the total unchanged while both markers move.
    fn refresh_bells(&mut self) -> bool {
        let next: std::collections::HashSet<TerminalId> = self
            .panes
            .keys()
            .filter(|tid| self.bell(tid))
            .cloned()
            .collect();
        let changed = next != self.bells_reported;
        self.bells_reported = next;
        changed
    }

    /// Report whether the top bar's host readings have moved.
    ///
    /// `render_top_bar` reads `hostmetrics::read` live off the poller thread, which is
    /// another value that only ever repainted because some pane somewhere dirtied the frame.
    /// Compared as what is actually DRAWN — text AND threshold colour — so a drift below the
    /// printed precision costs nothing while a severity change still repaints.
    fn refresh_host_metrics(&mut self) -> bool {
        // Nothing renders these when the bar is not on screen, so don't manufacture repaints
        // for them. `top_h()` rather than `cfg.top_bar`: the bar is also suppressed on a
        // terminal below `MIN_ROWS_FOR_TOP`, and a config check would still repaint there.
        if self.top_h() == 0 {
            if !self.host_reported.is_empty() {
                self.host_reported.clear();
            }
            return false;
        }
        let next: Vec<(String, String, Color)> = host_parts(&hostmetrics::read(&self.host_poll))
            .into_iter()
            .map(|p| (p.label, p.value, p.color))
            .collect();
        let changed = next != self.host_reported;
        self.host_reported = next;
        changed
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

    /// How often each attached client should be told to silently repaint its whole screen
    /// (`reconcile_secs`), or `None` when the self-heal is switched off.
    ///
    /// Read live off `self.cfg` rather than cached in the server loop, so `comux reload`
    /// changes the cadence on the RUNNING server like every other render-affecting setting.
    pub fn repaint_period(&self) -> Option<Duration> {
        match self.cfg.reconcile_secs {
            0 => None,
            n => Some(Duration::from_secs(n as u64)),
        }
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

    /// Start the background host-metrics poller (server-only; see `hostmetrics`).
    pub fn start_host_poll(&mut self) {
        self.host_poll = hostmetrics::spawn();
    }

    /// Start the background agent-activity poller (server-only; see `agentpoll`).
    /// Unconditional: unlike the usage and update pollers it does no network I/O and stays
    /// entirely idle — no wake work at all — until an agent pane exists to track.
    pub fn start_agent_poll(&mut self) {
        self.agent_poll = agentpoll::spawn();
    }

    /// Start the git working-tree poller (server-only). `sidebar_git = false` never starts
    /// it, so that setting forks no `git` at all.
    pub fn start_git_poll(&mut self) {
        if self.cfg.sidebar_git {
            self.git_poll = gitpoll::spawn();
        }
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

    /// Recompute each session's git subtitle from its focused pane's shell cwd: the branch
    /// (fork-free, `gitinfo`) decorated with the poller's last fresh reading for that repo
    /// (`gitpoll`) — `⑂ feat-x* ↑2 +12 −3`. Rebuilt each pass so a `cd` out of a repo clears
    /// it, and compared as the RENDERED string so a new poll result repaints.
    fn refresh_branches(&mut self) -> bool {
        let mut next = HashMap::new();
        let mut roots = Vec::new();
        for wid in self.state.workspace_ids() {
            let Some(cwd) = self
                .state
                .workspace(&wid)
                .and_then(|w| w.tab(&w.active_tab))
                .and_then(|t| t.layout.terminal_of(&t.focused).cloned())
                .and_then(|tid| self.panes.get(&tid))
                .and_then(|pane| pane.pid())
                .and_then(procinfo::process_cwd)
            else {
                continue;
            };
            let Some(branch) = gitinfo::branch(&cwd) else {
                continue;
            };
            let sub = match gitinfo::repo(&cwd) {
                Some(repo) if self.cfg.sidebar_git => {
                    let reading = gitpoll::reading(&self.git_poll, &repo.root);
                    roots.push(repo.root.clone());
                    gitpoll::subtitle(&branch, repo.linked, reading.as_ref())
                }
                Some(repo) => gitpoll::subtitle(&branch, repo.linked, None),
                None => branch,
            };
            next.insert(wid, sub);
        }
        gitpoll::set_wanted(&self.git_poll, roots);
        let changed = next != self.branches;
        self.branches = next;
        changed
    }

    /// Refresh, then every subtitle joined — what the tests compare.
    #[cfg(test)]
    fn refresh_branches_for_test(&mut self) -> String {
        self.refresh_branches();
        let mut v: Vec<&String> = self.branches.values().collect();
        v.sort();
        v.into_iter().cloned().collect::<Vec<_>>().join("|")
    }

    /// Recompute each agent pane's status (working/ready/blocked/idle) from Claude's
    /// session file + a screen-text fallback (`agentstate`). Agent panes only; the map
    /// is rebuilt each pass so closed/reclassified panes drop out.
    fn refresh_agent_statuses(&mut self) -> bool {
        use agentstate::AgentStatus;
        let mut next = HashMap::new();
        // Meaningful status TRANSITIONS to notify on (fired after the borrow ends).
        let mut events: Vec<(TerminalId, String, &'static str)> = Vec::new();
        // The activity poller is told which agents exist from HERE rather than sweeping the
        // process table itself: this loop already walks exactly the agent panes and holds
        // their labels, so `procinfo` keeps a single writer (#88).
        let mut wanted: Vec<(u32, String)> = Vec::new();
        // What each agent is DOING. Read here rather than only at render time so a change
        // reports `changed` — see `App::agent_details`.
        let mut details: HashMap<TerminalId, String> = HashMap::new();
        let mut seq_counter = self.agent_seq;
        for (tid, pane) in &self.panes {
            let Some(label) = self.labels.get(tid) else {
                continue;
            };
            if label.kind != procinfo::Kind::Agent {
                continue;
            }
            wanted.push((label.pid, label.text.clone()));
            if let Some(a) = agentpoll::activity(&self.agent_poll, label.pid) {
                details.insert(tid.clone(), a.detail);
            }
            let status = agentstate::resolve_with(Some(label.pid), || pane.snapshot());
            let prev = self.agent_statuses.get(tid).copied();
            let body = match (prev.map(|p| p.status), status) {
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
            // Carry the stamp forward while the status HOLDS — rebuilding the map each
            // pass must not reset "how long has this been blocked" twice a second. This
            // is also what keeps `changed` honest: an unchanged pass reproduces the map
            // exactly, so the equality check below still means "a status moved" and the
            // render loop's idle-skip survives.
            let since = status_since(prev, status, std::time::Instant::now());
            // A NEW status — including the first one seen for this pane, so an agent that
            // reappears after losing its classification is a change — takes the next number.
            let seq = status_seq(prev, label.pid, status, &mut seq_counter);
            next.insert(
                tid.clone(),
                AgentState {
                    status,
                    since,
                    seq,
                    pid: label.pid,
                },
            );
        }
        // The sidebar renders time-in-status bucketed to whole minutes, and a BLOCKED
        // agent — the one the attention band exists for — emits nothing, so no pane-dirty
        // signal will ever fire when its bucket rolls. The loop's HH:MM tick does not
        // cover it either: that fires on wall-clock minutes, not on the minute each
        // agent's status began, so a row could sit a full bucket behind its neighbours.
        // Report a roll as a change.
        self.agent_seq = seq_counter;
        agentpoll::set_wanted(&self.agent_poll, wanted);
        let mut minutes = HashMap::with_capacity(next.len());
        let mut rolled = false;
        for (tid, st) in &next {
            let m = st.since.elapsed().as_secs() / 60;
            rolled |= self.agent_minutes.get(tid) != Some(&m);
            minutes.insert(tid.clone(), m);
        }
        self.agent_minutes = minutes;
        let changed = next != self.agent_statuses || rolled || details != self.agent_details;
        self.agent_statuses = next;
        self.agent_details = details;

        // Fire desktop toasts (best-effort, non-blocking) — the server does this, so
        // they arrive even while detached. Replaces the retired `~/.claude` notify hooks.
        for (tid, tool, body) in events {
            let kind = if body == "awaiting input" {
                "blocked"
            } else {
                "done"
            };
            // An agent whose own hooks push this event kind (`comux notify`) owns it: the
            // hook knows the real moment, the sweep only infers it ~2 Hz later, and both
            // firing would double every toast. Ownership is per (pane, agent pid, kind), so
            // a Codex pane that only ever pushes `done` still gets sweep-detected `blocked`,
            // and a NEW agent in the same pane starts unowned rather than silently muted.
            if self.hook_owns(&tid, kind) {
                continue;
            }
            self.raise_notification(tid, tool, kind, body.to_string());
        }
        changed
    }

    /// Is this (pane, live agent pid, kind) already served by the agent's own hook?
    fn hook_owns(&self, term: &TerminalId, kind: &'static str) -> bool {
        let pid = self.labels.get(term).map(|l| l.pid).unwrap_or(0);
        self.hook_owned.contains(&(term.clone(), pid, kind))
    }

    /// Record that the agent in `term` reports `kind` through its own hooks, so the
    /// status sweep stops toasting that kind for it (see [`Self::hook_owns`]).
    ///
    /// Sweeps the process table FIRST. The claim is keyed by the agent's pid, and the
    /// cached label is up to 500ms (attached) or 5s (detached) old — long enough that an
    /// agent's first turn can be claimed under the pid of the shell that launched it. The
    /// sweep would then look up the real agent pid, miss, and toast the event a second
    /// time, which is exactly what ownership exists to prevent. A hook push is a
    /// human-paced, rare event, so paying for one sweep here is cheap.
    fn claim_hook_kind(&mut self, term: &TerminalId, kind: &'static str) {
        self.refresh_labels();
        let pid = self.labels.get(term).map(|l| l.pid).unwrap_or(0);
        self.hook_owned.insert((term.clone(), pid, kind));
        // Bound the set: it grows with (pane × agent invocation × kind). Pruning keeps only
        // entries naming a live pane's CURRENT agent — a pane that has restarted its agent
        // fifty times holds fifty dead pids otherwise, and a dead pid's claim can never be
        // consulted again.
        if self.hook_owned.len() > HOOK_OWNED_CAP {
            let live: HashMap<TerminalId, u32> = self
                .panes
                .keys()
                .map(|t| {
                    let pid = self.labels.get(t).map(|l| l.pid).unwrap_or(0);
                    (t.clone(), pid)
                })
                .collect();
            self.hook_owned
                .retain(|(t, pid, _)| live.get(t) == Some(pid));
        }
    }

    /// Raise one agent notification: a desktop toast whose click jumps to the pane, plus
    /// an entry in the in-app center (`Ctrl-b a`). Shared by the status sweep and the
    /// `comux notify` push so both produce identical, jumpable notifications.
    /// Returns the `tool · space/tab` attribution it used, so a caller can SEE where its
    /// notification landed. A pushing hook (`comux notify`) has no other way to tell a
    /// mis-addressed pane from a correctly addressed one — the toast is the only other
    /// evidence and it is transient.
    fn raise_notification(
        &mut self,
        term: TerminalId,
        tool: String,
        kind: &'static str,
        body: String,
    ) -> String {
        let (space, tab) = self
            .locate_terminal(&term)
            .map(|(w, t, _)| {
                let ws = self.state.workspace(&w);
                let space = ws
                    .and_then(|ws| ws.name.clone())
                    .unwrap_or_else(|| w.to_string());
                let tab = ws
                    .and_then(|ws| ws.tabs.iter().position(|x| x.id == t).map(|i| (ws, i)))
                    .map(|(ws, i)| tab_display_title(ws.tabs[i].name.as_deref(), i))
                    .unwrap_or_default();
                (space, tab)
            })
            .unwrap_or_default();
        // Desktop toast: env override wins, else the config `notify` flag. The in-app
        // center below is logged regardless (it's not a desktop toast).
        let title = notify_title(&tool, &space, &tab);
        if notify::env_override().unwrap_or(self.cfg.notify) {
            let action = self.jump_action(&term);
            notify::desktop(&title, &body, action.as_deref(), &self.desktop_env());
        }
        self.notifications.push_front(Notification {
            when: local_hhmm(),
            kind,
            tool,
            space,
            tab,
            body,
            terminal: term,
            read: false,
        });
        while self.notifications.len() > NOTIFY_LOG_CAP {
            self.notifications.pop_back();
        }
        title
    }

    /// Number of agent panes currently BLOCKED (awaiting input) — the status-bar
    /// attention count (`⚑N`), replacing the retired tmux `⚑N` chip.
    fn attention_count(&self) -> usize {
        self.agent_statuses
            .values()
            .filter(|s| s.status == agentstate::AgentStatus::Blocked)
            .count()
    }

    /// An agent pane's rolled-up status label for the sidebar (`idle` if unknown), plus
    /// how many whole seconds it has held that status (0 when unknown).
    fn agent_status(&self, tid: &TerminalId) -> (&'static str, u64) {
        self.agent_statuses
            .get(tid)
            .map(|s| (s.status.label(), s.since.elapsed().as_secs()))
            .unwrap_or(("idle", 0))
    }

    /// A one-line label for a pane row (index + short cwd basename if available).
    /// Refresh every pane's foreground-process label from a single `ps` sweep
    /// (throttled by the caller to ~2 Hz — never per frame).
    fn refresh_labels(&mut self) -> bool {
        self.last_labels = std::time::Instant::now();
        let swept = procinfo::ProcTree::snapshot().map(|tree| {
            self.panes
                .iter()
                .map(|(tid, pane)| {
                    // The terminal's foreground process GROUP is the real foreground
                    // command (resolve a live member — the pgid leader may have exited
                    // in a pipeline); fall back to descending from the shell pid.
                    let mut label = pane
                        .foreground_pgid()
                        .and_then(|pg| tree.command_of_pgroup(pg))
                        .or_else(|| pane.pid().and_then(|p| tree.foreground(p)));
                    // One extra lookup for the ONE resolved pid, so the chip keeps
                    // naming what the user typed rather than the shim it resolved to.
                    if let Some(l) = label.as_mut() {
                        procinfo::refine_label(l);
                    }
                    (tid.clone(), label)
                })
                .collect::<Vec<_>>()
        });
        let Some(next) = merge_labels(&self.labels, swept) else {
            self.label_sweeps_failed = self.label_sweeps_failed.saturating_add(1);
            return false;
        };
        let changed = next != self.labels;
        self.labels = next;
        changed
    }

    /// How many process sweeps have FAILED outright since the server started (the
    /// tree could not be read at all). Non-zero means labels are being carried
    /// forward rather than refreshed; surfaced by `comux doctor` so the condition
    /// that used to be an unexplained "my tab names vanished" is observable.
    pub fn label_sweeps_failed(&self) -> u64 {
        self.label_sweeps_failed
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
            for (ti, t) in w.tabs.iter().enumerate() {
                // The tab's custom name titles its agent rows, so several agents in ONE
                // space stay distinguishable (the reason tab rename exists). Unnamed
                // tabs fall back to their 1-based INDEX rather than the space name: the
                // sidebar already groups by space, so repeating it would render every
                // agent in a space as the same untellable row — the index at least says
                // which `Ctrl-b <n>` jumps there.
                let tab_title = tab_display_title(t.name.as_deref(), ti);
                for p in t.layout.panes() {
                    if let Some(tid) = t.layout.terminal_of(&p)
                        && let Some(label) = self.labels.get(tid)
                        && label.kind == procinfo::Kind::Agent
                    {
                        let (status, for_secs) = self.agent_status(tid);
                        out.push(AgentRow {
                            title: tab_title.clone(),
                            detail: agentpoll::activity(&self.agent_poll, label.pid)
                                .map(|a| a.detail),
                            space: space.clone(),
                            space_id: wid.clone(),
                            tool: label.text.clone(),
                            status,
                            for_secs,
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
        let h = self.content_rows(); // between the top bar and the bottom status bar
        // The strip starts below the top bar when there is one. Every write below goes
        // through `put`/`put_trailing` or the fill loop, and every click zone is pushed with
        // `y0 +`, so the body keeps using 0-based rows and there is exactly one place the
        // offset can be forgotten.
        let y0 = self.content_y();
        // Transparent fill (default terminal bg) so a configured background image shows
        // through the sidebar — the right-border `│` column is the only visual separator.
        let panel_bg = Color::Reset;
        let sidebar_w = self.sidebar_w();

        // Fill the strip + the right border column (down to the status bar).
        for y in y0..y0 + h {
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
            let y = y0 + y; // rows below are 0-based within the strip
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
        // In compact mode a row's subtitle trails its name on the SAME line, right-aligned
        // to the strip edge, and is dropped whole rather than truncated when the name
        // already reaches it — a half-word of branch name is noise, not information.
        let put_trailing = |buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style| {
            // NOT offset here: it delegates to `put`, which already applies `y0`. Offsetting
            // in both would push every compact subtitle one row down.
            if text.is_empty() {
                return;
            }
            let w = UnicodeWidthStr::width(text) as u16;
            let at = sidebar_w.saturating_sub(w + 1);
            if at > x {
                let mut tx = at;
                put(buf, &mut tx, y, text, style);
            }
        };
        // The rule trailing an agents group header — darker than the muted text so the
        // header reads as a divider, not as an agent row with a missing status dot.
        let divider_style = Style::default().fg(CAT_SURFACE0).bg(panel_bg);

        // ---- split the strip between the two halves ----
        // Both halves are measured first so a half that needs less than its share hands
        // the slack to the other (a fixed 50/50 wasted the spaces half on a 2-session /
        // 12-agent setup, which is exactly the many-agents case the panel is for).
        let sids = self.sorted_session_ids();
        let agent_rows = self.agent_rows();
        let agent_items = agent_items(&agent_rows);
        // Agents awaiting input, pinned in a band above the list (see below). Counted
        // into the agents half's ask so the band can't be paid for out of the list.
        let blocked: Vec<&AgentRow> = agent_rows
            .iter()
            .filter(|r| r.status == "blocked")
            .collect();
        let want_band = if blocked.is_empty() {
            0
        } else {
            blocked.len() as u32 + 1 // 1 compact row each + the closing rule
        };
        // u32 accumulation so a pathological session/agent count can't overflow.
        // `ask` is a half's total row demand at a given rows-per-entry.
        let ask_spaces = |e: u16| 1 + e as u32 * sids.len() as u32;
        let ask_agents =
            |e: u16| 1 + want_band + agent_items.iter().map(|i| i.rows(e) as u32).sum::<u32>();
        // `auto` needs to know WHICH half overflows, and that only falls out of the split —
        // which in turn depends on the entry heights. Break the cycle by splitting once at
        // the comfortable size, reading off who overflowed, then splitting for real. One
        // extra round of arithmetic, and the outcome is deterministic.
        let probe = split_sidebar(h, ask_spaces(2), ask_agents(2), 2, 2);
        let entry_sp = self
            .cfg
            .sidebar_density
            .entry_rows(ask_spaces(2) > probe as u32);
        let entry_ag = self
            .cfg
            .sidebar_density
            .entry_rows(ask_agents(2) > h.saturating_sub(probe) as u32);
        let mid = split_sidebar(
            h,
            ask_spaces(entry_sp),
            ask_agents(entry_ag),
            entry_sp,
            entry_ag,
        );

        // ---- spaces (top half) ----
        put(buf, &mut 0, 0, " spaces", header);
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
        let space_max = (mid.saturating_sub(1) / entry_sp) as usize;
        let space_total = sids.len();
        let (space_start, space_vis, space_trunc) = if space_total <= space_max || space_max < 2 {
            (0, space_max.min(space_total), false)
        } else {
            let vis = space_max - 1; // reserve one row for the "+N more" hint
            (list_window_start(space_total, space_center, vis), vis, true)
        };
        // A wheel over this half pins the window; the pin is dropped the moment the
        // auto-anchor moves on its own, so switching session re-centres as before.
        let space_max_start = space_total.saturating_sub(space_vis);
        let space_anchor = sids.get(space_center).map(|w| w.to_string());
        let space_start = match &self.spaces_scroll {
            Some(sc) if sc.anchor == space_anchor => sc.start.min(space_max_start),
            _ => space_start,
        };
        *self.spaces_view.borrow_mut() = HalfView {
            start: space_start,
            max_start: space_max_start,
            anchor: space_anchor,
        };
        let mut y = 1u16;
        for (i, sid) in sids.iter().enumerate().skip(space_start).take(space_vis) {
            if y + entry_sp > mid {
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
            // Ellipsis instead of a hard cut, leaving room for the bell marker after it.
            let bell_w = if self.cfg.bell && self.session_has_bell(sid) {
                2
            } else {
                0
            };
            let budget = sidebar_w.saturating_sub(x + bell_w) as usize;
            put(buf, &mut x, y, &clip_width(&name, budget), name_style);
            // A bell anywhere in this space. Placed after the name so it does not shift the
            // names out of alignment when one space rings and the others do not.
            if self.cfg.bell && self.session_has_bell(sid) {
                put(
                    buf,
                    &mut x,
                    y,
                    " !",
                    Style::default()
                        .fg(CAT_YELLOW)
                        .bg(panel_bg)
                        .add_modifier(Modifier::BOLD),
                );
            }
            // subtitle: git branch (herdr-style), falling back to the focused command
            // when the cwd isn't a git repo.
            let sub = self
                .branches
                .get(sid)
                .cloned()
                .unwrap_or_else(|| self.session_focus_label(sid));
            if entry_sp > 1 {
                let mut sx = 4u16;
                let budget = sidebar_w.saturating_sub(sx) as usize;
                put(buf, &mut sx, y + 1, &clip_width(&sub, budget), sub_style);
            } else {
                put_trailing(buf, x, y, &sub, sub_style);
            }
            // The whole entry clicks to switch to this session.
            self.click_zones.borrow_mut().push(ClickZone {
                x0: 0,
                x1: sidebar_w - 1,
                y0: y0 + y,
                y1: y0 + y + entry_sp - 1,
                target: ClickTarget::Session(sid.clone()),
            });
            y += entry_sp;
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
        // ABSOLUTE screen row: it is compared against a raw mouse `y` to decide which half
        // the wheel is over.
        self.sidebar_mid.set(y0 + mid);
        let mut y = mid;
        put(buf, &mut 0, y, " agents", header);
        // `⚑N` on the header counts EVERY blocked agent, including any the band below had
        // no room for, so a capped band never under-reports what is waiting.
        if !blocked.is_empty() {
            let flag = format!("⚑{} ", blocked.len());
            let mut fx = sidebar_w.saturating_sub(UnicodeWidthStr::width(flag.as_str()) as u16);
            put(
                buf,
                &mut fx,
                y,
                &flag,
                Style::default()
                    .fg(CAT_YELLOW)
                    .bg(panel_bg)
                    .add_modifier(Modifier::BOLD),
            );
        }
        y += 1;
        // ---- attention band ----
        // Blocked agents are the ones that need a human RIGHT NOW, and in a long list they
        // can sit anywhere. Pinning them in a compact 1-row band at the top makes them
        // always visible WITHOUT reordering the list underneath (sorting by status would
        // move rows out from under the pointer every time an agent changed state). They are
        // deliberately shown twice: the band is the shortcut, the list below is the map.
        let multi_space = agents_span_spaces(&agent_rows);
        let band_n = band_rows(h.saturating_sub(mid + 1), blocked.len(), entry_ag);
        if band_n > 0 {
            for row in blocked.iter().take(band_n) {
                let mut x = 1u16;
                put(
                    buf,
                    &mut x,
                    y,
                    "▲ ",
                    Style::default()
                        .fg(CAT_YELLOW)
                        .bg(panel_bg)
                        .add_modifier(Modifier::BOLD),
                );
                let who = if multi_space {
                    format!("{}/{}", row.space, row.title)
                } else {
                    row.title.clone()
                };
                put(
                    buf,
                    &mut x,
                    y,
                    &who,
                    Style::default().fg(CAT_TEXT).bg(panel_bg),
                );
                // Right-align "how long it has been waiting" — the band is scanned down
                // its right edge to find the one that has been blocked longest.
                let waited = fmt_elapsed(row.for_secs);
                if !waited.is_empty() {
                    let at = sidebar_w
                        .saturating_sub(UnicodeWidthStr::width(waited.as_str()) as u16 + 1);
                    if at > x {
                        x = at;
                        put(buf, &mut x, y, &waited, sub_style);
                    }
                }
                self.click_zones.borrow_mut().push(ClickZone {
                    x0: 0,
                    x1: sidebar_w - 1,
                    y0: y0 + y,
                    y1: y0 + y,
                    target: ClickTarget::Agent(row.term.clone()),
                });
                y += 1;
            }
            // Rule closing the band off from the list (the list's own first group header
            // is not guaranteed — a single-space workspace has none).
            let mut rx = 1u16;
            while rx < sidebar_w {
                let before = rx;
                put(buf, &mut rx, y, "─", divider_style);
                if rx == before {
                    break;
                }
            }
            y += 1;
        }
        // The agent whose pane is currently in focus (active session's focused pane), so
        // it can be highlighted like the active `spaces` row — bright/bold name vs the
        // dimmed siblings — instead of every agent looking identical.
        let active_agent_term = self.focused_terminal();
        let agent_total = agent_rows.len();
        let agent_focus = match &self.sidebar_focus {
            Some(f) if f.tab == PopupTab::Agents => Some(f.sel.min(agent_total.saturating_sub(1))),
            _ => None,
        };
        // Anchor the window on the keyboard-focused row, else on the agent whose pane is
        // FOCUSED — parity with the spaces half centering on the active session. It used
        // to pin at index 0, so the agent you were looking at could be off-window.
        let anchor_agent = agent_focus.or_else(|| {
            let t = active_agent_term.as_ref()?;
            agent_rows.iter().position(|r| &r.term == t)
        });
        let anchor = anchor_agent
            .and_then(|a| agent_items.iter().position(|i| i.is_agent(a)))
            .unwrap_or(0);
        // Identity, not index: the pin below expires when the anchor MOVES, and a bare
        // index can stay equal while pointing at a different agent. The anchor is TOTAL —
        // with no agent focused it falls back to the active SESSION rather than `None`, or
        // two different sessions whose focused panes are both shells would compare equal
        // and carry a stale pin across the switch [codex S3a/C2].
        let anchor_id = anchor_agent
            .and_then(|a| agent_rows.get(a))
            .map(|r| r.term.to_string())
            .or_else(|| Some(self.ws.to_string()));
        let heights: Vec<u16> = agent_items.iter().map(|i| i.rows(entry_ag)).collect();
        let avail = h.saturating_sub(y);
        let need: u32 = heights.iter().map(|&r| r as u32).sum();
        // Reserve a row for the "+M more" hint when the list can't fit; the full list is
        // in Ctrl-f. Never at the cost of the last ENTRY, though — on a strip short enough
        // that the half is down to its floor (header + one 2-row entry), reserving the
        // hint row would leave the hint and no agent at all [codex C1].
        let budget = if need <= avail as u32 || avail < 3 {
            avail
        } else {
            avail - 1
        };
        let end_y = y.saturating_add(budget);
        let agent_max_start = window_max_start(&heights, budget);
        let start = match &self.agents_scroll {
            Some(sc) if sc.anchor == anchor_id => sc.start.min(agent_max_start),
            _ => window_start_var(&heights, anchor, budget),
        };
        *self.agents_view.borrow_mut() = HalfView {
            start,
            max_start: agent_max_start,
            anchor: anchor_id,
        };
        let mut shown = 0usize;
        for item in agent_items.iter().skip(start) {
            if y + item.rows(entry_ag) > end_y {
                break;
            }
            // A group header is ALWAYS followed by an agent entry (that is how
            // `agent_items` builds it), so on the last row it would render a heading with
            // nothing under it. Stop instead.
            if matches!(item, AgentItem::Group(..)) && y + item.rows(entry_ag) + entry_ag > end_y {
                break;
            }
            let ai = match item {
                AgentItem::Group(space, wid) => {
                    // Dim space name + a rule to the edge: with agents from several
                    // sessions in one flat list there was no way to tell which was which.
                    let mut gx = 1u16;
                    put(buf, &mut gx, y, space, sub_style);
                    put(buf, &mut gx, y, " ", sub_style);
                    while gx < sidebar_w {
                        let before = gx;
                        put(buf, &mut gx, y, "─", divider_style);
                        if gx == before {
                            break; // no room left — never spin
                        }
                    }
                    // The header clicks through to its session, like a `spaces` row.
                    self.click_zones.borrow_mut().push(ClickZone {
                        x0: 0,
                        x1: sidebar_w - 1,
                        y0: y0 + y,
                        y1: y0 + y,
                        target: ClickTarget::Session(wid.clone()),
                    });
                    y += 1;
                    continue;
                }
                AgentItem::Agent(ai) => *ai,
            };
            let row = &agent_rows[ai];
            shown += 1;
            let is_focused = agent_focus == Some(ai);
            // The active pane's agent — highlighted like the active `spaces` row.
            let is_active = active_agent_term.as_ref() == Some(&row.term);
            // tmx-style glyph + colour per status (all width-1 geometric shapes).
            let (dot, scolor) = match row.status {
                "working" => ("●", CAT_GREEN),
                "ready" => ("◐", CAT_BLUE),
                "blocked" => ("▲", CAT_YELLOW),
                _ => ("○", CAT_OVERLAY), // idle
            };
            let mut x = 1u16;
            // Keep the status colour on the dot; bold it for the focused agent so the
            // icon reads as "selected" (mirrors the active space's brighter dot).
            put(
                buf,
                &mut x,
                y,
                dot,
                Style::default()
                    .fg(scolor)
                    .bg(panel_bg)
                    .add_modifier(if is_active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            );
            let name_style = if is_focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_active {
                // The pane you're looking at: bright + bold, like the active space name.
                Style::default()
                    .fg(CAT_TEXT)
                    .bg(panel_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                // Other agents: dimmed to subtext so the focused one stands out.
                Style::default().fg(CAT_SUBTEXT).bg(panel_bg)
            };
            put(buf, &mut x, y, " ", name_style);
            let budget = sidebar_w.saturating_sub(x) as usize;
            put(buf, &mut x, y, &clip_width(&row.title, budget), name_style);
            let waited = fmt_elapsed(row.for_secs);
            if entry_ag > 1 {
                let sub = if waited.is_empty() {
                    format!("{} · {}", row.status, row.tool)
                } else {
                    format!("{} {} · {}", row.status, waited, row.tool)
                };
                let mut sx = 4u16;
                put(buf, &mut sx, y + 1, &sub, sub_style);
            } else {
                // One row: the tool name is the first thing to go — the tab chip already
                // carries it, while the status and how long it has held are the point.
                let sub = if waited.is_empty() {
                    row.status.to_string()
                } else {
                    format!("{} {}", row.status, waited)
                };
                put_trailing(buf, x, y, &sub, sub_style);
            }
            // The whole entry clicks to jump to this agent's pane.
            self.click_zones.borrow_mut().push(ClickZone {
                x0: 0,
                x1: sidebar_w - 1,
                y0: y0 + y,
                y1: y0 + y + entry_ag - 1,
                target: ClickTarget::Agent(row.term.clone()),
            });
            y += entry_ag;
        }
        if shown < agent_total && y < h {
            let hidden = agent_total - shown;
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
    /// Any pane in this tab with an unacknowledged bell.
    fn tab_has_bell(&self, id: &TabId) -> bool {
        self.state
            .workspace(&self.ws)
            .and_then(|w| w.tab(id))
            .is_some_and(|t| t.layout.terminals().into_iter().any(|tid| self.bell(tid)))
    }

    /// Any pane anywhere in this SESSION with an unacknowledged bell — the sidebar `spaces`
    /// row marker, so a bell outside the current session is not invisible.
    fn session_has_bell(&self, wid: &WorkspaceId) -> bool {
        self.state.workspace(wid).is_some_and(|w| {
            w.tabs
                .iter()
                .any(|t| t.layout.terminals().into_iter().any(|tid| self.bell(tid)))
        })
    }

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
    /// The optional top bar: how the HOST is doing (`cpu 12%  mem 59%  gpu 2%  load 2.36`).
    ///
    /// Deliberately a different question from the bottom bar, which is about the MUX (session,
    /// tabs, agents, subscription limits). Splitting them is the reason to have a second bar
    /// at all; duplicating the bottom bar's content up here would just cost a row.
    ///
    /// A reading that is absent is OMITTED, never drawn as `0%` or `--`: see `hostmetrics`.
    /// When nothing at all is readable the bar is still drawn — it was asked for, and an
    /// empty strip is a visible, diagnosable "no readings" rather than a layout that silently
    /// changes height.
    fn render_top_bar(&self, buf: &mut Buffer) {
        let w = buf.area.width;
        let y = 0u16;
        for x in 0..w {
            if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                bc.set_symbol(" ");
                bc.set_skip(false);
                bc.set_style(Style::default().bg(CAT_BASE));
            }
        }
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
        let label = Style::default().fg(CAT_OVERLAY).bg(CAT_BASE);
        let mut x = 1u16;
        for part in host_parts(&hostmetrics::read(&self.host_poll)) {
            put(buf, &mut x, &part.label, label);
            put(
                buf,
                &mut x,
                &part.value,
                Style::default()
                    .fg(part.color)
                    .bg(CAT_BASE)
                    .add_modifier(Modifier::BOLD),
            );
            put(buf, &mut x, "  ", label);
        }
    }

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
        // Prefix armed (`Ctrl-b` pressed, waiting for the chord) — FIRST, so it sits at
        // the left edge of the right cluster where the eye already goes for mode flags.
        // Transient by nature: the very next key resolves the chord and clears it, which
        // is also why it is fine for it to briefly shrink the tab-chip window.
        if self.prefix_armed {
            segs.push((
                format!(" {} ", self.prefix_label()),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_RED)
                    .add_modifier(Modifier::BOLD),
            ));
        }
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
        // Bells, mux-wide. The per-tab chip and per-session sidebar markers cannot be the
        // guarantee — the ringing session's row can be outside the sidebar's visible window,
        // and the sidebar can be hidden entirely on a narrow terminal. `!` is tmux's bell
        // convention, and it reads as distinct from `⚑` (attention) and `●` (agents).
        let bells = self.bell_count();
        if bells > 0 && self.cfg.bell {
            segs.push((
                format!(" ! {bells} "),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_YELLOW)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let unread = self.unread_count();
        if unread > 0 {
            segs.push((
                format!(" ✉ {unread} "),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_MAUVE)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(sp) = &self.scroll_pane {
            let pos = self
                .panes
                .get(sp)
                .map(|p| format!(" {}/{}", p.scroll_offset(), p.history_len()))
                .unwrap_or_default();
            segs.push((
                format!(" SCROLL{pos} · j/k g/G · q "),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_YELLOW)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if self.sidebar_focus.is_some() {
            segs.push((
                " SIDEBAR · j/k h/l · ↵ · esc ".to_string(),
                Style::default()
                    .fg(CAT_BASE)
                    .bg(CAT_BLUE)
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
                // `!` is tmux's bell convention. It precedes the agent dot so the two read
                // as separate facts rather than one glyph blob.
                let bell = if self.cfg.bell && self.tab_has_bell(id) {
                    "! "
                } else {
                    ""
                };
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
                (format!(" {bell}{dot}{inner} "), agent)
            })
            .collect();
        let tab_end = w.saturating_sub(right_total);
        // Prefix armed: the next keys in place of the tab chips, rather than over the bottom
        // pane row. `prefix_armed` means SOME client is mid-chord, and an idle one can leave it
        // set — covering everyone's terminal content for that long would be the wrong trade.
        let hint = (self.prefix_armed && self.cfg.prefix_hint)
            .then(|| self.prefix_hint_text())
            .filter(|h| !h.is_empty());
        let message = self.message.as_ref().filter(|_| hint.is_none());
        let chips = if hint.is_some() || message.is_some() {
            Vec::new()
        } else {
            chips
        };
        if let Some(h) = &hint {
            x = put_str(
                buf,
                x,
                y,
                tab_end,
                h,
                Style::default().fg(CAT_TEXT).bg(CAT_BASE),
            );
        } else if let Some(m) = message {
            let st = if m.error {
                Style::default().fg(CAT_BASE).bg(CAT_RED)
            } else {
                Style::default().fg(CAT_BASE).bg(CAT_YELLOW)
            };
            x = put_str(buf, x, y, tab_end, &format!(" {} ", m.text), st);
        }
        let widths: Vec<u16> = chips.iter().map(|(s, _)| s.width() as u16).collect();
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
        let sel = pinned_sel(p, &items);

        // Clamp bounds so `min <= max` even on tiny/narrow terminals (the popup
        // is the narrow/mobile fallback, so it must never panic there).
        let maxw = area.width.saturating_sub(2).max(1);
        // Wide enough for a preview: the box takes most of the frame and splits.
        let preview = area.width >= POPUP_PREVIEW_MIN_COLS && area.height >= POPUP_PREVIEW_MIN_ROWS;
        let w = if preview {
            (area.width as u32 * 9 / 10) as u16
        } else {
            (area.width / 2).clamp(24.min(maxw), maxw)
        };
        let maxh = area.height.saturating_sub(2).max(1);
        let h = if preview {
            (area.height as u32 * 4 / 5) as u16
        } else {
            ((area.height as u32 * 3 / 5) as u16).clamp(5.min(maxh), maxh)
        };
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        // Transparent interior (default terminal bg) so a background image shows through;
        // only the border + selection highlights remain painted.
        let bg = Color::Reset;
        let border = Style::default().fg(Color::Cyan).bg(bg);

        // Box background + border.
        draw_box(buf, x0, y0, w, h, border);

        let put = |buf: &mut Buffer, y: u16, x: u16, maxw: u16, s: &str, st: Style| {
            put_str(buf, x, y, x.saturating_add(maxw), s, st);
        };

        // Title in the top border (with the key hints).
        put(
            buf,
            y0,
            x0 + 2,
            w.saturating_sub(4),
            " go to  (↵ go · ←/→ tab · ^R rename · esc · ! working @ blocked # idle) ",
            Style::default()
                .fg(Color::Cyan)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );

        let inner_x = x0 + 2;
        let full_w = w.saturating_sub(4);
        // With a preview the list keeps the left ~40% and the pane gets the rest.
        let inner_w = if preview {
            (full_w * 2 / 5).max(30).min(full_w)
        } else {
            full_w
        };
        if preview {
            let rule_x = inner_x + inner_w + 1;
            for y in y0 + 1..y0 + h - 1 {
                if let Some(bc) = buf.cell_mut(Position::new(rule_x, y)) {
                    bc.set_symbol("│");
                    bc.set_skip(false);
                    bc.set_style(Style::default().fg(CAT_SURFACE0));
                }
            }
            let px = rule_x + 2;
            let pw = (x0 + w - 2).saturating_sub(px);
            let target = items.get(sel).map(|r| r.target.clone());
            self.render_popup_preview(buf, target.as_ref(), px, y0 + 1, pw, h.saturating_sub(2));
        }

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
        let start = if sel >= visible { sel + 1 - visible } else { 0 };
        for (row_i, item) in items.iter().enumerate().skip(start).take(visible) {
            let y = list_top + (row_i - start) as u16;
            let selected = row_i == sel;
            let arrow = if selected { "▸" } else { " " };
            let text = format!("{arrow} {} {}", item.glyph, item.text);
            let padded = pad_width(&text, inner_w as usize);
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

    /// The terminal the switcher previews for `target`: an agent's own pane, or a session's
    /// active tab's focused pane.
    fn preview_terminal(&self, target: &PopupTarget) -> Option<TerminalId> {
        match target {
            PopupTarget::Agent(t) => Some(t.clone()),
            PopupTarget::Session(wid) => {
                let w = self.state.workspace(wid)?;
                let tab = w.tab(&w.active_tab)?;
                tab.layout.terminal_of(&tab.focused).cloned()
            }
        }
    }

    /// The pane the open switcher is previewing, if any — so its output counts as visible
    /// for repainting (`drain_pane_dirty`), even when it lives in a background session.
    fn popup_preview_terminal(&self) -> Option<TerminalId> {
        let p = self.popup.as_ref()?;
        if self.cols < POPUP_PREVIEW_MIN_COLS || self.rows < POPUP_PREVIEW_MIN_ROWS {
            return None;
        }
        let items = self.popup_items(p.tab, &p.filter);
        self.preview_terminal(&items.get(pinned_sel(p, &items))?.target)
    }

    /// Draw the switcher's preview of `target` into `(x, y, w, h)`: a one-line header, then
    /// the pane's CURRENT viewport (what it would show if you went there — scrolled-up panes
    /// preview their scrollback), bottom-anchored on its content and cropped to the width.
    /// Every cell of the region is written, so nothing the pane underneath drew (a wide
    /// glyph's skipped spacer above all) survives inside it.
    fn render_popup_preview(
        &self,
        buf: &mut Buffer,
        target: Option<&PopupTarget>,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let blank = Style::default().bg(Color::Reset);
        for yy in y..y + h {
            for xx in x..x + w {
                if let Some(bc) = buf.cell_mut(Position::new(xx, yy)) {
                    bc.set_symbol(" ");
                    bc.set_skip(false);
                    bc.set_style(blank);
                }
            }
        }
        let Some(term) = target.and_then(|t| self.preview_terminal(t)) else {
            return;
        };
        let header = match self.locate_terminal(&term) {
            Some((wid, tab, _)) => {
                let loc = Loc {
                    ws: wid,
                    tab,
                    term: None,
                };
                let label = self
                    .labels
                    .get(&term)
                    .map(|l| l.text.clone())
                    .unwrap_or_default();
                let status = self
                    .agent_statuses
                    .get(&term)
                    .map(|s| format!(" {}", s.status.label()))
                    .unwrap_or_default();
                format!("{} · {label}{status}", self.loc_label(&loc))
            }
            None => String::new(),
        };
        put_str(
            buf,
            x,
            y,
            x + w,
            &header,
            Style::default()
                .fg(CAT_SUBTEXT)
                .add_modifier(Modifier::BOLD),
        );
        let Some(pt) = self.panes.get(&term) else {
            return;
        };
        let snap = pt.snapshot();
        let body_h = h.saturating_sub(1) as usize;
        // Bottom-anchor on the last row with content (or the cursor), so a mostly-empty pane
        // shows its prompt rather than a screen of blank rows.
        let last_used = snap
            .cells
            .iter()
            .rposition(|row| row.iter().any(|c| !c.spacer && c.sym.trim() != ""))
            .unwrap_or(0)
            .max(snap.cursor.1 as usize);
        let end = (last_used + 1).min(snap.cells.len());
        let start = end.saturating_sub(body_h);
        for (i, row) in snap.cells[start..end].iter().enumerate() {
            let yy = y + 1 + i as u16;
            for (cx, cell) in row.iter().enumerate() {
                let xx = x + cx as u16;
                if xx >= x + w {
                    break;
                }
                let Some(bc) = buf.cell_mut(Position::new(xx, yy)) else {
                    continue;
                };
                if cell.spacer {
                    bc.set_symbol(" ");
                    bc.set_skip(true);
                    continue;
                }
                let wide = cell.sym.width() >= 2;
                if wide && xx + 1 >= x + w {
                    // Its second column would fall outside the preview: drop it.
                    bc.set_symbol(" ");
                    continue;
                }
                let mut style = Style::default().fg(to_color(cell.fg)).bg(to_color(cell.bg));
                if cell.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.reverse {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                bc.set_symbol(&cell.sym);
                bc.set_style(style);
            }
        }
    }

    /// The `Ctrl-b ?` help rows from the LIVE keymap: `(chords, description)`, with `None`
    /// chords marking a section header. One row per action per table; tabs 1–9 fold into
    /// one row when bound to consecutive digits.
    fn help_lines(&self) -> Vec<(Option<String>, String)> {
        let km = &self.cfg.keymap;
        let mut out = Vec::new();
        for prefix in [true, false] {
            let rows: Vec<(Chord, Action)> = km
                .bindings()
                .into_iter()
                .filter(|(p, _, _)| *p == prefix)
                .map(|(_, c, a)| (c, a))
                .collect();
            if rows.is_empty() {
                continue;
            }
            out.push((
                None,
                if prefix {
                    format!("prefix — press {} first, then", self.prefix_label())
                } else {
                    "no prefix".to_string()
                },
            ));
            let mut seen: Vec<Action> = Vec::new();
            for (_, a) in &rows {
                if seen.contains(a) {
                    continue;
                }
                if let Action::SelectTab(_) = a {
                    // Every tab row at once: folded to `1–9` only for the conventional
                    // range, else one row per tab so a rebinding says which key goes where.
                    let mut tabs: Vec<(u8, String)> = rows
                        .iter()
                        .filter_map(|(c, b)| match b {
                            Action::SelectTab(n) => Some((*n, c.label())),
                            _ => None,
                        })
                        .collect();
                    tabs.sort();
                    seen.extend((0..9).map(Action::SelectTab));
                    match fold_tab_range(&tabs) {
                        Some(range) => push_help(&mut out, &[range], "go to tab 1–9"),
                        None => {
                            for n in 0..9u8 {
                                let chords: Vec<String> = tabs
                                    .iter()
                                    .filter(|(m, _)| *m == n)
                                    .map(|(_, l)| l.clone())
                                    .collect();
                                if !chords.is_empty() {
                                    push_help(&mut out, &chords, &format!("go to tab {}", n + 1));
                                }
                            }
                        }
                    }
                    continue;
                }
                seen.push(*a);
                let chords: Vec<String> = rows
                    .iter()
                    .filter(|(_, b)| b == a)
                    .map(|(c, _)| c.label())
                    .collect();
                push_help(&mut out, &chords, a.describe());
            }
        }
        out
    }

    /// Rows the help box shows at once on this terminal.
    fn help_visible(&self) -> usize {
        // Borders, and the frame's own status bar row.
        self.rows.saturating_sub(4) as usize
    }

    fn help_max_scroll(&self) -> usize {
        self.help_lines()
            .len()
            .saturating_sub(self.help_visible().max(1))
    }

    /// The status-bar hint while the prefix is armed: the most useful next keys, as bound
    /// NOW. An action left unbound simply drops out, `?` included.
    fn prefix_hint_text(&self) -> String {
        use Action::*;
        let mut parts: Vec<(String, &'static str)> = Vec::new();
        // `?` FIRST: on a narrow bar the hint is clipped from the right, and the key that
        // shows everything else is the one that must survive.
        for a in [
            KeyHelp,
            NewTab,
            NewSession,
            NavLast,
            NextSession,
            PrevSession,
            SplitRight,
            SplitDown,
            Scrollback,
            ResumePicker,
            Detach,
        ] {
            let (Some(short), Some(c)) = (
                a.short(),
                self.cfg.keymap.chords_for(true, a).first().copied(),
            ) else {
                continue;
            };
            match parts.last_mut() {
                // `% " split`: the two splits share one noun.
                Some((keys, s)) if *s == short => {
                    keys.push(' ');
                    keys.push_str(&c.label());
                }
                _ => parts.push((c.label(), short)),
            }
        }
        parts
            .iter()
            .map(|(k, s)| format!("{k} {s}"))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// The key help (`Ctrl-b ?`): a centered, scrollable list of the live keymap.
    fn render_help(&self, buf: &mut Buffer) {
        let Some(scroll) = self.help else { return };
        let area = buf.area;
        let lines = self.help_lines();
        let maxw = area.width.saturating_sub(2).max(1);
        let w = 72u16.min(maxw);
        let maxh = area.height.saturating_sub(2).max(1);
        let h = (lines.len() as u16 + 2).min(maxh).max(3.min(maxh));
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        let border = Style::default().fg(CAT_BLUE).bg(Color::Reset);
        draw_box(buf, x0, y0, w, h, border);
        let visible = h.saturating_sub(2) as usize;
        let start = scroll.min(lines.len().saturating_sub(visible));
        let more = if lines.len() > visible {
            format!(
                " {}–{}/{} · j/k scroll",
                start + 1,
                (start + visible).min(lines.len()),
                lines.len()
            )
        } else {
            String::new()
        };
        put_str(
            buf,
            x0 + 2,
            y0,
            x0 + w.saturating_sub(2),
            &format!(" keys{more} · any other key closes "),
            border.add_modifier(Modifier::BOLD),
        );
        let inner_x = x0 + 2;
        let max_x = x0 + w.saturating_sub(2);
        for (i, (chords, text)) in lines.iter().skip(start).take(visible).enumerate() {
            let y = y0 + 1 + i as u16;
            match chords {
                None => {
                    put_str(
                        buf,
                        inner_x,
                        y,
                        max_x,
                        text,
                        Style::default().fg(CAT_MAUVE).add_modifier(Modifier::BOLD),
                    );
                }
                Some(keys) => {
                    let end = put_str(
                        buf,
                        inner_x + 1,
                        y,
                        max_x,
                        keys,
                        Style::default().fg(CAT_YELLOW),
                    );
                    // Rows wrap their chords to HELP_KEY_W, so the description column lines
                    // up; a single chord wider than that pushes its own description over.
                    let desc_x = end.max(inner_x + 1 + HELP_KEY_W as u16) + 1;
                    put_str(buf, desc_x, y, max_x, text, Style::default().fg(CAT_TEXT));
                }
            }
        }
    }

    /// The notification center (`Ctrl-b a`): a centered box listing logged agent-turn
    /// events, newest first — Enter jumps to the source pane, `d` dismisses.
    /// The resume picker (`Ctrl-b R`): a centered box listing past Claude/Codex
    /// conversations, newest first, with the fuzzy filter on top. Wider than the `Ctrl-f`
    /// switcher because each row carries a prompt AND a path.
    fn render_resume(&self, buf: &mut Buffer) {
        let Some(st) = self.resume.as_ref() else {
            return;
        };
        let area = buf.area;
        let rows_data = self.resume_rows(st);

        let maxw = area.width.saturating_sub(2).max(1);
        let w = ((area.width as u32 * 4 / 5) as u16).clamp(30.min(maxw), maxw);
        let maxh = area.height.saturating_sub(2).max(1);
        let h = ((area.height as u32 * 3 / 4) as u16).clamp(6.min(maxh), maxh);
        let x0 = (area.width.saturating_sub(w)) / 2;
        let y0 = (area.height.saturating_sub(h)) / 2;
        let bg = Color::Reset;
        let border = Style::default().fg(CAT_MAUVE).bg(bg);

        draw_box(buf, x0, y0, w, h, border);

        // Display-width-aware writer (a Korean prompt is 2 columns per char): advances `x`.
        let right_edge = x0 + w - 1;
        let put = |buf: &mut Buffer, x: &mut u16, y: u16, s: &str, style: Style| {
            *x = put_str(buf, *x, y, right_edge, s, style);
        };

        let mut tx = x0 + 2;
        put(
            buf,
            &mut tx,
            y0,
            &format!(
                " resume  ({} · ^A {} · ^R rescan) ",
                if st.all { "all" } else { "interactive" },
                if st.all { "interactive" } else { "all" }
            ),
            Style::default()
                .fg(CAT_MAUVE)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );

        let inner_x = x0 + 2;
        let inner_w = w.saturating_sub(4) as usize;

        // Row 1: the fuzzy filter with a block cursor, and the match count on the right.
        let mut fx = inner_x;
        put(
            buf,
            &mut fx,
            y0 + 1,
            &clip_width(&format!("> {}█", st.filter), inner_w.saturating_sub(10)),
            Style::default().fg(CAT_TEXT).bg(bg),
        );
        let count = format!("{} found", rows_data.len());
        let mut cx = inner_x + (inner_w.saturating_sub(count.width())) as u16;
        put(
            buf,
            &mut cx,
            y0 + 1,
            &count,
            Style::default().fg(CAT_OVERLAY).bg(bg),
        );

        let list_top = y0 + 2;
        let visible = (h.saturating_sub(3)) as usize; // two header rows + bottom border
        if rows_data.is_empty() {
            let msg = if self.sessions_shown.is_empty() && self.scan_shown.is_none() {
                "  scanning ~/.claude and ~/.codex…"
            } else if self.sessions_shown.is_empty() {
                "  no Claude or Codex transcripts found"
            } else {
                "  (no match)"
            };
            let mut mx = inner_x;
            put(
                buf,
                &mut mx,
                list_top,
                msg,
                Style::default().fg(CAT_OVERLAY).bg(bg),
            );
            return;
        }

        let start = if st.sel >= visible {
            st.sel + 1 - visible
        } else {
            0
        };
        for (i, row) in rows_data.iter().enumerate().skip(start).take(visible) {
            let y = list_top + (i - start) as u16;
            let selected = i == st.sel;
            // Right side first: it is fixed-size, and the prompt gets whatever is left.
            let path = row.cwd.as_deref().map(home_short).unwrap_or_default();
            let right = format!("{path} · {}", row.age);
            let glyph = if row.live.is_some() { "●" } else { "▪" };
            let arrow = if selected { "▸" } else { " " };
            let head = format!("{arrow} {glyph} {:<6} ", row.tool.bin());
            let title = if row.title.is_empty() {
                "(no prompt recorded)"
            } else {
                &row.title
            };
            let line = resume_line(&head, title, &right, inner_w);

            let st_row = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(CAT_MAUVE)
                    .add_modifier(Modifier::BOLD)
            } else if row.live.is_some() {
                Style::default().fg(CAT_GREEN).bg(bg)
            } else {
                Style::default().fg(CAT_TEXT).bg(bg)
            };
            let mut x = inner_x;
            put(buf, &mut x, y, &line, st_row);
        }
    }

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

        draw_box(buf, x0, y0, w, h, border);

        let put = |buf: &mut Buffer, y: u16, x: u16, maxw: u16, s: &str, st: Style| {
            put_str(buf, x, y, x.saturating_add(maxw), s, st);
        };

        let title = match p.kind {
            PromptKind::NewSession => " new session · ↵ create · esc cancel ",
            PromptKind::NewWorktree => " new worktree (branch) · ↵ create · esc cancel ",
            PromptKind::RenameSession(_) => " rename session · ↵ save · esc cancel ",
            PromptKind::RenameTab(..) => " rename tab · ↵ save (empty clears) · esc cancel ",
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
        let shown = tail_width(&format!("{}█", p.buf), inner_w);
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

        draw_box(buf, x0, y0, w, h, border);
        let inner_w = w.saturating_sub(4);
        put_str(
            buf,
            x0 + 2,
            y0 + 1,
            x0 + 2 + inner_w,
            &cf.message,
            Style::default().fg(CAT_TEXT).bg(bg),
        );
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

        draw_box(buf, x0, y0, w, h, border);

        // Width-aware writer (a wide session/tab name must not corrupt the border).
        let put = |buf: &mut Buffer, x: u16, y: u16, max_x: u16, s: &str, st: Style| {
            put_str(buf, x, y, max_x, s, st);
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
            let text = format!(" {}", pad_width(label, inner_w.saturating_sub(1)));
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

        draw_box(buf, x0, y0, w, h, border);

        let put = |buf: &mut Buffer, y: u16, x: u16, maxw: u16, s: &str, st: Style| {
            put_str(buf, x, y, x.saturating_add(maxw), s, st);
        };

        let inner_x = x0 + 2;
        let inner_w = w.saturating_sub(4);
        put(
            buf,
            y0,
            inner_x,
            inner_w,
            &format!(
                " notifications ({}, {} unread) — j/k · enter jump · d dismiss · D clear · q ",
                self.notifications.len(),
                self.unread_count()
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
                "{}{} {} {} {}  {}",
                if selected { "▸" } else { " " },
                if note.read { " " } else { "•" },
                note.when,
                glyph,
                notify_title(&note.tool, &note.space, &note.tab),
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
            let padded = pad_width(&row, inner_w as usize);
            put(buf, y, inner_x, inner_w, &padded, st);
        }
    }
}

/// Whether a history entry still names a live tab. Its pane may be gone: landing then keeps
/// whichever pane that tab focuses now.
fn loc_live(state: &State, l: &Loc) -> bool {
    state
        .workspace(&l.ws)
        .is_some_and(|w| w.tab(&l.tab).is_some())
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
/// `/Users/me/dev/copad` → `~/dev/copad`, so a row spends its width on the part that
/// distinguishes it. Left alone when it isn't under `$HOME`.
fn home_short(path: &std::path::Path) -> String {
    home_short_in(path, std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

/// [`home_short`] with `$HOME` passed in, so it can be asserted without mutating the process
/// environment (which a parallel test run shares).
fn home_short_in(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home.map(|h| path.strip_prefix(h)) {
        Some(Ok(rest)) if rest.as_os_str().is_empty() => "~".into(),
        Some(Ok(rest)) => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// Truncate `s` to at most `max` DISPLAY columns (`…` marks the cut), so a CJK prompt in a
/// picker row can't overrun its box the way a char count would.
fn clip_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new(); // not even the `…` fits
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw + 1 > max {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Columns the help gives the key column before the description.
const HELP_KEY_W: usize = 14;

/// Push one action's help row, wrapping its chords onto continuation rows (description on the
/// first only) instead of clipping them: the help promises EVERY binding.
fn push_help(out: &mut Vec<(Option<String>, String)>, chords: &[String], desc: &str) {
    let mut line = String::new();
    let mut first = true;
    for c in chords {
        if !line.is_empty() && line.width() + 1 + c.width() > HELP_KEY_W {
            let d = if first {
                desc.to_string()
            } else {
                String::new()
            };
            out.push((Some(std::mem::take(&mut line)), d));
            first = false;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(c);
    }
    let d = if first {
        desc.to_string()
    } else {
        String::new()
    };
    out.push((Some(line), d));
}

/// `1 … 9` (or `M-1 … M-9`) as `1–9` / `M-1–9` — only when tab `n` has exactly the one chord
/// `<stem><n+1>`. Anything else is not a range and is listed per tab.
fn fold_tab_range(tabs: &[(u8, String)]) -> Option<String> {
    if tabs.len() != 9 {
        return None;
    }
    let stem = tabs[0].1.strip_suffix('1')?;
    let conventional = tabs
        .iter()
        .enumerate()
        .all(|(i, (n, label))| *n as usize == i && *label == format!("{stem}{}", i + 1));
    conventional.then(|| format!("{stem}1–9"))
}

/// Draw an overlay box: a rounded-free single-line border in `border`, and a CLEARED interior
/// (the default terminal background, so a background image shows through). Every overlay
/// (switcher, resume picker, prompt, confirm, menu, notification center) draws through here.
fn draw_box(buf: &mut Buffer, x0: u16, y0: u16, w: u16, h: u16, border: Style) {
    let area = buf.area;
    if w == 0 || h == 0 {
        return;
    }
    let (x1, y1) = (x0 + w - 1, y0 + h - 1);
    for y in y0..(y0 + h).min(area.height) {
        for x in x0..(x0 + w).min(area.width) {
            let sym = match (x, y) {
                _ if y == y0 && x == x0 => "┌",
                _ if y == y0 && x == x1 => "┐",
                _ if y == y1 && x == x0 => "└",
                _ if y == y1 && x == x1 => "┘",
                _ if y == y0 || y == y1 => "─",
                _ if x == x0 || x == x1 => "│",
                _ => " ",
            };
            if let Some(bc) = buf.cell_mut(Position::new(x, y)) {
                bc.set_symbol(sym);
                // A wide glyph in the pane underneath leaves its trailing cell flagged as
                // skipped; an overlay cell that keeps the flag is dropped from the diff.
                bc.set_skip(false);
                let edge = y == y0 || y == y1 || x == x0 || x == x1;
                bc.set_style(if edge {
                    border
                } else {
                    Style::default().bg(Color::Reset)
                });
            }
        }
    }
}

/// Write `s` at `(x, y)` by DISPLAY width, stopping before column `max_x` (exclusive).
///
/// A char count is wrong for any wide glyph: a Korean session name is 2 columns per char,
/// so counting chars ran rows past the box border and misaligned the padding. A wide glyph
/// that would straddle `max_x` is dropped rather than half-drawn, and its trailing cell is
/// blanked + skipped so the client's diff stays in step. Returns the column after the text.
fn put_str(buf: &mut Buffer, mut x: u16, y: u16, max_x: u16, s: &str, st: Style) -> u16 {
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if cw == 0 {
            continue;
        }
        if x + cw > max_x {
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
    x
}

/// Fit `s` to EXACTLY `width` DISPLAY columns: clipped by columns, then padded with spaces.
/// A `{:<width$}` counts chars, which left a CJK row's highlight bar short of the box edge;
/// and clipping must happen here rather than in the writer, because a wide glyph dropped at
/// the edge leaves one column the padding never reaches.
fn pad_width(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push_str(&" ".repeat(width - used));
    out
}

/// The END of `s` that fits in `max` display columns — a prompt keeps its caret visible as
/// the input grows past the box.
fn tail_width(s: &str, max: usize) -> String {
    let mut used = 0usize;
    let mut start = s.len();
    for (i, ch) in s.char_indices().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max {
            break;
        }
        used += cw;
        start = i;
    }
    s[start..].to_string()
}

/// How closely a space's pane directory `pane` relates to `target`, the directory a
/// conversation ran in. Higher is better; `None` = unrelated.
///
/// Both containment directions count, because either can be the space the user thinks of as
/// "that project": a `/repo` conversation should find a space sitting in `/repo/src`, and a
/// `/repo/src` conversation should find a space sitting at `/repo`. Among containing spaces
/// the DEEPEST wins (closest to the conversation); among nested ones the SHALLOWEST does
/// (`/repo` beats `/repo/a/b/c` for a `/repo` conversation).
/// How well the resume picker's filter matches one row's text: `Some(0)` = the query appears
/// verbatim, `Some(1)` = its chars appear in order (fzf-style), `None` = no match. An empty
/// query matches everything at the top tier. Case-insensitive for ASCII, like `fuzzy_match`.
fn resume_rank(query: &str, hay: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    if hay
        .to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
    {
        return Some(0);
    }
    fuzzy_match(query, hay).then_some(1)
}

/// One picker row as a single padded line: `head` + as much of `title` as fits, then `right`
/// pushed to the far edge. Padded to exactly `inner_w` display columns so the selection
/// highlight covers the row, and never wider than that even with CJK text.
fn resume_line(head: &str, title: &str, right: &str, inner_w: usize) -> String {
    let right = clip_width(right, inner_w / 2);
    let avail = inner_w.saturating_sub(head.width() + right.width() + 2);
    let left = format!("{head}{}", clip_width(title, avail));
    let pad = inner_w.saturating_sub(left.width() + right.width());
    format!("{left}{}{right}", " ".repeat(pad))
}

fn cwd_affinity(pane: &std::path::Path, target: &std::path::Path) -> Option<(u8, usize)> {
    let depth = pane.components().count();
    if pane == target {
        Some((3, usize::MAX))
    } else if target.starts_with(pane) {
        Some((2, depth))
    } else if pane.starts_with(target) {
        Some((1, usize::MAX - depth))
    } else {
        None
    }
}

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

/// The human outcome message for a `reload` (tmux `source-file`): the reloaded path,
/// any parse warnings (one per line), and the standing reminder that server-lifetime
/// settings still need a restart. Pure so it can be asserted without spawning a server.
fn reload_note(path: &std::path::Path, warnings: &[String]) -> String {
    let mut msg = format!("reloaded {}", path.display());
    if !warnings.is_empty() {
        msg.push_str("\n  warnings:");
        for w in warnings {
            msg.push_str("\n    ");
            msg.push_str(w);
        }
    }
    msg.push_str(
        "\n  note: environment refresh/never_inherit and the persistence toggle/cadence \
         (persist, autosave_secs) are fixed at server start — run `comux server restart` to \
         change those",
    );
    msg
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

/// Drop `BOLD` from every composed cell that also carries `DIM`, so the two are never set
/// together. DIM wins: an inactive pane is dimmed precisely to de-emphasise it, and a faint-bold
/// cell is a combination terminals render inconsistently anyway.
///
/// This is not cosmetic — it dodges an asymmetry in ratatui's crossterm emit that otherwise
/// produces permanent residue. `ModifierDiff` writes `NormalIntensity` (`ESC [ 22 m`) to remove
/// DIM, and that sequence cancels BOLD as well; the mirror case is handled (removing BOLD while
/// keeping DIM re-emits `Dim`) but removing DIM while KEEPING BOLD is not, so BOLD is silently
/// dropped on the wire while ratatui's cached buffer still believes it is set — and a belief that
/// does not match the screen is never repainted by an incremental diff. comux hits the transition
/// constantly: `render_to` dims every inactive pane, so any row crossing from an inactive pane
/// into a divider or the focused pane is exactly `DIM|BOLD -> BOLD`.
///
/// With the sets kept disjoint, `to` containing BOLD means BOLD lands in `added` and is re-emitted
/// after the `NormalIntensity`, which is correct. A whole-buffer sweep rather than a fix at each
/// call site because DIM is written by the pane loop, the sidebar's group headers and the status
/// bar's muted labels, and the invariant has to hold no matter which of them ran.
/// Guarded by `dim_and_bold_are_never_set_together`.
fn resolve_dim_bold(buf: &mut Buffer) {
    let (w, h) = (buf.area.width, buf.area.height);
    for y in 0..h {
        for x in 0..w {
            if let Some(c) = buf.cell_mut(Position::new(x, y))
                && c.modifier.contains(Modifier::DIM)
            {
                c.modifier.remove(Modifier::BOLD);
            }
        }
    }
}

/// Current Unix time in seconds (0 on a clock before the epoch — informational only).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fold one process sweep into the pane-label map.
///
/// `swept` is `None` when the SWEEP ITSELF failed, and `Some(answer per live pane)`
/// otherwise — where a pane's `None` means the sweep ran but could not resolve that
/// one pane. Both levels RETAIN the previous label, because an absent label is never
/// merely cosmetic: it drops the tab chip to a bare index, empties the sidebar agents
/// list and `● N` counts, clears that pane's agent status (swallowing the next
/// turn-finished/awaiting-input toast, which needs a KNOWN previous status), and makes
/// an autosave landing in the window write a snapshot with NO agent restore command
/// (`layout_to_persist` gates on the label), so the next server restart silently comes
/// back as a bare shell. A whole-sweep failure used to blank all of that at once.
///
/// Returns `None` for a failed sweep (caller keeps its map untouched). On a successful
/// sweep, panes absent from `swept` have been reaped and are dropped — so a retained
/// label can only ever belong to a pane that still exists.
fn merge_labels(
    prev: &HashMap<TerminalId, procinfo::Label>,
    swept: Option<Vec<(TerminalId, Option<procinfo::Label>)>>,
) -> Option<HashMap<TerminalId, procinfo::Label>> {
    let swept = swept?;
    let mut next = HashMap::with_capacity(swept.len());
    for (tid, label) in swept {
        if let Some(label) = label.or_else(|| prev.get(&tid).cloned()) {
            next.insert(tid, label);
        }
    }
    Some(next)
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

/// `env` plus a freshly minted `COPAD_MUX_PANE` identity — the `$TMUX_PANE` analogue a
/// pane's own shell (and any hook running in it) uses to name itself back to comux. Every
/// spawn path funnels through here so no pane is born anonymous; the restore path needs it
/// most, since restored agent panes are exactly the ones that raise notifications.
fn with_pane_token(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.retain(|(k, _)| k != term::PANE_TOKEN_ENV);
    env.push((term::PANE_TOKEN_ENV.to_string(), term::next_pane_token()));
    env
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
) -> Option<(WorkspaceId, u64, HashMap<TabId, TerminalId>)> {
    let viewport = Rect { cols, rows };
    // Owner refs are snapshot-global (an owner can sit in another session), so they are
    // resolved only once every session is installed.
    let mut ref_terms: HashMap<u32, TerminalId> = HashMap::new();
    let mut owned_tabs: Vec<(TabId, u32)> = Vec::new();
    let mut budget = MAX_TOTAL_PANES; // total panes across all sessions
    let mut installed: Vec<WorkspaceId> = Vec::new();
    let mut active_ws: Option<WorkspaceId> = None;

    for (si, ps) in snap.sessions.iter().enumerate() {
        // Phase 1: spawn+prune every tab OFF-STATE.
        let mut restored_tabs: Vec<RestoredTab> = Vec::new();
        let mut tab_paneterms: Vec<Vec<RestoredPane>> = Vec::new();
        // Parallel to `restored_tabs`: a pruned tab must not shift its owner onto the next.
        let mut tab_owner_refs: Vec<Option<u32>> = Vec::new();
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
                tab_owner_refs.push(pt.owner);
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
            for (tid, (pane_term, pane_ref)) in terms.into_iter().zip(pts) {
                if let Some(r) = pane_ref {
                    ref_terms.entry(r).or_insert_with(|| tid.clone());
                }
                panes.insert(tid, pane_term);
            }
        }
        if let Some(w) = state.workspace(&id) {
            for (t, owner) in w.tabs.iter().zip(&tab_owner_refs) {
                if let Some(r) = owner {
                    owned_tabs.push((t.id.clone(), *r));
                }
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
    // A ref whose leaf was pruned (or never existed — the file is untrusted) owns nothing.
    let owners = owned_tabs
        .into_iter()
        .filter_map(|(tab, r)| ref_terms.get(&r).map(|tid| (tab, tid.clone())))
        .collect();
    // Restored ids are `local, s1, .., s{n-1}`; next new session must be `s{n}`.
    Some((active, installed.len() as u64, owners))
}

/// A restored leaf's PTY and the owner `pane_ref` its leaf carried.
type RestoredPane = (PaneTerm, Option<u32>);

/// Spawn PTYs for a persisted layout, pruning any leaf whose shell can't spawn. Returns
/// the pruned `LayoutSpec` (live leaves only) + the `PaneTerm`s in DFS pre-order, each with
/// its leaf's owner `pane_ref`, or `None`
/// if the whole subtree died. Enforces the depth cap and the shared pane `budget`.
fn spawn_layout(
    node: &PLayout,
    cols: u16,
    rows: u16,
    depth: usize,
    budget: &mut usize,
    sock_env: &[(String, String)],
) -> Option<(LayoutSpec, Vec<RestoredPane>)> {
    match node {
        PLayout::Leaf {
            cwd,
            command,
            pane_ref,
        } => {
            if *budget == 0 {
                return None;
            }
            let dir = resolve_cwd(cwd.as_deref());
            // A pane that can't spawn is pruned from the restore (transactional restore
            // drops failures rather than materializing a dead pane); the reason is not
            // actionable per-pane here, and `health` reports the budget if it was fd
            // exhaustion that pruned them.
            let pt = PaneTerm::spawn_with_env(
                cols.max(1),
                rows.max(1),
                None,
                dir,
                &with_pane_token(sock_env.to_vec()),
            )
            .ok()?;
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
            Some((LayoutSpec::Leaf, vec![(pt, *pane_ref)]))
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

/// The most messages one inbox holds; a full inbox drops its oldest and counts it.
const INBOX_CAP: usize = 256;
/// The largest message body. Anything bigger belongs in a `--result` file.
const INBOX_BODY_MAX: usize = 16 * 1024;
/// The longest `--result` path.
const INBOX_RESULT_MAX: usize = 4096;
/// The most serialized message bytes one `inbox` response carries — far below the client's
/// response cap even for one maximal message escaped to six bytes a character.
const INBOX_BATCH_BYTES: usize = 1024 * 1024;

/// One pane's inbox (`notify --to`). `seq` starts at 1 so `ack: 0` acknowledges nothing.
#[derive(Default)]
struct Mailbox {
    next_seq: u64,
    msgs: VecDeque<control::InboxMsg>,
    /// Dropped since the reader last acknowledged.
    dropped: u64,
}

/// What a new tab runs, and where. The default is a login shell in the server's cwd, created
/// behind the active tab.
#[derive(Default)]
struct TabLaunch {
    cwd: Option<PathBuf>,
    /// Make it its session's active tab.
    activate: bool,
    /// Run this argv as the pane's process instead of a login shell (see [`spawn_argv`]).
    argv: Option<Vec<String>>,
    /// Added to the pane's environment, after (so over) the usual pane variables.
    env: Vec<(String, String)>,
}

/// The argv a `comux spawn` pane actually runs: `argv` handed to the user's login shell as
/// POSITIONAL parameters — `$SHELL -l -i -c 'exec "$@"' comux-spawn <argv…>` — so PATH and
/// profile apply exactly as in a normal pane, yet nothing is ever typed into a shell: no
/// quoting, no line editor, no startup race that could eat or mangle the command.
///
/// `exec` makes the program the pane's process, so the pane ends when it does. `keep`
/// instead runs it as a child and then becomes an interactive login shell, leaving the output
/// (or the error) on screen. `-i` keeps an interrupted program from taking that shell down
/// with it, and sources the same rc files a pane's shell does.
///
/// The `"$@"` script is POSIX; a shell that is not (fish, nu, …) is replaced by `/bin/sh` as
/// the wrapper — the program still gets its argv verbatim, but that shell's own profile does
/// not apply.
fn spawn_argv(shell: Option<&str>, argv: &[String], keep: bool) -> Vec<String> {
    const POSIX: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "mksh", "yash"];
    let posix = shell.filter(|sh| {
        Path::new(sh)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| POSIX.contains(&n))
    });
    let (wrapper, flags): (&str, &[&str]) = match posix {
        Some(sh) => (sh, &["-l", "-i", "-c"]),
        None => ("/bin/sh", &["-l", "-c"]),
    };
    let script = if keep {
        r#""$@"; exec "${SHELL:-/bin/sh}" -l"#
    } else {
        r#"exec "$@""#
    };
    let mut out = vec![wrapper.to_string()];
    out.extend(flags.iter().map(|f| f.to_string()));
    out.push(script.to_string());
    out.push("comux-spawn".to_string());
    out.extend(argv.iter().cloned());
    out
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
/// just CROSSED the primary↔alt boundary in EITHER direction (its bit flipped). Panes no
/// longer visible are dropped from `prev` so a later re-appearance re-seeds cleanly (and can't
/// spuriously fire). Split out as a pure function so the edge logic is unit-testable without a
/// live PTY.
fn detect_alt_screen_transition(
    prev: &mut HashMap<TerminalId, bool>,
    visible: &[(TerminalId, bool)],
) -> bool {
    let mut transitioned = false;
    for (tid, now) in visible {
        let was = prev.insert(tid.clone(), *now).unwrap_or(false);
        if was != *now {
            transitioned = true;
        }
    }
    prev.retain(|tid, _| visible.iter().any(|(v, _)| v == tid));
    transitioned
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
                // ASCII space only — the grid's padding. `trim_end()` would also eat a
                // trailing U+00A0 the program wrote, silently altering what gets copied.
                out.push_str(line.trim_end_matches(' '));
                if row < end.1 {
                    out.push('\n');
                }
            }
        }
        row += 1;
    }
    out
}

/// One row-group in the sidebar's `agents` half: a space header (1 row) or an agent
/// entry (2 rows — name + `status · tool`). The two heights are why the agents half
/// windows with [`window_start_var`] rather than the uniform [`list_window_start`].
enum AgentItem {
    /// Space name + its session, emitted only when the agents span MORE than one space.
    Group(String, WorkspaceId),
    /// Index into the `agent_rows()` vec.
    Agent(usize),
}

impl AgentItem {
    /// `entry_h` is the configured rows-per-entry (`sidebar_density`); a group header is
    /// always one row.
    fn rows(&self, entry_h: u16) -> u16 {
        match self {
            AgentItem::Group(..) => 1,
            AgentItem::Agent(_) => entry_h,
        }
    }

    /// Is this the entry for agent index `i`? (For locating the window anchor.)
    fn is_agent(&self, i: usize) -> bool {
        matches!(self, AgentItem::Agent(a) if *a == i)
    }
}

/// How many rows the attention band may take out of `region` — the agents half below its
/// header — given how many agents are `blocked`. A quarter of the region, floored at one
/// row, but never at the cost of the list's own floor: one entry (`entry_h` rows, per
/// `sidebar_density`) plus the band's
/// closing rule. A strip too short for both drops the BAND, not the map, since the list is
/// what carries the focused-agent anchor [codex S2/C1].
fn band_rows(region: u16, blocked: usize, entry_h: u16) -> usize {
    let keep = 1 + entry_h; // the closing rule + one list entry
    let cap = ((region / 4).max(1) as usize).min(region.saturating_sub(keep) as usize);
    blocked.min(cap)
}

/// Do the agents live in more than one space? Decides both whether the list gets group
/// headers and whether an attention-band row needs a `space/` prefix — the band is flat,
/// so without it four `tab 2` rows from four sessions read identically.
///
/// `agent_rows()` yields agents grouped by space, so comparing neighbours is enough.
fn agents_span_spaces(rows: &[AgentRow]) -> bool {
    rows.windows(2).any(|w| w[0].space_id != w[1].space_id)
}

/// Interleave space group headers into the agent list. `agent_rows()` already yields
/// agents grouped by space (it walks workspaces in order), so a header goes wherever the
/// space CHANGES. Skipped entirely for a single space — the header would be pure noise
/// and it costs a row that the agents themselves want.
fn agent_items(rows: &[AgentRow]) -> Vec<AgentItem> {
    let multi_space = agents_span_spaces(rows);
    let mut out = Vec::with_capacity(rows.len());
    let mut cur: Option<&WorkspaceId> = None;
    for (i, r) in rows.iter().enumerate() {
        if multi_space && cur != Some(&r.space_id) {
            out.push(AgentItem::Group(r.space.clone(), r.space_id.clone()));
            cur = Some(&r.space_id);
        }
        out.push(AgentItem::Agent(i));
    }
    out
}

/// The row at which the sidebar's `agents` half starts, given what each half WANTS
/// (its header + all its entries). A half that needs less than its share hands the
/// slack to the other — the old fixed 50/50 spent half the strip on a 2-session list
/// while 12 agents truncated to 7, which is the many-agents case the panel exists for.
/// Falls back to an even split only when both halves overflow.
fn split_sidebar(h: u16, want_spaces: u32, want_agents: u32, entry_sp: u16, entry_ag: u16) -> u16 {
    // Each half's floor is its header plus one entry, so it scales with `sidebar_density`:
    // a compact half needs 2 rows where a comfortable one needs 3.
    let (floor_sp, floor_ag) = (1 + entry_sp, 1 + entry_ag);
    if h < floor_sp + floor_ag {
        // Too short for either half to hold anything — keep the old split verbatim
        // rather than an approximation of it [codex R2/C1].
        return (h / 2).max(2);
    }
    let (h32, half) = (h as u32, (h / 2) as u32);
    let mid = if want_spaces <= half || want_spaces + want_agents <= h32 {
        // Spaces fit within their share (or everything fits): give them exactly what
        // they need and let the agents have the whole remainder.
        want_spaces
    } else if want_agents <= h32 - half {
        // Mirror: a short agents list keeps its size, spaces absorb the slack.
        h32 - want_agents
    } else {
        half // both overflow — split evenly
    };
    // Always leave each half a header + one entry's worth of rows.
    (mid.min(h32) as u16).clamp(floor_sp, h - floor_ag)
}

/// The furthest a variable-height window can scroll and still be FULL: the smallest start
/// whose tail fits in `avail`. The uniform-height equivalent is `total - visible`.
fn window_max_start(heights: &[u16], avail: u16) -> usize {
    let mut used = 0u16;
    let mut i = heights.len();
    while i > 0 {
        let h = heights[i - 1];
        if used.saturating_add(h) > avail {
            break;
        }
        used += h;
        i -= 1;
    }
    i
}

/// Start index into a VARIABLE-height item list such that `anchor` is visible within
/// `avail` rows, keeping it roughly centered. Mirrors [`list_window_start`] for lists
/// whose items are not all the same height (the agents half's group headers).
fn window_start_var(heights: &[u16], anchor: usize, avail: u16) -> usize {
    if heights.is_empty() || avail == 0 {
        return 0;
    }
    let anchor = anchor.min(heights.len() - 1);
    // Spend at most half the budget on items ABOVE the anchor — and never so much that the
    // anchor itself no longer fits. Without the second cap a 2-row entry under a 1-row
    // group header could be pushed out of a 2-row window by its own header, which is the
    // opposite of what anchoring is for [codex C1].
    let cap = (avail / 2).min(avail.saturating_sub(heights[anchor]));
    let mut start = anchor;
    let mut above = 0u16;
    while start > 0 && above + heights[start - 1] <= cap {
        start -= 1;
        above += heights[start];
    }
    // If the tail from `start` leaves slack (anchor near the end of the list), pull the
    // window further back so it stays full instead of trailing blank rows.
    let mut total: u16 = heights[start..]
        .iter()
        .fold(0u16, |a, &b| a.saturating_add(b));
    while start > 0 && total.saturating_add(heights[start - 1]) <= avail {
        start -= 1;
        total = total.saturating_add(heights[start]);
    }
    start
}

/// Every session (id, name) and its tabs (id, name), for the selector resolver.
type SelectorSnap = Vec<(WorkspaceId, Option<String>, Vec<(TabId, Option<String>)>)>;

/// Why a session/tab address did not resolve: a [`control::Resp::code`] and a message.
type AddrErr = (&'static str, String);

fn addr_err(code: &'static str, msg: impl Into<String>) -> AddrErr {
    (code, msg.into())
}

/// Borrow a [`App::selector_snapshot`] as the resolver's view.
fn selector_views(snap: &SelectorSnap) -> Vec<selector::SessionView<'_>> {
    snap.iter()
        .map(|(w, name, tabs)| selector::SessionView {
            id: &w.0,
            name: name.as_deref(),
            tabs: tabs
                .iter()
                .map(|(t, n)| (t.0.as_str(), n.as_deref()))
                .collect(),
        })
        .collect()
}

/// The instant an agent has held `status` since: the previous stamp while the status
/// HOLDS, else `now`. Extracted so the carry-forward is pinned by a test — the status map
/// is rebuilt from scratch twice a second, and resetting the stamp on every pass would peg
/// every readout at `0s` while looking perfectly correct.
fn status_since(
    prev: Option<AgentState>,
    status: agentstate::AgentStatus,
    now: std::time::Instant,
) -> std::time::Instant {
    match prev {
        Some(p) if p.status == status => p.since,
        _ => now,
    }
}

/// The change number for `status`: the previous one while the status HOLDS, else the next
/// value of the mux-wide `counter`. A pane seen for the first time (`prev = None`) is a change
/// too, so an agent that loses its classification and comes back is never mistaken for the
/// reading a `wait-agent --after-seq` already saw.
fn status_seq(
    prev: Option<AgentState>,
    pid: u32,
    status: agentstate::AgentStatus,
    counter: &mut u64,
) -> u64 {
    match prev {
        Some(p) if p.status == status && p.pid == pid => p.seq,
        _ => {
            *counter += 1;
            *counter
        }
    }
}

/// Coarse "how long in this status" for a sidebar row: empty under a minute, then whole
/// minutes, then hours. MINUTE granularity on purpose — a seconds-precise readout would
/// recompose the frame (and re-diff it to every attached client) once a second forever,
/// while the render loop's existing HH:MM rollover already guarantees a repaint at that
/// cadence. The cost is that a bucket can be shown up to a minute late, which is invisible
/// at this resolution.
fn fmt_elapsed(secs: u64) -> String {
    match secs {
        0..60 => String::new(),
        60..3600 => format!("{}m", secs / 60),
        _ => match ((secs / 3600), (secs % 3600) / 60) {
            (h, 0) => format!("{h}h"),
            (h, m) => format!("{h}h{m}m"),
        },
    }
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
        AgentItem, AgentRow, AgentState, CAT_GREEN, CAT_RED, CAT_TEXT, CAT_YELLOW, Menu,
        MenuAction, agent_items, agent_popup_text, agents_span_spaces, band_rows,
        build_command_line, clip_width, cwd_affinity, detect_alt_screen_transition,
        extract_selection, filter_env, fmt_elapsed, home_short_in, host_parts, list_window_start,
        menu_origin, merge_env, merge_labels, notify_title, reload_note, resume_line, resume_rank,
        sel_bounds, sel_cols, shell_quote, split_sidebar, status_seq, status_since,
        tab_display_title, tab_window, usage_should_roll, usage_threshold_color, window_max_start,
        window_start_var, with_pane_token, wrap_page,
    };
    use crate::model::{TerminalId, WorkspaceId};
    use crate::procinfo::{Kind, Label};
    use crate::term::{self, CellColor, CellSnap, Snapshot};
    use std::collections::HashMap;

    fn tid(s: &str) -> TerminalId {
        TerminalId::new(s)
    }

    fn label(text: &str, kind: Kind) -> Label {
        Label {
            text: text.to_string(),
            kind,
            pid: 1,
        }
    }

    #[test]
    fn a_failed_sweep_keeps_every_label_instead_of_blanking_them() {
        let prev = HashMap::from([
            (tid("t1"), label("claude", Kind::Agent)),
            (tid("t2"), label("zsh", Kind::Shell)),
        ]);
        // The sweep itself failed: the caller must be told "no change" so the tab
        // chips, sidebar agents list, agent statuses and the next autosave's restore
        // commands all keep working off the last-known labels.
        assert!(merge_labels(&prev, None).is_none());
    }

    #[test]
    fn a_pane_the_sweep_cannot_resolve_keeps_its_previous_label() {
        let prev = HashMap::from([
            (tid("t1"), label("claude", Kind::Agent)),
            (tid("t2"), label("zsh", Kind::Shell)),
        ]);
        let next = merge_labels(
            &prev,
            Some(vec![
                // t1 resolved to something new — the fresh answer wins.
                (tid("t1"), Some(label("nvim", Kind::Other))),
                // t2 missed this sweep (mid-exec / briefly a zombie) — retained, so
                // its chip does not flicker back to a bare index for one frame.
                (tid("t2"), None),
            ]),
        )
        .unwrap();
        assert_eq!(next[&tid("t1")].text, "nvim");
        assert_eq!(next[&tid("t2")].text, "zsh");
    }

    #[test]
    fn a_reaped_pane_is_dropped_rather_than_retained() {
        let prev = HashMap::from([
            (tid("t1"), label("zsh", Kind::Shell)),
            (tid("gone"), label("claude", Kind::Agent)),
        ]);
        // `gone` is absent from the sweep list (built from live panes), so retention
        // must NOT resurrect it — otherwise a closed agent pane would haunt the
        // sidebar forever.
        let next = merge_labels(&prev, Some(vec![(tid("t1"), None)])).unwrap();
        assert_eq!(next.len(), 1);
        assert!(!next.contains_key(&tid("gone")));
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
    fn reload_note_reports_path_warnings_and_restart_hint() {
        let path = std::path::Path::new("/home/u/.config/copad/mux.toml");
        // Clean reload: path + restart hint, no warnings block.
        let clean = reload_note(path, &[]);
        assert!(clean.starts_with("reloaded /home/u/.config/copad/mux.toml"));
        assert!(!clean.contains("warnings:"));
        assert!(clean.contains("comux server restart"));
        // With warnings: each is listed under a warnings header, hint still present.
        let warned = reload_note(
            path,
            &[
                "bad chord 'Ctrl-'".to_string(),
                "sidebar_width out of range".to_string(),
            ],
        );
        assert!(warned.contains("\n  warnings:"));
        assert!(warned.contains("bad chord 'Ctrl-'"));
        assert!(warned.contains("sidebar_width out of range"));
        assert!(warned.contains("comux server restart"));
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
    fn alt_screen_transition_fires_on_both_edges_once_each() {
        let mut prev = HashMap::new();
        let t = tid("t1");
        // First observation while already in alt screen: the seed default is primary (false),
        // so false→true reads as an ENTER edge and fires (a restored full-screen pane wants a
        // clean baseline anyway).
        assert!(detect_alt_screen_transition(
            &mut prev,
            &[(t.clone(), true)]
        ));
        // Still in alt screen: no edge.
        assert!(!detect_alt_screen_transition(
            &mut prev,
            &[(t.clone(), true)]
        ));
        // Leaves the alt screen: the true→false EXIT edge fires exactly once.
        assert!(detect_alt_screen_transition(
            &mut prev,
            &[(t.clone(), false)]
        ));
        // Staying on the primary screen: no repeat fire.
        assert!(!detect_alt_screen_transition(
            &mut prev,
            &[(t.clone(), false)]
        ));
        // Entering alt screen (false→true) fires the ENTER edge exactly once.
        assert!(detect_alt_screen_transition(
            &mut prev,
            &[(t.clone(), true)]
        ));
        // Staying in alt screen: no repeat fire.
        assert!(!detect_alt_screen_transition(&mut prev, &[(t, true)]));
    }

    #[test]
    fn alt_screen_transition_drops_hidden_panes_and_never_fires_spuriously() {
        let mut prev = HashMap::new();
        let (a, b) = (tid("a"), tid("b"));
        // Seed: pane a in alt screen (false→true enter edge fires once), pane b on primary.
        // Then let both settle so no edge is pending.
        detect_alt_screen_transition(&mut prev, &[(a.clone(), true), (b.clone(), false)]);
        assert!(!detect_alt_screen_transition(
            &mut prev,
            &[(a.clone(), true), (b.clone(), false)]
        ));
        // Pane a becomes hidden (only b visible): a is dropped from the store, and b staying on
        // the primary screen must NOT be read as a transition.
        assert!(!detect_alt_screen_transition(
            &mut prev,
            &[(b.clone(), false)]
        ));
        assert!(!prev.contains_key(&a));
        // Pane a reappears STILL on the primary screen (it left alt while hidden): because it was
        // dropped, it re-seeds as false (default) and does not spuriously fire — the tab switch
        // that revealed it recomposes the whole frame, so there is no residue to clear here.
        assert!(!detect_alt_screen_transition(
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
    fn every_pane_env_carries_exactly_one_fresh_token() {
        let a = with_pane_token(kv(&[("COPAD_MUX", "1")]));
        let b = with_pane_token(kv(&[("COPAD_MUX", "1")]));
        let tok = |e: &[(String, String)]| {
            let found: Vec<_> = e
                .iter()
                .filter(|(k, _)| k == term::PANE_TOKEN_ENV)
                .map(|(_, v)| v.clone())
                .collect();
            assert_eq!(found.len(), 1, "exactly one token per pane env");
            found[0].clone()
        };
        assert_ne!(tok(&a), tok(&b), "each pane gets its own identity");
        assert!(a.iter().any(|(k, v)| k == "COPAD_MUX" && v == "1"));
    }

    /// A server born inside a comux pane would otherwise hand its OWN `COPAD_MUX_PANE`
    /// down to every pane it spawns, so two panes would answer to the same token and a
    /// notification would jump to whichever matched first.
    #[test]
    fn an_inherited_token_is_replaced_not_kept() {
        let env = with_pane_token(kv(&[(term::PANE_TOKEN_ENV, "stale-from-parent")]));
        let tokens: Vec<_> = env
            .iter()
            .filter(|(k, _)| k == term::PANE_TOKEN_ENV)
            .collect();
        assert_eq!(tokens.len(), 1);
        assert_ne!(tokens[0].1, "stale-from-parent");
    }

    #[test]
    fn resume_rank_puts_verbatim_matches_ahead_of_scattered_ones() {
        assert_eq!(resume_rank("", "anything"), Some(0));
        assert_eq!(resume_rank("copad", "claude fix it ~/dev/copad"), Some(0));
        assert_eq!(resume_rank("COPAD", "~/dev/copad"), Some(0));
        // Every char in order but not contiguous — a match, but a worse one. This is the
        // coincidence that used to outrank real hits by sitting higher in recency order.
        assert_eq!(resume_rank("copad", "claude open a pad ~/dev/x"), Some(1));
        assert_eq!(resume_rank("copad", "claude 확인해봐 ~/dev/manifest"), None);
        assert_eq!(resume_rank("zzq", "claude ~/dev/copad"), None);
    }

    #[test]
    fn resume_line_fills_exactly_the_row_width() {
        use unicode_width::UnicodeWidthStr;
        let head = " ▪ claude ";
        // A CJK title is 2 columns per char and must not push the row past its box.
        let line = resume_line(head, &"세션".repeat(40), "~/dev/copad · 3h", 60);
        assert_eq!(line.width(), 60);
        // A short row is padded (so the selection highlight covers it) with the right-hand
        // detail flush to the edge.
        let line = resume_line(head, "fix it", "~/x · 2d", 60);
        assert_eq!(line.width(), 60);
        assert!(line.ends_with("~/x · 2d"));
        assert!(line.starts_with(" ▪ claude fix it"));
    }

    #[test]
    fn cwd_affinity_prefers_the_closest_space_in_either_direction() {
        use std::path::Path;
        let target = Path::new("/u/dev/copad");
        // Exact match beats everything.
        assert!(
            cwd_affinity(Path::new("/u/dev/copad"), target)
                > cwd_affinity(Path::new("/u/dev"), target)
        );
        // A space CONTAINING the conversation beats one nested inside it...
        assert!(
            cwd_affinity(Path::new("/u/dev"), target)
                > cwd_affinity(Path::new("/u/dev/copad/copad-mux"), target)
        );
        // ...and among containing spaces the deepest (closest) one wins.
        assert!(cwd_affinity(Path::new("/u/dev"), target) > cwd_affinity(Path::new("/u"), target));
        // Among nested spaces the shallowest (closest) one wins.
        assert!(
            cwd_affinity(Path::new("/u/dev/copad/copad-mux"), target)
                > cwd_affinity(Path::new("/u/dev/copad/copad-mux/src"), target)
        );
        // Unrelated trees never match, and a sibling with a shared prefix is not a parent.
        assert_eq!(cwd_affinity(Path::new("/other"), target), None);
        assert_eq!(cwd_affinity(Path::new("/u/dev/copad-mux"), target), None);
    }

    #[test]
    fn clip_width_counts_display_columns() {
        // Korean is 2 columns per char: 4 chars = 8 columns, so 6 columns keeps 2 + `…`.
        assert_eq!(clip_width("세션목록", 8), "세션목록");
        assert_eq!(clip_width("세션목록", 6), "세션…");
        assert_eq!(clip_width("abcdef", 6), "abcdef");
        assert_eq!(clip_width("abcdef", 4), "abc…");
    }

    #[test]
    fn home_short_only_rewrites_inside_home() {
        use std::path::Path;
        let home = Some(Path::new("/u/me"));
        assert_eq!(
            home_short_in(Path::new("/u/me/dev/copad"), home),
            "~/dev/copad"
        );
        assert_eq!(home_short_in(Path::new("/u/me"), home), "~");
        assert_eq!(home_short_in(Path::new("/opt/x"), home), "/opt/x");
        // No HOME at all → the path is shown as-is rather than mangled.
        assert_eq!(home_short_in(Path::new("/u/me/x"), None), "/u/me/x");
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
    /// The spaces half gets exactly what it needs when it fits, handing the whole
    /// remainder to the agents — the case the old fixed 50/50 split wasted (a 29-row
    /// strip with 2 sessions + 12 agents showed only 7 agents; now it shows 11).
    fn split_sidebar_gives_slack_to_the_crowded_half() {
        // 2 sessions (1 + 2*2 = 5 rows) vs 12 agents (1 + 24 = 25): 30 > 29, but the
        // spaces fit in their share, so they take 5 and agents get the other 24.
        assert_eq!(split_sidebar(29, 5, 25, 2, 2), 5);
        // Mirror: 12 sessions vs 2 agents — agents keep their 5, spaces absorb 24.
        assert_eq!(split_sidebar(29, 25, 5, 2, 2), 24);
        // Both overflow → even split (the old behaviour).
        assert_eq!(split_sidebar(29, 25, 25, 2, 2), 14);
        // Everything fits with room to spare: spaces take theirs, agents get the rest.
        assert_eq!(split_sidebar(49, 3, 41, 2, 2), 3);
    }

    #[test]
    /// Neither half may be starved below a header + one entry, and a strip too short to
    /// reason about keeps the old even split.
    fn split_sidebar_reserves_a_minimum_for_both_halves() {
        // 1 session (3 rows) but only 6 rows total: agents keep 3.
        assert_eq!(split_sidebar(6, 3, 99, 2, 2), 3);
        // A huge spaces list can't push the agents header off the bottom.
        assert_eq!(split_sidebar(10, 99, 3, 2, 2), 7);
        // Below the floor, fall back to the pre-adaptive `(h / 2).max(2)` verbatim.
        assert_eq!(split_sidebar(5, 3, 3, 2, 2), 2);
        assert_eq!(split_sidebar(1, 3, 3, 2, 2), 2);
    }

    #[test]
    /// Variable-height windowing keeps the anchor visible and the window full — the
    /// agents half mixes 1-row group headers with 2-row agent entries.
    fn window_start_var_centers_anchor_and_fills_the_window() {
        let h = [1u16, 2, 2, 1, 2, 2, 2]; // 2 groups + 5 agents = 12 rows
        assert_eq!(window_start_var(&h, 0, 12), 0); // everything fits
        assert_eq!(window_start_var(&h, 0, 6), 0); // anchor at the top
        // Anchor at the end → clamp so the window stays full (items 3..7 = 7 rows > 6,
        // so it starts at 4: rows 2+2+2 = 6).
        assert_eq!(window_start_var(&h, 6, 6), 4);
        // Anchor in the middle: at most half the budget (3 rows) goes above it, so the
        // window opens at item 2 (2 + 1 rows above the anchor) and the anchor still fits.
        assert_eq!(window_start_var(&h, 4, 6), 2);
        assert_eq!(window_start_var(&[], 0, 6), 0);
        assert_eq!(window_start_var(&h, 99, 0), 0);
    }

    #[test]
    /// A compact half's floor is a header + ONE row, so `split_sidebar` must not reserve
    /// the comfortable 3 rows for it — that would waste a row exactly where space is
    /// scarcest, which is the whole reason to go compact.
    fn split_sidebar_floors_follow_the_entry_height() {
        // Comfortable floors (3/3): 6 rows is the shortest strip that splits at all.
        assert_eq!(split_sidebar(6, 3, 99, 2, 2), 3);
        // Compact floors (2/2): the same strip gives the crowded half a row back.
        assert_eq!(split_sidebar(6, 2, 99, 1, 1), 2);
        assert_eq!(split_sidebar(10, 99, 2, 1, 1), 8);
        // Below the combined floor, the legacy fallback.
        assert_eq!(split_sidebar(3, 2, 2, 1, 1), 2);
    }

    #[test]
    /// Compact entries halve what the agents list asks for, which is the point.
    fn agent_item_rows_track_the_entry_height() {
        let two = [arow("copad", "a"), arow("bots", "b")];
        let items = agent_items(&two); // header, a, header, b
        assert_eq!(items.iter().map(|i| i.rows(2)).sum::<u16>(), 6);
        assert_eq!(items.iter().map(|i| i.rows(1)).sum::<u16>(), 4); // headers stay 1 row
    }

    #[test]
    /// The band's reserve for the list shrinks with the entry it has to protect.
    fn band_rows_reserve_tracks_the_entry_height() {
        // Comfortable: band + rule + a 2-row entry needs 4 rows.
        assert_eq!(band_rows(3, 5, 2), 0);
        assert_eq!(band_rows(4, 5, 2), 1);
        // Compact: the same reserve is one row cheaper.
        assert_eq!(band_rows(3, 5, 1), 1);
        assert_eq!(band_rows(2, 5, 1), 0);
    }

    #[test]
    /// The scroll ceiling: past it the window would trail blank rows instead of content.
    fn window_max_start_stops_where_the_tail_stops_filling() {
        let h = [1u16, 2, 2, 1, 2, 2, 2]; // 12 rows over 7 items
        assert_eq!(window_max_start(&h, 12), 0); // everything fits — nothing to scroll
        assert_eq!(window_max_start(&h, 99), 0);
        // 6 rows of tail = items 4,5,6 (2+2+2); item 3 would make 7.
        assert_eq!(window_max_start(&h, 6), 4);
        // 2 rows = the last item alone.
        assert_eq!(window_max_start(&h, 2), 6);
        // Not even one item fits — clamp to the end rather than underflow.
        assert_eq!(window_max_start(&h, 1), 7);
        assert_eq!(window_max_start(&[], 6), 0);
    }

    #[test]
    /// On a window barely big enough for one entry, the anchor still renders — its own
    /// group header must not push it out (the half's floor is header + one entry).
    fn window_start_var_never_spends_the_anchors_own_rows_on_a_header() {
        let h = [1u16, 2, 1, 2];
        // 2 rows: only the anchor entry fits, so the window opens ON it.
        assert_eq!(window_start_var(&h, 1, 2), 1);
        assert_eq!(window_start_var(&h, 3, 2), 3);
        // 3 rows: room for the header above it too.
        assert_eq!(window_start_var(&h, 3, 3), 2);
        // An anchor taller than the whole window can't fit either way — don't hang.
        assert_eq!(window_start_var(&h, 1, 1), 1);
    }

    fn arow(space: &str, title: &str) -> AgentRow {
        AgentRow {
            detail: None,
            title: title.to_string(),
            for_secs: 0,
            space: space.to_string(),
            space_id: WorkspaceId::new(space),
            tool: "claude".to_string(),
            status: "ready",
            term: tid(title),
        }
    }

    #[test]
    /// Minute buckets, so the composed frame changes at most once a minute per agent
    /// instead of once a second.
    fn fmt_elapsed_is_bucketed_to_minutes() {
        assert_eq!(fmt_elapsed(0), "");
        assert_eq!(fmt_elapsed(59), ""); // under a minute says nothing
        assert_eq!(fmt_elapsed(60), "1m");
        assert_eq!(fmt_elapsed(119), "1m");
        assert_eq!(fmt_elapsed(3599), "59m");
        assert_eq!(fmt_elapsed(3600), "1h");
        assert_eq!(fmt_elapsed(3660), "1h1m");
        assert_eq!(fmt_elapsed(86_400), "24h");
    }

    #[test]
    fn the_top_bar_omits_readings_it_does_not_have() {
        use crate::hostmetrics::HostMetrics;
        // Nothing readable at all: no parts. The bar is still DRAWN (the height must not
        // depend on the reading), it is simply empty.
        assert!(host_parts(&HostMetrics::default()).is_empty());

        let m = HostMetrics {
            cpu: Some(12.4),
            mem_used: Some(30),
            mem_total: Some(100),
            gpu: None,
            load1: Some(2.357),
        };
        let parts = host_parts(&m);
        let rendered: Vec<String> = parts
            .iter()
            .map(|p| format!("{}{}", p.label, p.value))
            .collect();
        assert_eq!(rendered, vec!["cpu 12%", "mem 30%", "load 2.36"]);
        // The GPU had no reading, so it contributes NOTHING — not `gpu 0%`, which would be a
        // claim about the machine, and not `gpu --`, which still spends a column on absence.
        assert!(!rendered.iter().any(|r| r.starts_with("gpu")));

        // A zero reading is a reading and must still appear.
        let idle = HostMetrics {
            gpu: Some(0.0),
            ..HostMetrics::default()
        };
        assert_eq!(host_parts(&idle).len(), 1);
        assert_eq!(host_parts(&idle)[0].value, "0%");
    }

    #[test]
    fn only_the_percentages_are_threshold_coloured() {
        use crate::hostmetrics::HostMetrics;
        let hot = HostMetrics {
            cpu: Some(95.0),
            load1: Some(99.0),
            ..HostMetrics::default()
        };
        let parts = host_parts(&hot);
        assert_eq!(parts[0].color, usage_threshold_color(95.0));
        // Load average is not a percentage: what counts as high depends on the core count,
        // so colouring it would imply a judgement we cannot make.
        assert_eq!(parts[1].color, CAT_TEXT);
    }

    #[test]
    fn an_agent_switcher_row_omits_what_it_has_no_reading_for() {
        assert_eq!(
            agent_popup_text(
                "codex",
                "working",
                260,
                Some("running: cargo test"),
                "copad · tab 2"
            ),
            "codex · working 4m — running: cargo test  (copad · tab 2)"
        );
        // No detail: the row must look exactly as it did before the poller existed, with no
        // placeholder standing in for information we do not have.
        assert_eq!(
            agent_popup_text("claude", "ready", 260, None, "copad · tab 1"),
            "claude · ready 4m  (copad · tab 1)"
        );
        assert_eq!(
            agent_popup_text("claude", "ready", 260, Some(""), "copad · tab 1"),
            "claude · ready 4m  (copad · tab 1)"
        );
        // Under a minute `fmt_elapsed` is empty, and the row must not grow a stray space or
        // a dangling separator because of it.
        assert_eq!(
            agent_popup_text("codex", "blocked", 3, None, "copad · tab 2"),
            "codex · blocked  (copad · tab 2)"
        );
        assert_eq!(
            agent_popup_text("codex", "blocked", 3, Some("thinking"), "p"),
            "codex · blocked — thinking  (p)"
        );
    }

    #[test]
    fn a_tab_title_falls_back_to_the_key_that_jumps_to_it() {
        assert_eq!(tab_display_title(Some("build"), 0), "build");
        assert_eq!(tab_display_title(Some("  build  "), 3), "build");
        // No name, an empty name and a whitespace-only name all mean "unnamed": show the
        // 1-based index, which is the `Ctrl-b <n>` that jumps there.
        assert_eq!(tab_display_title(None, 0), "tab 1");
        assert_eq!(tab_display_title(Some(""), 1), "tab 2");
        assert_eq!(tab_display_title(Some("   "), 8), "tab 9");
    }

    #[test]
    fn a_notification_title_names_the_tab_and_omits_what_it_does_not_know() {
        assert_eq!(
            notify_title("claude", "copad", "tab 2"),
            "claude · copad/tab 2"
        );
        assert_eq!(
            notify_title("codex", "copad", "review"),
            "codex · copad/review"
        );
        // A pane that could not be located must not yield a trailing or leading bare `/`,
        // which would read as a real but nameless tab.
        assert_eq!(notify_title("claude", "copad", ""), "claude · copad");
        assert_eq!(notify_title("claude", "", "tab 2"), "claude · tab 2");
        assert_eq!(notify_title("claude", "", ""), "claude");
        assert!(!notify_title("claude", "copad", "").contains('/'));
        assert!(!notify_title("claude", "", "").contains('·'));
    }

    #[test]
    /// The status map is rebuilt every refresh, so the stamp must be CARRIED while the
    /// status holds — resetting it would peg every `blocked Nm` readout at nothing.
    fn status_seq_moves_only_on_a_change_and_never_repeats() {
        use crate::agentstate::AgentStatus;
        let now = std::time::Instant::now();
        let st = |status, seq| AgentState {
            status,
            since: now,
            seq,
            pid: 100,
        };
        let mut c = 5;
        // Holding: same number, counter untouched.
        let held = status_seq(
            Some(st(AgentStatus::Working, 5)),
            100,
            AgentStatus::Working,
            &mut c,
        );
        assert_eq!((held, c), (5, 5));
        // A change takes the next number.
        let moved = status_seq(
            Some(st(AgentStatus::Working, 5)),
            100,
            AgentStatus::Ready,
            &mut c,
        );
        assert_eq!(moved, 6);
        // A REPLACEMENT agent (new pid) in the same status is a change too.
        let replaced = status_seq(
            Some(st(AgentStatus::Ready, 6)),
            200,
            AgentStatus::Ready,
            &mut c,
        );
        assert_eq!(replaced, 7);
        // A first sighting is a change as well, and numbers are never reused.
        assert_eq!(status_seq(None, 100, AgentStatus::Ready, &mut c), 8);
        assert_eq!(status_seq(None, 100, AgentStatus::Ready, &mut c), 9);
    }

    #[test]
    fn status_since_carries_forward_until_the_status_moves() {
        use crate::agentstate::AgentStatus;
        let then = std::time::Instant::now();
        let now = then + std::time::Duration::from_secs(300);
        let prev = AgentState {
            status: AgentStatus::Blocked,
            since: then,
            seq: 1,
            pid: 1,
        };
        // Same status → keep the original stamp (still "blocked for 5 minutes").
        assert_eq!(status_since(Some(prev), AgentStatus::Blocked, now), then);
        // Moved → restart the clock.
        assert_eq!(status_since(Some(prev), AgentStatus::Ready, now), now);
        // First sighting → start now.
        assert_eq!(status_since(None, AgentStatus::Working, now), now);
    }

    #[test]
    /// The band is a shortcut layered over the list; it may never squeeze the list below
    /// the one-entry floor stage 1 guarantees, so a short strip drops the band instead.
    fn band_rows_never_squeezes_the_list_below_one_entry() {
        // Roomy: a quarter of the region, bounded by how many are actually blocked.
        assert_eq!(band_rows(28, 12, 2), 7);
        assert_eq!(band_rows(28, 3, 2), 3);
        assert_eq!(band_rows(28, 0, 2), 0);
        // Tight: band + rule + one 2-row entry is exactly 4 rows.
        assert_eq!(band_rows(4, 5, 2), 1);
        // Too tight for all three — the list wins.
        assert_eq!(band_rows(3, 5, 2), 0);
        assert_eq!(band_rows(0, 5, 2), 0);
    }

    #[test]
    /// The same predicate gates BOTH the list's group headers and the attention band's
    /// `space/` prefix — a flat band of `tab 2` rows from four sessions is unreadable,
    /// but prefixing a single-space workspace is pure noise.
    fn agents_span_spaces_only_when_more_than_one_is_involved() {
        assert!(!agents_span_spaces(&[]));
        assert!(!agents_span_spaces(&[arow("copad", "a")]));
        assert!(!agents_span_spaces(&[
            arow("copad", "a"),
            arow("copad", "b")
        ]));
        assert!(agents_span_spaces(&[arow("copad", "a"), arow("bots", "b")]));
    }

    #[test]
    /// Group headers appear where the space changes — but only when the agents actually
    /// span more than one space, since a lone header just costs an agent its row.
    fn agent_items_groups_only_when_agents_span_spaces() {
        let one = [arow("copad", "a"), arow("copad", "b")];
        let items = agent_items(&one);
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| matches!(i, AgentItem::Agent(_))));

        let two = [arow("copad", "a"), arow("copad", "b"), arow("bots", "c")];
        let items = agent_items(&two);
        // header, a, b, header, c
        assert_eq!(items.len(), 5);
        assert!(matches!(&items[0], AgentItem::Group(s, _) if s == "copad"));
        assert!(items[1].is_agent(0));
        assert!(items[2].is_agent(1));
        assert!(matches!(&items[3], AgentItem::Group(s, _) if s == "bots"));
        assert!(items[4].is_agent(2));
        // Rows: 1 + 2 + 2 + 1 + 2
        assert_eq!(items.iter().map(|i| i.rows(2)).sum::<u16>(), 8);
    }

    #[test]
    fn list_window_start_centers_active_and_clamps() {
        assert_eq!(list_window_start(3, 1, 5), 0); // all fit
        assert_eq!(list_window_start(20, 0, 6), 0); // active at top
        assert_eq!(list_window_start(20, 19, 6), 14); // active at bottom → clamp
        assert_eq!(list_window_start(20, 10, 6), 7); // centered (10 - 3)
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

/// END-TO-END RENDER FIDELITY OF THE REAL COMPOSED SCREEN.
///
/// `term.rs`'s `render_repro` harness already drives compose → wire delta → client buffer →
/// real ratatui emit → a reference emulator, and it is production-accurate. What it has never
/// seen is the screen a user actually looks at: it composes ONE full-screen pane. The real
/// [`App::render_to`] lays a sidebar, a split pane grid, divider columns and a status bar into
/// a single buffer — and `ratatui::buffer::Buffer::diff`, which decides what gets shipped,
/// carries its wide-glyph suppression state (`to_skip` / `invalidated`) across a FLAT buffer.
/// That state therefore walks straight through every region boundary and every row boundary in
/// the composed screen, which is precisely where a withheld cell would go unnoticed.
///
/// The invariant asserted here: **what the terminal ends up showing after the incremental
/// relay must equal what it would show if the composed buffer were painted onto it from
/// scratch.** Anything less is residue — a stale cell no later delta will ever correct, which
/// is the failure mode this module exists to hunt.
#[cfg(test)]
mod app_render_fidelity {
    use super::*;
    use crate::term::{CellColor, Snapshot, snapshot_grid};
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config as AlacConfig, Term};
    use alacritty_terminal::vte::ansi::Processor;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::Rect as RRect;
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use std::rc::Rc;

    /// A `Write` sink shared with the caller, so the exact escape bytes ratatui emits can be
    /// read back and replayed into a reference emulator (`writer_mut` is feature-gated).
    #[derive(Clone)]
    struct Sink(Rc<RefCell<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn ref_term(cols: u16, rows: u16) -> (Term<VoidListener>, Processor) {
        let size = TermSize::new(cols as usize, rows as usize);
        (
            Term::new(AlacConfig::default(), &size, VoidListener),
            Processor::new(),
        )
    }

    fn sink_terminal(cols: u16, rows: u16) -> (Sink, Terminal<CrosstermBackend<Sink>>) {
        let sink = Sink(Rc::new(RefCell::new(Vec::new())));
        let term = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(RRect::new(0, 0, cols, rows)),
            },
        )
        .expect("terminal");
        (sink, term)
    }

    /// A comparable view of a screen: glyph + fg/bg + bold/reverse, with the trailing half of a
    /// wide glyph collapsed to a sentinel (its own colour is unobservable — the glyph in the
    /// preceding cell covers both columns and nothing is ever printed there).
    type Row = Vec<(String, CellColor, CellColor, bool, bool)>;
    fn norm(s: &Snapshot) -> Vec<Row> {
        s.cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| {
                        if c.spacer {
                            (
                                String::new(),
                                CellColor::Default,
                                CellColor::Default,
                                false,
                                false,
                            )
                        } else {
                            (c.sym.clone(), c.fg, c.bg, c.bold, c.reverse)
                        }
                    })
                    .collect()
            })
            .collect()
    }

    fn text(s: &Snapshot) -> Vec<String> {
        s.cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| if c.spacer { "" } else { c.sym.as_str() })
                    .collect::<String>()
            })
            .collect()
    }

    /// Blit `src` into a ratatui frame at the top-left, blanking the letterbox margin — byte
    /// for byte what `client::run_attached` does in its draw closure.
    fn blit(
        term: &mut Terminal<CrosstermBackend<Sink>>,
        prev: Option<&Buffer>,
        src: &Buffer,
        area: RRect,
        cursor: Option<(u16, u16)>,
    ) -> Buffer {
        let mut next = Buffer::empty(area);
        crate::client::blit_view(src, &mut next);
        crate::client::emit_view(term, prev, &next, cursor).expect("emit");
        next
    }

    /// GROUND TRUTH: what a terminal shows when `buf` is painted onto it from scratch.
    ///
    /// Deliberately another alacritty screen rather than a direct read of the buffer, so BOTH
    /// sides of the assertion pass through ONE width/style oracle. A direct read would compare
    /// ratatui's model against alacritty's and fail on their known disagreements, which are not
    /// the bug being hunted.
    fn painted_fresh(buf: &Buffer) -> Snapshot {
        let (cols, rows) = (buf.area.width, buf.area.height);
        let (mut t, mut p) = ref_term(cols, rows);
        let (sink, mut term) = sink_terminal(cols, rows);
        term.clear().expect("clear");
        blit(&mut term, None, buf, RRect::new(0, 0, cols, rows), None);
        let bytes = std::mem::take(&mut *sink.0.borrow_mut());
        p.advance(&mut t, &bytes);
        snapshot_grid(&t)
    }

    /// The production relay: `server::push_frames` → wire → `client::run_attached` → a
    /// reference emulator standing in for the user's terminal.
    struct Relay {
        area: RRect,
        sink: Sink,
        cterm: Terminal<CrosstermBackend<Sink>>,
        ref_t: Term<VoidListener>,
        ref_p: Processor,
        /// The server's `c.last` — the RAW composed buffer, never the client's normalized one.
        /// Keeping it independent is what exposes a baseline that disagrees with what the
        /// client actually holds.
        server_last: Buffer,
        /// The client's mirror of the screen.
        client: Buffer,
        /// What the client believes is ON the terminal — its emit baseline, exactly as
        /// `run_attached` keeps it now that the second diff is gone.
        painted: Buffer,
        first: bool,
    }

    impl Relay {
        fn new(cols: u16, rows: u16) -> Self {
            let area = RRect::new(0, 0, cols, rows);
            let (sink, cterm) = sink_terminal(cols, rows);
            let (ref_t, ref_p) = ref_term(cols, rows);
            Self {
                area,
                sink,
                cterm,
                ref_t,
                ref_p,
                server_last: Buffer::empty(area),
                client: Buffer::empty(area),
                painted: Buffer::empty(area),
                first: true,
            }
        }

        /// One server tick. Returns the composed buffer — the thing the client must end up
        /// showing.
        fn tick(&mut self, app: &App) -> Buffer {
            let mut buf = Buffer::empty(self.area);
            let cursor = app.render_to(&mut buf).map(|p| (p.x, p.y));
            assert_no_dim_bold(&buf);

            let full = self.first;
            if full {
                self.server_last = Buffer::empty(self.area);
                self.client = Buffer::empty(self.area);
            }
            // `push_frames`: ship the changed cells, then advance the baseline to the RAW
            // compose (NOT to what the client ends up holding — that asymmetry is the point).
            // Each cell goes through the real wire type AND a serde round-trip, so a style that
            // does not survive ndjson is caught here rather than on someone's screen.
            let changed = self.server_last.diff(&buf);
            for (x, y, cell) in &changed {
                let wire = crate::proto::WireCell::of(*x, *y, cell);
                let line = serde_json::to_string(&wire).expect("encode");
                let wire: crate::proto::WireCell = serde_json::from_str(&line).expect("decode");
                if let Some(c) = self.client.cell_mut(Position::new(wire.x, wire.y)) {
                    wire.apply_to(c);
                }
            }
            self.server_last = buf.clone();
            // `run_attached`: rebuild spacer structure, clear only on a `full` frame, emit.
            crate::client::fix_wide_spacers(&mut self.client);
            if full {
                self.cterm.clear().expect("clear");
                self.painted = Buffer::empty(self.area);
            }
            let src = self.client.clone();
            let prev = (!full).then(|| self.painted.clone());
            self.painted = blit(&mut self.cterm, prev.as_ref(), &src, self.area, cursor);
            let bytes = std::mem::take(&mut *self.sink.0.borrow_mut());
            self.ref_p.advance(&mut self.ref_t, &bytes);
            self.first = false;
            buf
        }

        /// Apply the silent full repaint (`FrameMsg::repaint`) — the self-heal. After it, the
        /// screen must match the composed buffer even if it had drifted.
        fn repaint(&mut self, cursor: Option<(u16, u16)>) {
            let src = self.client.clone();
            let view = (self.area.width, self.area.height);
            crate::client::repaint_all(&mut self.cterm, &src, view, cursor).expect("repaint");
            let bytes = std::mem::take(&mut *self.sink.0.borrow_mut());
            self.ref_p.advance(&mut self.ref_t, &bytes);
        }

        /// Describe one position across every layer, so a failure says WHICH layer lost it:
        /// the server's composed buffer, the baseline it believes the client holds, and the
        /// client's own mirror. If composed == client, the delta pipeline is fine and the emit
        /// path drifted; if they differ, the delta withheld a cell.
        fn layers_at(&self, composed: &Buffer, x: u16, y: u16) -> String {
            let show = |b: &Buffer| match b.cell(Position::new(x, y)) {
                Some(c) => format!(
                    "{:?} fg={:?} bg={:?} mods={:?} skip={}",
                    c.symbol(),
                    c.fg,
                    c.bg,
                    c.modifier,
                    c.skip
                ),
                None => "(out of area)".to_string(),
            };
            format!(
                "  composed     {}\n  server c.last {}\n  client buf   {}",
                show(composed),
                show(&self.server_last),
                show(&self.client)
            )
        }

        /// The cells LEADING UP TO `(x, y)` in each layer. Whether a position is a wide-glyph
        /// spacer is decided by its predecessors, so a divergence there is almost always
        /// explained by a neighbour rather than by the cell itself.
        fn neighbourhood(&self, composed: &Buffer, x: u16, y: u16) -> String {
            let lo = x.saturating_sub(3);
            let row = |b: &Buffer| {
                (lo..=x)
                    .map(|i| match b.cell(Position::new(i, y)) {
                        Some(c) => {
                            format!("{i}:{:?}{}", c.symbol(), if c.skip { "/skip" } else { "" })
                        }
                        None => format!("{i}:-"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            format!(
                "  row {y} composed  {}\n  row {y} client    {}",
                row(composed),
                row(&self.client)
            )
        }

        /// Write `ch` at `(x, y)` straight into the terminal, BEHIND comux's back — what an
        /// outer emulator with its own damage tracking, or a lossy link, effectively does.
        fn corrupt(&mut self, x: u16, y: u16, ch: char) {
            let seq = format!("\u{1b}[0m\u{1b}[{};{}H{ch}", y + 1, x + 1);
            let bytes = seq.into_bytes();
            self.ref_p.advance(&mut self.ref_t, &bytes);
        }

        /// The glyph the terminal is actually showing at `(x, y)`.
        fn screen_sym(&self, x: u16, y: u16) -> String {
            let snap = snapshot_grid(&self.ref_t);
            snap.cells[y as usize][x as usize].sym.clone()
        }

        fn assert_shows(&self, composed: &Buffer, ctx: &str) {
            let got = snapshot_grid(&self.ref_t);
            let want = painted_fresh(composed);
            let (gn, wn) = (norm(&got), norm(&want));
            if gn != wn {
                // Name the first differing CELL, with both styles — a style-only divergence
                // (a lost colour or attribute on the right glyph) is just as much residue as a
                // wrong glyph, and reporting rows alone cannot see it.
                let mut cell = "(none)".to_string();
                'find: for (y, (gr, wr)) in gn.iter().zip(wn.iter()).enumerate() {
                    for (x, (gc, wc)) in gr.iter().zip(wr.iter()).enumerate() {
                        if gc != wc {
                            cell = format!(
                                "cell ({x},{y})\n  screen got  {gc:?}\n  screen want {wc:?}\n{}\n{}",
                                self.layers_at(composed, x as u16, y as u16),
                                self.neighbourhood(composed, x as u16, y as u16)
                            );
                            break 'find;
                        }
                    }
                }
                let (g, w) = (text(&got), text(&want));
                let row = g
                    .iter()
                    .zip(w.iter())
                    .position(|(a, b)| a != b)
                    .map(|i| {
                        format!(
                            "\nfirst differing row {i}:\n  got  {:?}\n  want {:?}",
                            g[i], w[i]
                        )
                    })
                    .unwrap_or_else(|| "\n(glyphs all match — style only)".to_string());
                panic!(
                    "\n{ctx}\nthe relayed screen diverged from a fresh paint of the composed \
                     buffer — this is residue no later delta will correct.\n{cell}{row}\n"
                );
            }
        }
    }

    /// No composed cell may carry DIM and BOLD together — see [`resolve_dim_bold`]. Checked on
    /// every tick of the fuzz rather than once, because any widget could reintroduce it.
    fn assert_no_dim_bold(buf: &Buffer) {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(c) = buf.cell(Position::new(x, y)) {
                    assert!(
                        !(c.modifier.contains(Modifier::DIM)
                            && c.modifier.contains(Modifier::BOLD)),
                        "cell ({x},{y}) {:?} carries DIM and BOLD; ratatui's emit drops the BOLD \
                         permanently (see resolve_dim_bold)",
                        c.symbol()
                    );
                }
            }
        }
    }

    /// xorshift64*, so the fuzz is reproducible from its seed without a dev-dependency.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1) // 0 is the degenerate state
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() % n as u64) as usize
            }
        }
    }

    /// Byte sequences chosen to stress the seams: graphemes the two width models disagree about
    /// (VS16 / ZWJ), genuinely wide CJK, erase-in-line (the "should become blank" case the
    /// reported symptom is about), absolute cursor moves, colour/attribute runs, and
    /// alt-screen + synchronized-update toggles.
    fn vocabulary() -> Vec<String> {
        let mut v: Vec<String> = [
            "hello ",
            "x",
            "0123456789",
            " ",
            "|",
            "\r\n",
            "\n",
            "\r",
            "\x1b[2K",
            "\x1b[K",
            "\x1b[J",
            "\x1b[1J",
            "\x1b[H",
            "\x1b[3;5H",
            "\x1b[9;1H",
            "\x1b[1m",
            "\x1b[7m",
            "\x1b[2m",
            "\x1b[0m",
            "\x1b[38;2;203;166;247m",
            "\x1b[48;5;236m",
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[?2026h",
            "\x1b[?2026l",
            "\x1b[2J\x1b[H",
            "가나다",
            "漢字テスト",
            "❤\u{fe0f}",
            "👨\u{200d}👩\u{200d}👧",
            "🎉",
            "→",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Wide-glyph runs at BOTH column parities and long enough to wrap a pane: the way to
        // land a leading wide cell on a pane's last column, which is the position ratatui's
        // flat diff suppresses the FOLLOWING cell for — in a composed screen, the next region's
        // first column.
        for pad in 0..3 {
            v.push(format!("{}{}", "x".repeat(pad), "가".repeat(40)));
            v.push(format!("{}{}", "y".repeat(pad), "❤\u{fe0f}".repeat(40)));
            v.push(format!("{}{}", "z".repeat(pad), "漢".repeat(40)));
        }
        v
    }

    /// One screen shape to fuzz. Varying these is the whole point: the sidebar strip, the
    /// divider column and the status bar are what put a region boundary in the MIDDLE of a row.
    struct Shape {
        name: &'static str,
        cols: u16,
        rows: u16,
        splits: &'static [&'static str],
        sidebar: bool,
        top_bar: bool,
    }

    const SHAPES: &[Shape] = &[
        Shape {
            name: "single pane",
            cols: 100,
            rows: 30,
            splits: &[],
            sidebar: false,
            top_bar: false,
        },
        Shape {
            name: "sidebar + status bar",
            cols: 100,
            rows: 30,
            splits: &[],
            sidebar: true,
            top_bar: false,
        },
        Shape {
            name: "vertical split + sidebar",
            cols: 100,
            rows: 30,
            splits: &["right"],
            sidebar: true,
            top_bar: false,
        },
        Shape {
            name: "nested splits + sidebar + top bar",
            cols: 110,
            rows: 32,
            splits: &["right", "down"],
            sidebar: true,
            top_bar: true,
        },
        // An odd width matters: it decides whether a pane's content columns end on an even or
        // odd boundary, and therefore whether a wide glyph can straddle the pane's edge.
        Shape {
            name: "odd width, split, sidebar",
            cols: 101,
            rows: 29,
            splits: &["right"],
            sidebar: true,
            top_bar: false,
        },
    ];

    fn build(shape: &Shape) -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        cfg.sidebar = shape.sidebar;
        cfg.top_bar = shape.top_bar;
        // A quiet shell keeps the pane content mostly the harness's own, so a failure is easy
        // to read. Its prompt arriving anyway is harmless — the assertion compares the relay
        // against the buffer composed on the SAME tick, so extra churn only adds coverage.
        let env = vec![("PS1".to_string(), String::new())];
        let mut app = App::new(shape.cols, shape.rows, env, Vec::new(), cfg).expect("app");
        for dir in shape.splits {
            app.handle_control(&crate::control::Req::Split {
                dir: (*dir).to_string(),
                from: None,
                no_focus: false,
            });
        }
        app.resize(shape.cols, shape.rows);
        app
    }

    #[test]
    fn a_silent_repaint_heals_a_screen_the_delta_pipeline_cannot() {
        // What `reconcile_secs` buys, stated as a property. Corrupt the terminal behind comux's
        // back and no number of further deltas repairs it — every buffer in the chain still
        // agrees about that cell, so nothing is ever re-sent for it. That is precisely the
        // residue a user ends up clearing by hand with `Ctrl-b r`. One silent repaint fixes it,
        // and unlike `Ctrl-b r` it needs no `Clear` and therefore no visible flash.
        let app = build(&SHAPES[0]); // single pane: no clock or agent row to churn under us
        let mut relay = Relay::new(SHAPES[0].cols, SHAPES[0].rows);
        let composed = relay.tick(&app);
        relay.assert_shows(&composed, "baseline");

        // A cell the composition keeps blank, well away from the shell's prompt.
        let (x, y) = (40u16, 15u16);
        assert_eq!(
            composed.cell(Position::new(x, y)).map(|c| c.symbol()),
            Some(" "),
            "the test needs a blank cell at ({x},{y})"
        );

        relay.corrupt(x, y, 'Z');
        assert_eq!(relay.screen_sym(x, y), "Z", "the corruption landed");

        let composed = relay.tick(&app);
        assert_eq!(
            relay.screen_sym(x, y),
            "Z",
            "an incremental delta cannot see the corruption — this is the bug, not a flaw in \
             the test"
        );

        relay.repaint(None);
        assert_eq!(relay.screen_sym(x, y), " ", "the silent repaint healed it");
        relay.assert_shows(&composed, "after the silent repaint");
    }

    #[test]
    fn dim_and_bold_are_never_set_together() {
        // DIM wins, and the sweep runs over the WHOLE buffer — the reason it is a sweep rather
        // than a fix at each call site is that DIM is written by the pane loop, the sidebar's
        // group headers and the status bar's muted labels alike.
        let mut buf = Buffer::empty(RRect::new(0, 0, 4, 1));
        let styles = [
            Style::default().add_modifier(Modifier::DIM | Modifier::BOLD),
            Style::default().add_modifier(Modifier::BOLD),
            Style::default().add_modifier(Modifier::DIM),
            Style::default().add_modifier(Modifier::DIM | Modifier::BOLD | Modifier::REVERSED),
        ];
        for (x, st) in styles.iter().enumerate() {
            buf.cell_mut(Position::new(x as u16, 0))
                .expect("cell")
                .set_style(*st);
        }
        super::resolve_dim_bold(&mut buf);
        let m = |x: u16| buf.cell(Position::new(x, 0)).expect("cell").modifier;
        assert_eq!(m(0), Modifier::DIM, "DIM wins over BOLD");
        assert_eq!(m(1), Modifier::BOLD, "BOLD alone is untouched");
        assert_eq!(m(2), Modifier::DIM, "DIM alone is untouched");
        assert_eq!(
            m(3),
            Modifier::DIM | Modifier::REVERSED,
            "only BOLD is dropped — other attributes survive"
        );
    }

    #[test]
    fn the_composed_screen_relays_without_residue() {
        let vocab = vocabulary();
        // Enough seeds to be a real gate without dominating the suite; crank it up to hunt
        // (`COPAD_MUX_FUZZ_SEEDS=400 cargo test --release app_render_fidelity`). Seeds are
        // derived from a counter so raising the number only ADDS cases — it never renumbers the
        // ones a past failure was reported against.
        let seeds: u64 = std::env::var("COPAD_MUX_FUZZ_SEEDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12);
        for shape in SHAPES {
            for seed in (1..=seeds).map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15)) {
                let app = build(shape);
                let mut rng = Rng::new(seed);
                let mut relay = Relay::new(shape.cols, shape.rows);
                // SORTED: `panes` is a `HashMap`, so unsorted iteration would hand the same
                // seed's token streams to different panes on every run and a reported failure
                // could not be reproduced from its seed.
                let mut ids: Vec<TerminalId> = app.panes.keys().cloned().collect();
                ids.sort_by(|a, b| a.0.cmp(&b.0));
                let mut parsers: HashMap<TerminalId, Processor> =
                    ids.iter().map(|i| (i.clone(), Processor::new())).collect();

                for tick in 0..24 {
                    for id in &ids {
                        let n = 1 + rng.below(4);
                        for _ in 0..n {
                            let tok = vocab[rng.below(vocab.len())].clone();
                            let parser = parsers.get_mut(id).expect("parser");
                            app.panes
                                .get(id)
                                .expect("pane")
                                .feed_test_bytes(parser, tok.as_bytes());
                        }
                    }
                    let composed = relay.tick(&app);
                    relay.assert_shows(
                        &composed,
                        &format!(
                            "shape {:?} (cols={} rows={}), seed {seed:#x}, tick {tick}",
                            shape.name, shape.cols, shape.rows
                        ),
                    );
                }
            }
        }
    }
}

/// Creating panes somewhere the user is not looking must not move what they ARE looking at.
#[cfg(test)]
mod placement {
    use super::*;
    use crate::control::Req;

    fn app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    /// `(attached session, every session's (id, active tab, tab count))`.
    fn view(app: &App) -> (WorkspaceId, Vec<(WorkspaceId, TabId, usize)>) {
        let all = app
            .session_ids()
            .into_iter()
            .filter_map(|id| {
                let w = app.state.workspace(&id)?;
                Some((id, w.active_tab.clone(), w.tabs.len()))
            })
            .collect();
        (app.ws.clone(), all)
    }

    fn new_tab(session: Option<&str>, no_focus: bool) -> Req {
        Req::NewTab {
            session: session.map(str::to_string),
            from: None,
            cwd: None,
            name: Some("bg".into()),
            no_focus,
            command: Vec::new(),
            env: Vec::new(),
            keep: false,
            owner: None,
        }
    }

    #[test]
    fn a_tab_in_another_session_moves_nobody() {
        let mut app = app();
        assert!(
            app.handle_control(&Req::NewSession {
                name: Some("other".into()),
                cwd: None,
            })
            .ok
        );
        assert!(
            app.handle_control(&Req::SelectSession {
                index: Some(0),
                target: None,
            })
            .ok
        );
        let (ws0, before) = view(&app);

        let r = app.handle_control(&new_tab(Some("name:other"), false));
        assert!(r.ok, "{:?}", r.error);
        let (ws1, after) = view(&app);
        assert_eq!(ws0, ws1, "the user's session did not change");
        assert_eq!(before[1].1, after[1].1, "nor did that session's active tab");
        assert_eq!(after[1].2, before[1].2 + 1, "but it has one more tab");
        let tab = r.tab.expect("the created tab is named in the response");
        assert_eq!(tab.name, "bg");
        assert_eq!(r.session.unwrap().id, after[1].0.to_string());

        // Split the new background pane: still nothing visible moves.
        let token = r.pane.expect("token");
        let s = app.handle_control(&Req::Split {
            dir: "right".into(),
            from: Some(token),
            no_focus: false,
        });
        assert!(s.ok, "{:?}", s.error);
        assert_eq!(s.tab.unwrap().id, tab.id, "the split landed in that tab");
        assert_eq!(view(&app), (ws1, after), "a background split moves nothing");
    }

    #[test]
    fn no_focus_keeps_the_active_tab() {
        let mut app = app();
        let (ws, before) = view(&app);
        assert!(app.handle_control(&new_tab(None, true)).ok);
        let (ws_after, after) = view(&app);
        assert_eq!(ws, ws_after);
        assert_eq!(before[0].1, after[0].1);
        assert_eq!(after[0].2, 2);
        // The legacy shape still selects the new tab.
        assert!(app.handle_control(&new_tab(None, false)).ok);
        assert_ne!(view(&app).1[0].1, before[0].1);
    }

    #[test]
    fn a_spawn_runs_in_the_background_and_a_bad_one_creates_nothing() {
        let mut app = app();
        let (ws, before) = view(&app);
        let spawn = |command: &[&str], env: &[&str]| Req::NewTab {
            session: None,
            from: None,
            cwd: None,
            name: None,
            no_focus: true,
            command: command.iter().map(|s| s.to_string()).collect(),
            env: env.iter().map(|s| s.to_string()).collect(),
            keep: false,
            owner: None,
        };
        let r = app.handle_control(&spawn(&["sleep", "30"], &["FOO=bar"]));
        assert!(r.ok, "{:?}", r.error);
        assert!(r.pane.is_some_and(|p| !p.is_empty()));
        let (ws_after, after) = view(&app);
        assert_eq!(
            (ws.clone(), before[0].1.clone()),
            (ws_after, after[0].1.clone())
        );
        assert_eq!(after[0].2, 2);

        for bad in [
            spawn(&["true"], &["COPAD_MUX_PANE=x"]),
            spawn(&["true"], &["1X=y"]),
            spawn(&["a\0b"], &[]),
        ] {
            assert_eq!(app.handle_control(&bad).code.as_deref(), Some("invalid"));
        }
        assert_eq!(view(&app).1[0].2, 2, "a refused spawn creates no tab");
    }

    #[test]
    fn spawn_argv_hands_the_argv_to_a_login_shell_verbatim() {
        let argv: Vec<String> = ["echo", "a b", "$HOME", "it's"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            spawn_argv(Some("/bin/zsh"), &argv, false),
            [
                "/bin/zsh",
                "-l",
                "-i",
                "-c",
                r#"exec "$@""#,
                "comux-spawn",
                "echo",
                "a b",
                "$HOME",
                "it's"
            ]
        );
        let keep = spawn_argv(Some("/usr/bin/fish"), &argv, true);
        assert_eq!(
            keep[..3],
            ["/bin/sh", "-l", "-c"],
            "a non-POSIX shell is not the wrapper"
        );
        assert!(keep[3].starts_with(r#""$@"; exec"#), "{keep:?}");
        assert_eq!(spawn_argv(None, &argv, false)[0], "/bin/sh");
    }

    #[test]
    fn an_unknown_session_is_not_found() {
        let mut app = app();
        let r = app.handle_control(&new_tab(Some("nope"), false));
        assert_eq!(r.code.as_deref(), Some("not_found"));
    }
}

/// A-2: inside a pane, an index counts in the CALLER's tab, so `list` and every index verb
/// agree for an agent in a tab the user is not looking at.
#[cfg(test)]
mod caller_index {
    use super::*;
    use crate::control::Req;

    fn app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    /// A background tab holding two panes; returns the first pane's token (the "agent").
    fn background_pair(app: &mut App) -> (String, String) {
        let r = app.handle_control(&Req::NewTab {
            session: None,
            from: None,
            cwd: None,
            name: Some("bg".into()),
            no_focus: true,
            command: Vec::new(),
            env: Vec::new(),
            keep: false,
            owner: None,
        });
        assert!(r.ok, "{:?}", r.error);
        let agent = r.pane.unwrap();
        let s = app.handle_control(&Req::Split {
            dir: "right".into(),
            from: Some(agent.clone()),
            no_focus: true,
        });
        assert!(s.ok, "{:?}", s.error);
        (agent, s.pane.unwrap())
    }

    fn tokens(app: &mut App, caller: Option<&str>) -> Vec<String> {
        let r = app.handle_control(&Req::List {
            caller: caller.map(str::to_string),
        });
        assert!(r.ok, "{:?}", r.error);
        r.panes.unwrap().into_iter().map(|p| p.token).collect()
    }

    fn view(app: &App) -> (WorkspaceId, TabId) {
        (
            app.ws.clone(),
            app.state.workspace(&app.ws).unwrap().active_tab.clone(),
        )
    }

    #[test]
    fn list_and_the_index_verbs_count_in_the_callers_tab() {
        let mut app = app();
        let before = view(&app);
        let (agent, sibling) = background_pair(&mut app);

        assert_eq!(
            tokens(&mut app, Some(&agent)),
            vec![agent.clone(), sibling.clone()]
        );
        assert_eq!(
            tokens(&mut app, None).len(),
            1,
            "no caller = the tab on screen"
        );

        // Index 1 is the sibling — for capture as for list.
        let c = app.handle_control(&Req::CapturePane {
            target: None,
            index: Some(1),
            lines: None,
            caller: Some(agent.clone()),
        });
        assert!(c.ok, "{:?}", c.error);
        assert_eq!(c.pane.as_deref(), Some(sibling.as_str()));
        let s = app.handle_control(&Req::SendKeys {
            target: None,
            index: Some(1),
            text: String::new(),
            paste: false,
            enter: false,
            caller: Some(agent.clone()),
        });
        assert!(s.ok, "{:?}", s.error);

        // Focus moves within the background tab only.
        let f = app.handle_control(&Req::Focus {
            index: 0,
            caller: Some(agent.clone()),
        });
        assert!(f.ok, "{:?}", f.error);
        let r = app.handle_control(&Req::List {
            caller: Some(agent.clone()),
        });
        assert_eq!(r.focused, Some(0), "the caller's tab now focuses index 0");
        assert_eq!(view(&app), before, "the view did not move");

        let rz = app.handle_control(&Req::ResizePane {
            index: 0,
            dir: "right".into(),
            caller: Some(agent.clone()),
        });
        assert!(rz.ok, "{:?}", rz.error);

        // Close the sibling by index; the on-screen tab is untouched.
        let x = app.handle_control(&Req::Close {
            index: 1,
            caller: Some(agent.clone()),
        });
        assert!(x.ok, "{:?}", x.error);
        assert_eq!(tokens(&mut app, Some(&agent)), vec![agent.clone()]);
        assert_eq!(tokens(&mut app, None).len(), 1);
        assert_eq!(view(&app), before);
    }

    #[test]
    fn an_out_of_range_index_or_unknown_caller_is_not_found() {
        let mut app = app();
        let (agent, _) = background_pair(&mut app);
        let r = app.handle_control(&Req::Focus {
            index: 5,
            caller: Some(agent),
        });
        assert_eq!(r.code.as_deref(), Some("not_found"));
        let r = app.handle_control(&Req::List {
            caller: Some("nope-1".into()),
        });
        assert_eq!(r.code.as_deref(), Some("not_found"));
    }
}

/// #8: tabs remember the pane that made them; children report into that pane's inbox.
#[cfg(test)]
mod owner_inbox {
    use super::*;
    use crate::control::Req;
    use crate::persist::{PLayout, PSession, PTab, PersistState};

    fn app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn focused_token(app: &App) -> String {
        let t = app.focused_terminal().unwrap();
        app.token_of(&t).unwrap()
    }

    /// A background tab owned by `owner`; returns its pane token and tab id.
    fn owned_tab(app: &mut App, owner: &str) -> (String, String) {
        let r = app.handle_control(&Req::NewTab {
            session: None,
            from: None,
            cwd: None,
            name: Some("child".into()),
            no_focus: true,
            command: Vec::new(),
            env: Vec::new(),
            keep: false,
            owner: Some(owner.to_string()),
        });
        assert!(r.ok, "{:?}", r.error);
        (r.pane.unwrap(), r.tab.unwrap().id)
    }

    fn deliver(to: &str, from: Option<&str>, body: &str) -> Req {
        Req::Deliver {
            to: to.into(),
            from: from.map(str::to_string),
            kind: "done".into(),
            body: body.into(),
            result: None,
        }
    }

    fn inbox(app: &mut App, pane: &str, ack: Option<u64>) -> Vec<control::InboxMsg> {
        let r = app.handle_control(&Req::Inbox {
            pane: pane.into(),
            ack,
        });
        assert!(r.ok, "{:?}", r.error);
        r.messages.unwrap()
    }

    #[test]
    fn an_owned_tab_lists_its_owner_and_filters_by_it() {
        let mut app = app();
        let parent = focused_token(&app);
        let (_, tab) = owned_tab(&mut app, &parent);
        let r = app.handle_control(&Req::ListTree {
            session: None,
            owner: Some(parent.clone()),
        });
        let tree = r.tree.unwrap();
        let tabs: Vec<_> = tree.iter().flat_map(|s| &s.tabs).collect();
        assert_eq!(tabs.len(), 1, "only the owned tab");
        assert_eq!(tabs[0].id, tab);
        assert_eq!(tabs[0].owner.as_deref(), Some(parent.as_str()));

        // An unknown owner creates nothing.
        let before = app.state.workspace(&app.ws).unwrap().tabs.len();
        let r = app.handle_control(&Req::NewTab {
            session: None,
            from: None,
            cwd: None,
            name: None,
            no_focus: true,
            command: Vec::new(),
            env: Vec::new(),
            keep: false,
            owner: Some("nope-1".into()),
        });
        assert_eq!(r.code.as_deref(), Some("not_found"));
        assert_eq!(app.state.workspace(&app.ws).unwrap().tabs.len(), before);
    }

    #[test]
    fn a_child_reports_to_its_owner_and_the_message_stays_until_acknowledged() {
        let mut app = app();
        let parent = focused_token(&app);
        let (child, _) = owned_tab(&mut app, &parent);
        let r = app.handle_control(&deliver("owner", Some(&child), "tests passed"));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.pane.as_deref(), Some(parent.as_str()));

        let first = inbox(&mut app, &parent, None);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].from.as_deref(), Some(child.as_str()));
        assert_eq!(first[0].body, "tests passed");
        assert_eq!(
            inbox(&mut app, &parent, None),
            first,
            "reading does not consume"
        );
        assert!(inbox(&mut app, &parent, Some(first[0].seq)).is_empty());

        // A tab without an owner has nobody to report to.
        let r = app.handle_control(&deliver("owner", Some(&parent), "x"));
        assert_eq!(r.code.as_deref(), Some("not_found"));
        // Bad input is refused before anything is queued.
        let big = "x".repeat(INBOX_BODY_MAX + 1);
        assert_eq!(
            app.handle_control(&deliver(&parent, None, &big))
                .code
                .as_deref(),
            Some("invalid")
        );
        let rel = Req::Deliver {
            to: parent.clone(),
            from: None,
            kind: "done".into(),
            body: String::new(),
            result: Some("out.txt".into()),
        };
        assert_eq!(app.handle_control(&rel).code.as_deref(), Some("invalid"));
        assert!(inbox(&mut app, &parent, None).is_empty());
    }

    #[test]
    fn a_full_inbox_drops_its_oldest_and_says_so() {
        let mut app = app();
        let parent = focused_token(&app);
        for i in 0..=INBOX_CAP {
            assert!(
                app.handle_control(&deliver(&parent, None, &i.to_string()))
                    .ok
            );
        }
        let r = app.handle_control(&Req::Inbox {
            pane: parent.clone(),
            ack: None,
        });
        let msgs = r.messages.unwrap();
        assert_eq!(msgs.len(), INBOX_CAP);
        assert_eq!(msgs[0].body, "1", "the oldest went");
        assert_eq!(r.dropped, Some(1));
    }

    #[test]
    fn a_huge_inbox_is_read_in_batches_that_fit_a_response() {
        let mut app = app();
        let parent = focused_token(&app);
        // Backslashes double when JSON-escaped: the worst case for the response size.
        let body = "\\".repeat(INBOX_BODY_MAX);
        for _ in 0..INBOX_CAP {
            assert!(app.handle_control(&deliver(&parent, None, &body)).ok);
        }
        let mut seen = 0;
        let mut ack = None;
        loop {
            let r = app.handle_control(&Req::Inbox {
                pane: parent.clone(),
                ack,
            });
            let line = serde_json::to_string(&r).unwrap();
            assert!(line.len() < 2 * INBOX_BATCH_BYTES, "{} bytes", line.len());
            let msgs = r.messages.unwrap();
            if msgs.is_empty() {
                break;
            }
            seen += msgs.len();
            ack = msgs.last().map(|m| m.seq);
            assert_eq!(r.truncated == Some(true), seen < INBOX_CAP);
        }
        assert_eq!(seen, INBOX_CAP, "every message arrives, batch by batch");
    }

    #[test]
    fn a_dry_run_keeps_a_sessions_last_tab_like_the_close_does() {
        let mut app = app();
        let ws = app.ws.clone();
        let r = app.handle_control(&Req::NewSession {
            name: Some("kids".into()),
            cwd: None,
        });
        assert!(r.ok, "{:?}", r.error);
        let first = r.pane.unwrap();
        assert!(
            app.handle_control(&Req::SelectSession {
                index: Some(0),
                target: None,
            })
            .ok
        );
        assert_eq!(app.ws, ws);
        let parent = focused_token(&app);
        // Own the new session's ONLY tab, plus one more there.
        let kid_tab = app
            .resolve_pane_token(&first)
            .and_then(|t| app.locate_terminal(&t))
            .map(|(_, tab, _)| tab)
            .unwrap();
        let parent_term = app.resolve_pane_token(&parent).unwrap();
        app.tab_owner.insert(kid_tab, parent_term);
        let r = app.handle_control(&Req::NewTab {
            session: Some("name:kids".into()),
            from: None,
            cwd: None,
            name: None,
            no_focus: true,
            command: Vec::new(),
            env: Vec::new(),
            keep: false,
            owner: Some(parent.clone()),
        });
        assert!(r.ok, "{:?}", r.error);
        let close = |dry_run| Req::CloseOwned {
            owner: parent.clone(),
            caller: None,
            dry_run,
        };
        let preview = app.handle_control(&close(true));
        let real = app.handle_control(&close(false));
        assert_eq!(preview.closed.as_ref().map(Vec::len), Some(1));
        assert_eq!(preview.skipped.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            preview.closed.unwrap()[0].id,
            real.closed.unwrap()[0].id,
            "the preview closes what the close does"
        );
        assert_eq!(real.skipped.map(|s| s.len()), Some(1));
    }

    #[test]
    fn close_owned_never_closes_the_caller_and_dry_run_closes_nothing() {
        let mut app = app();
        let parent = focused_token(&app);
        let (child, tab) = owned_tab(&mut app, &parent);
        let tabs = |app: &App| app.state.workspace(&app.ws).unwrap().tabs.len();
        let close = |dry_run, caller: Option<&str>| Req::CloseOwned {
            owner: parent.clone(),
            caller: caller.map(str::to_string),
            dry_run,
        };
        let r = app.handle_control(&close(true, None));
        assert_eq!(r.closed.unwrap()[0].id, tab);
        assert_eq!(tabs(&app), 2, "a dry run closes nothing");
        let r = app.handle_control(&close(false, Some(&child)));
        assert_eq!(r.code.as_deref(), Some("refused"));
        assert_eq!(tabs(&app), 2);
        let r = app.handle_control(&close(false, Some(&parent)));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.closed.unwrap().len(), 1);
        assert_eq!(tabs(&app), 1);
    }

    #[test]
    fn the_owner_survives_a_snapshot_round_trip() {
        let mut app = app();
        let parent = focused_token(&app);
        let _ = owned_tab(&mut app, &parent);
        let snap = app.snapshot();
        let tabs = &snap.sessions[0].tabs;
        assert_eq!(tabs[1].owner, Some(0));
        assert!(matches!(
            tabs[0].layout,
            PLayout::Leaf {
                pane_ref: Some(0),
                ..
            }
        ));
    }

    fn leaf(pane_ref: Option<u32>) -> PLayout {
        PLayout::Leaf {
            cwd: None,
            command: None,
            pane_ref,
        }
    }

    #[test]
    fn restore_reattaches_owners_across_sessions_and_pruned_tabs() {
        // A tab the restore rejects (too many leaves) sits BEFORE the owned one: its owner
        // must not shift onto a neighbour.
        let mut too_big = leaf(None);
        for _ in 0..persist::MAX_LEAVES_PER_TAB {
            too_big = PLayout::Branch {
                dir: Dir::Right,
                ratio: 0.5,
                first: Box::new(leaf(None)),
                second: Box::new(too_big),
            };
        }
        let snap = PersistState {
            version: persist::SCHEMA_VERSION,
            saved_at: 0,
            active_session: 0,
            sessions: vec![
                PSession {
                    name: None,
                    active_tab: 0,
                    tabs: vec![
                        PTab {
                            name: Some("parent".into()),
                            layout: leaf(Some(7)),
                            owner: None,
                        },
                        PTab {
                            name: Some("pruned".into()),
                            layout: too_big,
                            owner: Some(7),
                        },
                        PTab {
                            name: Some("child".into()),
                            layout: leaf(None),
                            owner: Some(7),
                        },
                        PTab {
                            name: Some("dangling".into()),
                            layout: leaf(None),
                            owner: Some(99),
                        },
                    ],
                },
                PSession {
                    name: Some("other".into()),
                    active_tab: 0,
                    tabs: vec![PTab {
                        name: Some("far child".into()),
                        layout: leaf(None),
                        owner: Some(7),
                    }],
                },
            ],
        };
        let mut state = State::new();
        let mut panes = HashMap::new();
        let (_, _, owners) =
            restore_sessions(&mut state, &mut panes, &snap, 80, 24, &[]).expect("restored");
        let named = |name: &str| {
            state
                .workspace_ids()
                .into_iter()
                .filter_map(|w| state.workspace(&w).cloned())
                .flat_map(|w| w.tabs.clone())
                .find(|t| t.name.as_deref() == Some(name))
                .map(|t| {
                    (
                        t.id.clone(),
                        t.layout.terminal_of(&t.focused).cloned().unwrap(),
                    )
                })
        };
        let (_, parent_term) = named("parent").unwrap();
        assert!(named("pruned").is_none());
        let (child, _) = named("child").unwrap();
        let (far, _) = named("far child").unwrap();
        let (dangling, _) = named("dangling").unwrap();
        assert_eq!(owners.get(&child), Some(&parent_term));
        assert_eq!(owners.get(&far), Some(&parent_term), "across sessions");
        assert!(
            !owners.contains_key(&dangling),
            "a ref to no leaf owns nothing"
        );
        assert_eq!(owners.len(), 2);
    }
}

#[cfg(test)]
mod nav {
    use super::*;
    use crate::control::{NavDir, Req};

    fn app_with(sort_by: SortBy) -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        cfg.sort_by = sort_by;
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn new_session(app: &mut App, name: &str) {
        let r = app.handle_control(&Req::NewSession {
            name: Some(name.into()),
            cwd: None,
        });
        assert!(r.ok, "{:?}", r.error);
    }

    fn select(app: &mut App, name: &str) {
        let r = app.handle_control(&Req::SelectSession {
            index: None,
            target: Some(format!("name:{name}")),
        });
        assert!(r.ok, "{:?}", r.error);
    }

    fn nav(app: &mut App, dir: NavDir) -> bool {
        app.handle_control(&Req::Nav { dir }).ok
    }

    /// The attached session's display name.
    fn here(app: &App) -> String {
        app.session_display_name(&app.ws)
    }

    fn key(app: &mut App, prefix: &mut bool, code: KeyCode, mods: KeyModifiers) {
        app.feed_key(KeyEvent::new(code, mods), prefix);
    }

    /// `local`, then `a`, `b`, `c`, each attached as it is made.
    fn four_sessions(sort_by: SortBy) -> App {
        let mut app = app_with(sort_by);
        for n in ["a", "b", "c"] {
            new_session(&mut app, n);
        }
        app
    }

    #[test]
    fn last_toggles_and_back_forward_walk_the_trail() {
        let mut app = four_sessions(SortBy::Created);
        assert_eq!(here(&app), "c");
        assert!(nav(&mut app, NavDir::Last));
        assert_eq!(here(&app), "b");
        assert!(nav(&mut app, NavDir::Last));
        assert_eq!(here(&app), "c", "last again comes back");
        assert!(nav(&mut app, NavDir::Back));
        assert_eq!(here(&app), "b");
        assert!(nav(&mut app, NavDir::Back));
        assert_eq!(here(&app), "a");
        assert!(nav(&mut app, NavDir::Forward));
        assert_eq!(here(&app), "b");
    }

    #[test]
    fn nowhere_to_go_is_an_error_not_a_silent_ok() {
        let mut app = app_with(SortBy::Created);
        let r = app.handle_control(&Req::Nav { dir: NavDir::Back });
        assert!(!r.ok);
        assert_eq!(r.code.as_deref(), Some("not_found"));
    }

    #[test]
    fn a_tab_change_is_a_jump_too() {
        let mut app = app_with(SortBy::Created);
        let t0 = app.active_tab_id().unwrap();
        app.feed_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &mut false,
        );
        let mut prefix = false;
        key(
            &mut app,
            &mut prefix,
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        );
        key(
            &mut app,
            &mut prefix,
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        ); // new tab
        let t1 = app.active_tab_id().unwrap();
        assert_ne!(t0, t1);
        key(
            &mut app,
            &mut prefix,
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        );
        key(
            &mut app,
            &mut prefix,
            KeyCode::Char(';'),
            KeyModifiers::NONE,
        );
        assert_eq!(
            app.active_tab_id().unwrap(),
            t0,
            "Ctrl-b ; returned to the first tab"
        );
        key(
            &mut app,
            &mut prefix,
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        );
        key(&mut app, &mut prefix, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.active_tab_id().unwrap(), t1, "Ctrl-b Tab toggles back");
    }

    #[test]
    fn a_session_made_from_the_prompt_is_recorded() {
        // The prompt commit returns early from the key handler — exactly the path a
        // bottom-of-function hook would miss.
        let mut app = app_with(SortBy::Created);
        let mut prefix = false;
        key(
            &mut app,
            &mut prefix,
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        );
        key(
            &mut app,
            &mut prefix,
            KeyCode::Char('C'),
            KeyModifiers::SHIFT,
        );
        for c in "work".chars() {
            key(&mut app, &mut prefix, KeyCode::Char(c), KeyModifiers::NONE);
        }
        key(&mut app, &mut prefix, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(here(&app), "work");
        assert!(nav(&mut app, NavDir::Last));
        assert_eq!(here(&app), "local");
    }

    #[test]
    fn killing_the_attached_session_lands_on_the_previous_one() {
        let mut app = four_sessions(SortBy::Created);
        select(&mut app, "a");
        select(&mut app, "c");
        let r = app.handle_control(&Req::KillSession {
            index: None,
            target: Some("name:c".into()),
        });
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            here(&app),
            "a",
            "the most recent survivor, not the first-created"
        );
        // History pruned the dead session, and the fallback is not left atop `back`: `back`
        // MOVES (to the session visited before `a`), it does not report success in place.
        assert!(nav(&mut app, NavDir::Back));
        assert_ne!(here(&app), "a");
        assert_ne!(here(&app), "c");
    }

    #[test]
    fn mru_lists_every_session_even_the_never_visited() {
        let mut app = app_with(SortBy::Created);
        new_session(&mut app, "a");
        new_session(&mut app, "b");
        // A session made in the background never becomes attached.
        let r = app.handle_control(&Req::NewSession {
            name: Some("bg".into()),
            cwd: None,
        });
        assert!(r.ok);
        select(&mut app, "a");
        let names: Vec<String> = app
            .mru_sessions()
            .iter()
            .map(|w| app.session_display_name(w))
            .collect();
        assert_eq!(names[0], "a");
        assert_eq!(names.len(), 4, "{names:?}");
    }

    /// The selected switcher row's session.
    fn preselected(app: &App) -> String {
        let p = app.popup.as_ref().expect("open");
        match &app.popup_items(p.tab, &p.filter)[p.sel].target {
            PopupTarget::Session(w) => app.session_display_name(w),
            PopupTarget::Agent(_) => panic!("agent row"),
        }
    }

    #[test]
    fn the_switcher_opens_on_the_previous_session_under_every_order() {
        for sort in [SortBy::Created, SortBy::Alphabetical, SortBy::Recent] {
            let mut app = four_sessions(sort);
            select(&mut app, "b");
            select(&mut app, "a");
            app.open_popup();
            assert_eq!(preselected(&app), "b", "{sort:?}");
            // Enter switches there: Ctrl-f Enter is the Alt-Tab toggle.
            let mut prefix = false;
            key(&mut app, &mut prefix, KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(here(&app), "b", "{sort:?}");
        }
    }

    #[test]
    fn typing_a_filter_selects_the_best_match_and_clearing_it_restores_the_default() {
        let mut app = four_sessions(SortBy::Created);
        select(&mut app, "a");
        app.open_popup();
        app.popup_type('c');
        assert_eq!(app.popup.as_ref().unwrap().sel, 0);
        app.popup_backspace();
        assert_eq!(preselected(&app), "c");
    }

    #[test]
    fn going_back_to_a_tab_whose_pane_died_lands_on_its_survivor() {
        let mut app = app_with(SortBy::Created);
        let r = app.handle_control(&Req::Split {
            dir: "right".into(),
            from: None,
            no_focus: false,
        });
        assert!(r.ok, "{:?}", r.error);
        let split = app.focused_terminal().unwrap();
        new_session(&mut app, "other");
        // Back in `local`, the split pane is closed while we are away.
        assert!(
            app.handle_control(&Req::SelectSession {
                index: Some(0),
                target: None,
            })
            .ok
        );
        let r = app.handle_control(&Req::Close {
            index: 1,
            caller: None,
        });
        assert!(r.ok, "{:?}", r.error);
        select(&mut app, "other");
        assert!(nav(&mut app, NavDir::Back));
        assert_eq!(here(&app), "local");
        let now = app.focused_terminal().unwrap();
        assert_ne!(now, split, "the dead pane is not focused");
        assert_eq!(app.nav.current().unwrap().term.as_ref(), Some(&now));
    }

    #[test]
    fn cycling_follows_a_stable_display_order() {
        let mut app = app_with(SortBy::Alphabetical);
        for n in ["zeta", "alpha", "mid"] {
            new_session(&mut app, n);
        }
        select(&mut app, "alpha");
        app.cycle_session(1);
        // Alphabetical: alpha, local, mid, zeta.
        assert_eq!(here(&app), "local");
        app.cycle_session(1);
        assert_eq!(here(&app), "mid");
    }
}

#[cfg(test)]
mod overlay_width {
    use super::*;
    use crate::control::Req;
    use ratatui::layout::Rect;

    fn app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn frame(app: &App) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        app.render_to(&mut buf);
        buf
    }

    fn sym(buf: &Buffer, x: u16, y: u16) -> String {
        buf.cell(Position::new(x, y)).unwrap().symbol().to_string()
    }

    /// The overlay box whose top-left corner is the first `┌` at or after column `from`
    /// on row `y` — `(x0, x1)`.
    fn box_cols(buf: &Buffer, y: u16) -> (u16, u16) {
        let x0 = (0..100)
            .find(|&x| sym(buf, x, y) == "┌")
            .expect("box top-left");
        let x1 = (x0 + 1..100)
            .find(|&x| sym(buf, x, y) == "┐")
            .expect("box top-right");
        (x0, x1)
    }

    #[test]
    fn a_korean_session_name_keeps_the_switcher_border_intact() {
        let mut app = app();
        let name = "한글로된아주긴세션이름입니다한글로된아주긴세션이름입니다";
        assert!(
            app.handle_control(&Req::NewSession {
                name: Some(name.into()),
                cwd: None,
            })
            .ok
        );
        app.open_popup();
        let buf = frame(&app);
        let y0 = (0..30)
            .find(|&y| (0..100).any(|x| sym(&buf, x, y) == "┌"))
            .unwrap();
        let (x0, x1) = box_cols(&buf, y0);
        let bottom = (y0 + 1..30).find(|&y| sym(&buf, x0, y) == "└").unwrap();
        for y in y0 + 1..bottom {
            assert_eq!(sym(&buf, x1, y), "│", "right border broken on row {y}");
        }
        // The highlight bar reaches the border on the selected row (padding by width).
        let sel_y = (y0 + 1..bottom)
            .find(|&y| (x0..x1).any(|x| sym(&buf, x, y) == "▸"))
            .unwrap();
        let before_border = buf.cell(Position::new(x1 - 2, sel_y)).unwrap();
        assert_eq!(
            before_border.bg,
            Color::Cyan,
            "the selection bar is padded to the edge"
        );
    }

    #[test]
    fn the_selection_bar_reaches_the_edge_at_every_width() {
        // Odd inner widths are the case a wide glyph cannot fill exactly.
        for cols in 96..=104u16 {
            let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
            cfg.persist = false;
            let mut app = App::new(cols, 30, Vec::new(), Vec::new(), cfg).expect("app");
            assert!(
                app.handle_control(&Req::NewSession {
                    name: Some("한글".repeat(40)),
                    cwd: None,
                })
                .ok
            );
            app.open_popup();
            // Select the Korean row (the switcher preselects the previous session).
            app.popup.as_mut().unwrap().sel = 1;
            let mut buf = Buffer::empty(Rect::new(0, 0, cols, 30));
            app.render_to(&mut buf);
            let y0 = (0..30)
                .find(|&y| (0..cols).any(|x| sym(&buf, x, y) == "┌"))
                .unwrap();
            let x0 = (0..cols).find(|&x| sym(&buf, x, y0) == "┌").unwrap();
            let x1 = (x0 + 1..cols).find(|&x| sym(&buf, x, y0) == "┐").unwrap();
            let sel_y = (y0 + 1..30)
                .find(|&y| (x0..x1).any(|x| sym(&buf, x, y) == "▸"))
                .unwrap();
            let last = buf.cell(Position::new(x1 - 2, sel_y)).unwrap();
            assert_eq!(last.bg, Color::Cyan, "cols={cols}: the bar stops short");
        }
    }

    #[test]
    fn a_long_korean_prompt_keeps_its_caret_and_border() {
        let mut app = app();
        app.open_new_session_prompt();
        let mut prefix = false;
        for c in "가나다라마바사아자차카타파하".repeat(4).chars() {
            app.feed_key(
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut prefix,
            );
        }
        let buf = frame(&app);
        let y0 = (0..30)
            .find(|&y| (0..100).any(|x| sym(&buf, x, y) == "┌"))
            .unwrap();
        let (_, x1) = box_cols(&buf, y0);
        assert_eq!(sym(&buf, x1, y0 + 1), "│");
        let row: String = (0..100).map(|x| sym(&buf, x, y0 + 1)).collect();
        assert!(row.contains('█'), "the caret stays visible: {row}");
    }

    #[test]
    fn width_helpers_count_columns() {
        assert_eq!(pad_width("한글", 6).width(), 6);
        assert_eq!(pad_width("abc", 2), "ab");
        // A wide glyph that would straddle the edge is dropped and the gap padded.
        assert_eq!(pad_width("ab한", 3), "ab ");
        assert_eq!(tail_width("가나다█", 5), "나다█");
        assert_eq!(tail_width("abc", 10), "abc");
    }
}

#[cfg(test)]
mod key_help {
    use super::*;
    use ratatui::layout::Rect;

    fn app_from(toml: &str) -> App {
        let mut cfg = config::tests::config_from_str(toml);
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn press(app: &mut App, prefix: &mut bool, code: KeyCode, mods: KeyModifiers) {
        app.feed_key(KeyEvent::new(code, mods), prefix);
    }

    fn open(app: &mut App) {
        let mut prefix = false;
        press(app, &mut prefix, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(app, &mut prefix, KeyCode::Char('?'), KeyModifiers::SHIFT);
    }

    fn text(app: &App) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        app.render_to(&mut buf);
        (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buf.cell(Position::new(x, y)).unwrap().symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn opens_on_prefix_question_mark_scrolls_and_closes() {
        let mut app = app_from("");
        open(&mut app);
        assert_eq!(app.help, Some(0));
        let shown = text(&app).join("\n");
        assert!(shown.contains("split the pane side by side"), "{shown}");
        assert!(shown.contains("prefix — press ^b first"), "{shown}");
        assert!(shown.contains("1–9"), "tabs fold into one row: {shown}");
        let mut prefix = false;
        press(
            &mut app,
            &mut prefix,
            KeyCode::Char('j'),
            KeyModifiers::NONE,
        );
        assert_eq!(app.help, Some(1.min(app.help_max_scroll())));
        press(
            &mut app,
            &mut prefix,
            KeyCode::Char('G'),
            KeyModifiers::SHIFT,
        );
        assert_eq!(app.help, Some(app.help_max_scroll()), "clamped to the end");
        press(
            &mut app,
            &mut prefix,
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        );
        assert_eq!(app.help, None);
        assert!(!prefix, "closing does not leave the prefix armed");
    }

    #[test]
    fn a_rebound_key_shows_as_bound_now() {
        let mut app = app_from("prefix = \"C-a\"\n[keys]\nnew-tab = \"t\"\nkey-help = \"/\"");
        let mut prefix = false;
        press(
            &mut app,
            &mut prefix,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        press(
            &mut app,
            &mut prefix,
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        );
        assert_eq!(app.help, Some(0));
        let lines = app.help_lines();
        assert!(
            lines
                .iter()
                .any(|(k, d)| k.as_deref() == Some("t") && d == "new tab")
        );
        assert!(lines.iter().any(|(k, d)| k.is_none() && d.contains("^a")));
        assert!(app.prefix_hint_text().contains("/ all keys"));
        assert!(app.prefix_hint_text().contains("t tab"));
    }

    #[test]
    fn a_rebound_tab_key_is_listed_per_tab_and_a_multibyte_one_does_not_panic() {
        let app = app_from("[keys]\ntab-1 = \"가\"");
        let lines = app.help_lines();
        assert!(
            lines
                .iter()
                .any(|(k, d)| k.as_deref() == Some("가") && d == "go to tab 1"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|(k, d)| k.as_deref() == Some("2") && d == "go to tab 2")
        );
        assert!(
            !lines.iter().any(|(k, _)| k.as_deref() == Some("1–9")),
            "the prefix table's tabs are not a range any more"
        );
        // The global table's Alt-digits are still the conventional range.
        assert!(
            lines
                .iter()
                .any(|(k, d)| k.as_deref() == Some("M-1–9") && d == "go to tab 1–9")
        );
    }

    #[test]
    fn many_aliases_wrap_instead_of_being_clipped() {
        let app = app_from(
            "[keys]\ndetach = [\"d\", \"q\", \"D\", \"Q\", \"z\", \"Z\", \"y\", \"Y\", \"u\"]",
        );
        let lines = app.help_lines();
        let at = lines
            .iter()
            .position(|(_, d)| d == "detach this client")
            .unwrap();
        let mut keys: Vec<String> = Vec::new();
        for (k, d) in &lines[at..] {
            if d != "detach this client" && !d.is_empty() {
                break;
            }
            let k = k.as_ref().unwrap();
            assert!(k.width() <= HELP_KEY_W, "{k}");
            keys.extend(k.split(' ').map(str::to_string));
        }
        keys.sort();
        assert_eq!(keys, ["D", "Q", "Y", "Z", "d", "q", "u", "y", "z"]);
    }

    #[test]
    fn the_mouse_never_reaches_what_the_help_covers() {
        let mut app = app_from("");
        let r = app.handle_control(&control::Req::Split {
            dir: "right".into(),
            from: None,
            no_focus: false,
        });
        assert!(r.ok);
        let focused = app.focused_terminal();
        open(&mut app);
        // A click on the left pane would normally focus it.
        app.mouse_at(ClientId(0), 2, 2, MouseKind::ScrollDown);
        assert!(app.help.is_some(), "the wheel scrolls, it does not close");
        app.mouse_at(ClientId(0), 2, 2, MouseKind::Click);
        assert_eq!(app.help, None, "a click closes it");
        assert_eq!(app.focused_terminal(), focused, "and is consumed");
    }

    #[test]
    fn the_prefix_hint_replaces_the_tab_chips_only_while_armed() {
        let mut app = app_from("");
        let bar = |app: &App| text(app)[29].clone();
        assert!(!bar(&app).contains("all keys"));
        app.set_prefix_armed(true);
        let armed = bar(&app);
        assert!(armed.contains("? all keys"), "{armed}");
        assert!(armed.contains("; last"), "{armed}");
        assert!(armed.contains("% \" split"), "{armed}");
        let mut app = app_from("prefix_hint = false");
        app.set_prefix_armed(true);
        assert!(!bar(&app).contains("all keys"));
    }
}

#[cfg(test)]
mod mode_hints {
    use super::*;
    use ratatui::layout::Rect;

    fn app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn rows(app: &App) -> Vec<String> {
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        app.render_to(&mut buf);
        (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buf.cell(Position::new(x, y)).unwrap().symbol().to_string())
                    .collect()
            })
            .collect()
    }

    fn bar(app: &App) -> String {
        rows(app)[29].clone()
    }

    fn chord(app: &mut App, c: char) {
        let mut prefix = false;
        app.feed_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &mut prefix,
        );
        app.feed_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut prefix,
        );
    }

    #[test]
    fn an_empty_history_says_so_and_the_message_expires() {
        let mut app = app();
        chord(&mut app, '<');
        assert!(
            bar(&app).contains("no earlier session/tab"),
            "{}",
            bar(&app)
        );
        assert!(!app.maybe_expire_message(), "not yet");
        app.message.as_mut().unwrap().until = std::time::Instant::now();
        assert!(app.maybe_expire_message(), "expiry repaints");
        assert!(!bar(&app).contains("no earlier"));
    }

    #[test]
    fn a_spawn_failure_is_shown_in_the_bar_and_kept_for_the_api() {
        let mut app = app();
        app.report_spawn_failure("new tab", "Too many open files\n(os error 24)");
        assert!(
            bar(&app).contains("new tab failed: Too many open files (os error 24)"),
            "{}",
            bar(&app)
        );
        assert!(app.message.as_ref().unwrap().error);
        assert!(app.last_spawn_error.is_some());
    }

    #[test]
    fn the_armed_prefix_hint_wins_over_a_message() {
        let mut app = app();
        app.flash("something happened".into(), false);
        app.set_prefix_armed(true);
        assert!(bar(&app).contains("all keys"));
        assert!(!bar(&app).contains("something happened"));
    }

    #[test]
    fn modes_say_how_to_leave_them() {
        let mut app = app();
        chord(&mut app, '[');
        assert!(bar(&app).contains("SCROLL 0/"), "{}", bar(&app));
        assert!(bar(&app).contains("· q"));
        let mut prefix = false;
        app.feed_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut prefix,
        );
        chord(&mut app, 'e');
        assert!(bar(&app).contains("SIDEBAR"), "{}", bar(&app));
        app.feed_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut prefix);
        app.open_new_session_prompt();
        assert!(rows(&app).iter().any(|r| r.contains("esc cancel")));
    }
}

#[cfg(test)]
mod switcher_preview {
    use super::*;
    use crate::control::Req;
    use agentstate::AgentStatus;
    use ratatui::layout::Rect;

    fn app(cols: u16, rows: u16) -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(cols, rows, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn text(app: &App) -> String {
        let (cols, rows) = app.size();
        let mut buf = Buffer::empty(Rect::new(0, 0, cols, rows));
        app.render_to(&mut buf);
        (0..rows)
            .map(|y| {
                (0..cols)
                    .map(|x| buf.cell(Position::new(x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Make `term` look like an agent in `status` (the sweep is never run in tests).
    fn fake_agent(app: &mut App, term: &TerminalId, status: AgentStatus) {
        app.labels.insert(
            term.clone(),
            procinfo::Label {
                text: "claude".into(),
                kind: procinfo::Kind::Agent,
                pid: 1,
            },
        );
        app.agent_statuses.insert(
            term.clone(),
            AgentState {
                status,
                since: std::time::Instant::now(),
                seq: 0,
                pid: 1,
            },
        );
    }

    /// One new session per name, returning each one's terminal.
    fn sessions(app: &mut App, names: &[&str]) -> Vec<TerminalId> {
        names
            .iter()
            .map(|n| {
                assert!(
                    app.handle_control(&Req::NewSession {
                        name: Some((*n).into()),
                        cwd: None,
                    })
                    .ok
                );
                app.focused_terminal().unwrap()
            })
            .collect()
    }

    #[test]
    fn the_preview_shows_the_selected_sessions_pane_and_only_when_wide() {
        let mut app = app(120, 30);
        let t = sessions(&mut app, &["bg"]);
        assert!(
            app.handle_control(&Req::SelectSession {
                index: Some(0),
                target: None,
            })
            .ok
        );
        app.panes[&t[0]].input(b"echo PREVIEW-MARK-$((40+2))\n");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        app.open_popup(); // preselects `bg`, the previous session
        loop {
            if text(&app).contains("PREVIEW-MARK-42") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "{}", text(&app));
            std::thread::sleep(Duration::from_millis(100));
        }
        // The previewed pane counts as visible, so a background pane's output repaints.
        assert_eq!(app.popup_preview_terminal(), Some(t[0].clone()));
        let narrow = app_narrow();
        assert!(narrow.popup_preview_terminal().is_none());
    }

    fn app_narrow() -> App {
        let mut a = app(100, 30);
        a.open_popup();
        a
    }

    #[test]
    fn blocked_agents_list_first_and_the_selection_follows_its_row() {
        let mut app = app(100, 30);
        let t = sessions(&mut app, &["a", "b", "c"]);
        fake_agent(&mut app, &t[0], AgentStatus::Working);
        fake_agent(&mut app, &t[1], AgentStatus::Ready);
        fake_agent(&mut app, &t[2], AgentStatus::Blocked);
        app.open_popup();
        app.popup_tab(PopupTab::Agents);
        let order = |app: &App| -> Vec<TerminalId> {
            let p = app.popup.as_ref().unwrap();
            app.popup_items(p.tab, &p.filter)
                .into_iter()
                .map(|r| match r.target {
                    PopupTarget::Agent(t) => t,
                    PopupTarget::Session(_) => unreachable!(),
                })
                .collect()
        };
        assert_eq!(order(&app), vec![t[2].clone(), t[0].clone(), t[1].clone()]);
        // Select `a` (working, row 1); then it turns blocked and moves to the top.
        app.popup_move(1);
        fake_agent(&mut app, &t[0], AgentStatus::Blocked);
        assert!(app.reconcile_popup(), "the index moved with the row");
        let p = app.popup.as_ref().unwrap();
        assert_eq!(order(&app)[p.sel], t[0], "still on `a`");
        // ...and when it disappears the selection stays in range on something listed.
        app.labels.remove(&t[0]);
        app.reconcile_popup();
        let p = app.popup.as_ref().unwrap();
        assert!(p.sel < order(&app).len());
    }

    #[test]
    fn enter_follows_the_pinned_agent_even_before_a_reconcile() {
        // The server reconciles BEFORE the status sweep, so a re-sort can land between the
        // last reconcile and the next key: Enter must still go to the row the user chose.
        let mut app = app(100, 30);
        let t = sessions(&mut app, &["a", "b"]);
        fake_agent(&mut app, &t[0], AgentStatus::Working);
        fake_agent(&mut app, &t[1], AgentStatus::Blocked);
        app.open_popup();
        app.popup_tab(PopupTab::Agents);
        app.popup_move(1); // `a` (working) sits below the blocked `b`
        fake_agent(&mut app, &t[0], AgentStatus::Blocked); // re-sorts: `a` moves up
        fake_agent(&mut app, &t[1], AgentStatus::Ready);
        app.popup_select();
        assert_eq!(app.focused_terminal(), Some(t[0].clone()));
    }

    #[test]
    fn status_operators_filter_agents_and_sessions() {
        let mut app = app(100, 30);
        let t = sessions(&mut app, &["api", "web", "docs"]);
        fake_agent(&mut app, &t[0], AgentStatus::Blocked);
        fake_agent(&mut app, &t[1], AgentStatus::Working);
        fake_agent(&mut app, &t[2], AgentStatus::Blocked);
        let agents = |f: &str| app.popup_items(PopupTab::Agents, f).len();
        assert_eq!(agents("@"), 2);
        assert_eq!(agents("!"), 1);
        assert_eq!(agents("#"), 0);
        assert_eq!(agents("@api"), 1, "the rest is fuzzy-matched");
        assert_eq!(agents("@ api"), 1);
        let sessions = |f: &str| -> Vec<String> {
            app.popup_items(PopupTab::Sessions, f)
                .into_iter()
                .map(|r| match r.target {
                    PopupTarget::Session(w) => app.session_display_name(&w),
                    PopupTarget::Agent(_) => unreachable!(),
                })
                .collect()
        };
        assert_eq!(sessions("@"), ["api", "docs"]);
        assert_eq!(sessions("!"), ["web"]);
    }
}

#[cfg(test)]
mod attention {
    use super::*;
    use crate::control::Req;
    use agentstate::AgentStatus;

    fn app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        cfg.notify = false; // never raise a real desktop toast from a test
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn sessions(app: &mut App, names: &[&str]) -> Vec<TerminalId> {
        names
            .iter()
            .map(|n| {
                assert!(
                    app.handle_control(&Req::NewSession {
                        name: Some((*n).into()),
                        cwd: None,
                    })
                    .ok
                );
                app.focused_terminal().unwrap()
            })
            .collect()
    }

    fn blocked(app: &mut App, term: &TerminalId, waited_secs: u64) {
        app.labels.insert(
            term.clone(),
            procinfo::Label {
                text: "claude".into(),
                kind: procinfo::Kind::Agent,
                pid: 1,
            },
        );
        app.agent_statuses.insert(
            term.clone(),
            AgentState {
                status: AgentStatus::Blocked,
                since: std::time::Instant::now() - Duration::from_secs(waited_secs),
                seq: 0,
                pid: 1,
            },
        );
    }

    #[test]
    fn the_attention_key_walks_the_longest_waiting_first() {
        let mut app = app();
        let t = sessions(&mut app, &["a", "b", "c", "home"]);
        blocked(&mut app, &t[0], 10);
        blocked(&mut app, &t[1], 300);
        blocked(&mut app, &t[2], 60);
        let mut seen = Vec::new();
        for _ in 0..3 {
            app.jump_to_attention();
            seen.push(app.focused_terminal().unwrap());
        }
        assert_eq!(seen, vec![t[1].clone(), t[2].clone(), t[0].clone()]);
    }

    #[test]
    fn unread_notifications_count_until_seen_and_jump_to_the_newest() {
        let mut app = app();
        let t = sessions(&mut app, &["a", "b", "home"]);
        app.raise_notification(t[0].clone(), "claude".into(), "done", "first".into());
        app.raise_notification(t[1].clone(), "claude".into(), "blocked", "second".into());
        assert_eq!(app.unread_count(), 2);
        // Acting in an unrelated pane acknowledges nothing.
        app.ack_focused_bell();
        assert_eq!(app.unread_count(), 2);
        app.jump_unread();
        assert_eq!(
            app.focused_terminal(),
            Some(t[1].clone()),
            "the newest first"
        );
        assert_eq!(app.unread_count(), 1);
        app.jump_unread();
        assert_eq!(app.focused_terminal(), Some(t[0].clone()));
        assert_eq!(app.unread_count(), 0);
        app.jump_unread();
        assert!(app.message.as_ref().unwrap().text.contains("no unread"));
    }

    #[test]
    fn acting_on_the_pane_marks_its_notification_read() {
        let mut app = app();
        let t = sessions(&mut app, &["a", "home"]);
        app.raise_notification(t[0].clone(), "claude".into(), "done", "x".into());
        app.jump_to_terminal(&t[0]);
        assert_eq!(app.unread_count(), 1, "landing alone is not evidence");
        app.ack_focused_bell(); // what the server calls on a key/mouse event
        assert_eq!(app.unread_count(), 0);
    }
}

#[cfg(test)]
mod sidebar_git {
    use super::*;
    use crate::control::Req;
    use ratatui::layout::Rect;

    fn app() -> App {
        let (mut cfg, _) = MuxConfig::load_from(Path::new("/nonexistent/mux.toml"));
        cfg.persist = false; // never restore (or save over) the user's real sessions
        App::new(100, 30, Vec::new(), Vec::new(), cfg).expect("app")
    }

    fn sidebar_text(app: &App) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
        app.render_to(&mut buf);
        let w = app.sidebar_w();
        (0..30)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell(Position::new(x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_long_session_name_ends_in_an_ellipsis() {
        let mut app = app();
        assert!(
            app.handle_control(&Req::NewSession {
                name: Some("a-very-long-session-name-that-cannot-fit".into()),
                cwd: None,
            })
            .ok
        );
        let side = sidebar_text(&app);
        assert!(side.contains("a-very-long-session"), "{side}");
        assert!(side.contains('…'), "{side}");
    }

    #[test]
    fn a_fresh_reading_decorates_the_branch() {
        let dir = std::env::temp_dir().join(format!("comux-sbgit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feat-x\n").unwrap();
        let dir = dir.canonicalize().unwrap();
        let mut app = app();
        assert!(
            app.handle_control(&Req::NewSession {
                name: Some("repo".into()),
                cwd: Some(dir.to_string_lossy().into_owned()),
            })
            .ok
        );
        // The shell needs a moment to exist before its cwd can be read.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !app.refresh_branches_for_test().contains("feat-x") {
            assert!(std::time::Instant::now() < deadline, "branch never read");
            std::thread::sleep(Duration::from_millis(50));
        }
        gitpoll::publish(
            &app.git_poll,
            dir.clone(),
            gitpoll::GitStat {
                head: "feat-x".into(),
                changed: 2,
                ahead: 1,
                added: 5,
                removed: 2,
                ..Default::default()
            },
        );
        assert!(app.refresh_branches(), "a new reading is a change");
        assert!(app.refresh_branches_for_test().contains("feat-x* ↑1 +5 −2"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
