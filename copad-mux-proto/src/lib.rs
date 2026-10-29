//! comux **Mux API v1** (L0): what a host UI — copad on macOS/Linux, web-bridge, the iOS
//! client — needs to draw comux's chrome natively, and to drive it.
//!
//! See `docs/comux-native-integration-plan.md` for the design and the codex rounds behind it.
//! This crate is deliberately serde-only (plus `std` for [`link`]) so a host can depend on it
//! without pulling comux's terminal stack.
//!
//! # Wire
//!
//! ndjson over comux's unix socket (or `comux proxy --stdio`, which bridges that socket to a
//! pipe — one SSH exec channel per connection). The first line a client sends selects the
//! connection's role; [`SubMsg::Subscribe`] opens an L0 stream:
//!
//! 1. server → [`PubMsg::Welcome`] — check [`PROTO`] and remember `incarnation`;
//! 2. server → [`PubMsg::Snapshot`] — every topic, atomically, each with its own `rev`;
//! 3. server → [`PubMsg::State`] whenever a topic's value changes, plus [`PubMsg::Event`]s.
//!
//! **Every `State` carries the COMPLETE topic value** (replace, never a delta). `rev` is the
//! publication identity of that value; delivery order ACROSS topics is not monotonic, so a
//! client keeps the last rev per topic and ignores anything not newer ([`Model::apply`]).
//! There is no replay: a reconnect resubscribes and gets a fresh snapshot. A changed
//! `incarnation` (the server restarted) invalidates every id the client holds.
//!
//! A topic that could not be published whole arrives as [`PubMsg::Unavailable`]: drop the
//! cached value, and stop claiming any chrome that needs it.
//!
//! # Commands
//!
//! [`SubMsg::Cmd`] carries an existing comux control request (`{"cmd":"select-session",…}`,
//! the grammar `comux <verb> --json` speaks) as opaque JSON; the answer is a
//! [`PubMsg::Reply`] with the same `id`. Replies are delivered reliably or the connection is
//! dropped — never silently lost after the mutation ran.
//!
//! # Host chrome
//!
//! A render client (a `comux` attached in a terminal, or a native client speaking the render
//! protocol) may carry a binding `tag` (`COPAD_MUX_HOST_TAG`, or `copad:<panel>@<socket>`
//! inside a copad tab). The `clients` topic lists attachments with their tags; a host that
//! finds its own sends [`SubMsg::HostReady`] for that ATTACHMENT id. While it holds the claim
//! and keeps pinging (every [`PING_SECS`]), comux stops drawing those [`Feature`]s — but only
//! when EVERY render client is claimed for them, because the frame is shared. A claim lapses
//! after [`CLAIM_EXPIRY_SECS`] of silence or when the subscription drops, and the TUI chrome
//! comes back on the next frame.
//!
//! # Render protocol (not defined here)
//!
//! The render stream (`{"t":"attach"}`) keeps comux's existing JSON grammar: `WireCell` colors
//! are ratatui's serde strings (`"Reset"`, `"Red"`, `"#rrggbb"`, `"12"` for indexed), modifiers
//! are bitflag strings (`"BOLD | DIM"`, `""` for none), keys are crossterm `KeyEvent`s. comux
//! pins that grammar with golden tests (`copad-mux/src/proto.rs`) so a dependency bump cannot
//! change it silently; a native client implements it from those tests.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub mod link;

/// The L0 protocol version. A host refuses a server whose [`PubMsg::Welcome`] says otherwise.
pub const PROTO: u32 = 1;

/// How often a host pings while it holds a chrome claim.
pub const PING_SECS: u64 = 10;

/// Silence after which the server drops a subscriber's claims (the TUI chrome returns).
pub const CLAIM_EXPIRY_SECS: u64 = 30;

/// Topic names, in snapshot order.
pub mod topic {
    pub const TREE: &str = "tree";
    pub const AGENTS: &str = "agents";
    pub const NOTIFICATIONS: &str = "notifications";
    pub const STATUS: &str = "status";
    pub const CLIENTS: &str = "clients";
    pub const ALL: [&str; 5] = [TREE, AGENTS, NOTIFICATIONS, STATUS, CLIENTS];
}

/// A piece of TUI chrome a host can take over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Feature {
    /// The left sidebar (spaces + agents). Needs `tree` + `agents`.
    Sidebar,
    /// The bottom status bar. Needs `status` + `tree`; a host claiming it MUST show
    /// `prefix_armed` and `flash`, which the TUI then stops showing.
    Status,
    /// Overlays (`Ctrl-f`, `Ctrl-b a`, prompts …) delegated to the host (P3).
    Overlays,
}

impl Feature {
    /// The topics the host must hold for this feature to be honest.
    pub fn needs(self) -> &'static [&'static str] {
        match self {
            Feature::Sidebar => &[topic::TREE, topic::AGENTS],
            Feature::Status => &[topic::STATUS, topic::TREE],
            Feature::Overlays => &[topic::TREE],
        }
    }
}

/// Client → server, one per line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum SubMsg {
    /// First line: open an L0 stream.
    Subscribe { v: u32 },
    /// Run a comux control request (`req` = the `{"cmd":…}` object).
    Cmd { id: String, req: Value },
    /// Claim `attachment` (an id from the `clients` topic) for `features`.
    HostReady {
        attachment: u64,
        incarnation: String,
        features: Vec<Feature>,
    },
    /// Release a claim.
    HostGone { attachment: u64 },
    /// Liveness; also what keeps claims alive.
    Ping { id: u64 },
    /// The answer to an [`PubMsg::Overlay`] (P3): `choice` for a picker/confirm, `text` for a
    /// prompt; both absent = cancelled.
    OverlayResult {
        overlay: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Ask comux to open one of its overlays FOR `attachment` (decisions #172): what a key
    /// would open, delegated straight back to this host. Native buttons and context menus use
    /// it so a rename still gets comux's seeded prompt and a kill still asks comux's confirm.
    /// `what` = `new-session` | `resume` | `switcher` | `tabs` (the switcher on the attached
    /// session's tabs) | `notifications` | `new-worktree` |
    /// `rename-session` | `kill-session` | `rename-tab` | `close-tab`; `target` names the
    /// session (`<session id>`) or tab (`<session id>/<tab id>`), default = the active one.
    Open {
        attachment: u64,
        what: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
}

/// One topic value inside a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopicValue {
    pub rev: u64,
    pub data: Value,
}

/// Server → client, one per line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum PubMsg {
    Welcome {
        proto: u32,
        /// Minted at server start; changes on every restart.
        incarnation: String,
        /// comux's crate version, for diagnostics.
        version: String,
    },
    Snapshot {
        rev: u64,
        topics: BTreeMap<String, TopicValue>,
    },
    State {
        topic: String,
        rev: u64,
        data: Value,
    },
    /// The topic could not be published whole (too large): drop the cached value.
    Unavailable {
        topic: String,
        rev: u64,
        reason: String,
    },
    /// Something happened that is not state: `notify` (a new notification, for a toast;
    /// its content is also in the `notifications` topic), `flash` (an action's outcome
    /// message), `bell`.
    Event {
        kind: String,
        data: Value,
    },
    Reply {
        id: String,
        resp: Value,
    },
    Pong {
        id: u64,
    },
    /// A delegated overlay for the host to draw (P3). `client` = the render attachment whose
    /// key opened it; only that attachment's claiming host receives it.
    Overlay {
        overlay: u64,
        client: u64,
        spec: OverlaySpec,
    },
    /// The server closed an overlay itself (its target vanished, the client detached).
    OverlayClosed {
        overlay: u64,
    },
    /// A protocol-level refusal (bad first line, version mismatch).
    Error {
        code: String,
        message: String,
    },
}

/// What a delegated overlay asks the host to show. Three generic shapes cover every comux
/// overlay; the server keeps the meaning, the host only collects a choice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum OverlaySpec {
    /// A filterable list, optionally split into tabs (`Ctrl-f`: sessions | agents).
    Picker {
        title: String,
        #[serde(default)]
        tabs: Vec<PickerTab>,
        /// Index into `tabs` to open on.
        #[serde(default)]
        tab: usize,
        /// Extra per-row actions besides the default (e.g. `dismiss` in the notification
        /// center); the host answers with `choice = "<action>:<row id>"`.
        #[serde(default)]
        actions: Vec<PickerAction>,
        /// The row a plain Enter picks before anything is typed (the TUI's own default —
        /// `Ctrl-f` opens on the previous session so `Ctrl-f Enter` toggles).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected: Option<String>,
        /// The rows are still being gathered off-loop (a directory listing, a transcript
        /// scan): show a spinner. The same overlay id is sent again with the rows; a host
        /// REPLACES an open overlay whose id it already shows (decisions #172).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        loading: bool,
        /// A line shown above the rows (an unreadable directory's error).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        /// Row-independent keys (the browser's `Left` → `up`, the resume picker's `C-a`).
        /// A declared key wins over the host's defaults (←/→ switch tabs, ↑/↓ move, Enter
        /// picks, Esc cancels).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<PickerKey>,
        /// No filter field (the notification center): printable keys are free for
        /// `keys`/`actions` (`d` dismisses, like the TUI).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_filter: bool,
    },
    /// A single-line text prompt (rename, new session name, worktree branch).
    Prompt {
        title: String,
        #[serde(default)]
        initial: String,
        #[serde(default)]
        placeholder: String,
    },
    /// A yes/no question (kill session, close pane).
    Confirm { message: String },
    /// A context menu (right-click on a tab chip / session / agent): the host shows its own
    /// menu and answers with the chosen item's id.
    Menu { title: String, items: Vec<MenuItem> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MenuItem {
    pub id: String,
    pub label: String,
    /// Kills or closes something (drawn red).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub destructive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PickerTab {
    pub name: String,
    pub rows: Vec<PickerRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PickerRow {
    /// Opaque; sent back as the choice.
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Status word for colouring (`working`, `blocked`, `done` …), empty when none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub badge: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub current: bool,
    /// The picker's per-row actions don't apply to this row (the resume picker's
    /// "Show headless runs" toggle).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_actions: bool,
    /// The pane a host previews for this row (`pane-preview`): an agent's own pane, a
    /// session's active tab's focused pane, a notification's pane. Also the join key into
    /// the `agents` topic, so a host can draw the row from live state.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane: String,
    /// The `tree` session this row stands for, when it is one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
}

impl PickerRow {
    pub fn new(id: String, label: String, detail: String) -> Self {
        PickerRow {
            id,
            label,
            detail,
            badge: String::new(),
            current: false,
            no_actions: false,
            pane: String::new(),
            session: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PickerAction {
    pub id: String,
    pub label: String,
    /// The key that applies it to the highlighted row (`C-r`, `C-f`, `d`, `Right`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Answering with it leaves the overlay open: comux re-sends it under the same id
    /// (dismissing one notification keeps the center up, as in the TUI).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stays: bool,
}

/// A row-independent key of a picker. Key names: `Left`, `Right`, `C-a` (Ctrl-a), `D` (a
/// printable character as itself).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PickerKey {
    pub key: String,
    /// The answer it sends; empty = cancel.
    #[serde(default)]
    pub choice: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stays: bool,
}

/// A pane's screen as styled runs (`pane-preview`), what the TUI switcher previews.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Preview {
    pub pane: String,
    /// `space/tab · label status`, the TUI preview's header.
    #[serde(default)]
    pub header: String,
    pub cols: u16,
    /// Bottom-anchored rows of the pane's current viewport, top to bottom.
    pub lines: Vec<Vec<Run>>,
}

/// Cells of one style. Default colours and false attributes are omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub t: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<RunColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<RunColor>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dim: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverse: bool,
}

/// A palette index (0–255, the host's theme for 0–15) or `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Serialize for RunColor {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            RunColor::Indexed(i) => s.serialize_u8(i),
            RunColor::Rgb(r, g, b) => s.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}")),
        }
    }
}

impl<'de> Deserialize<'de> for RunColor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Number(n) => n
                .as_u64()
                .and_then(|i| u8::try_from(i).ok())
                .map(RunColor::Indexed)
                .ok_or_else(|| serde::de::Error::custom("palette index out of range")),
            Value::String(s) => {
                let hex = s.strip_prefix('#').filter(|h| h.len() == 6 && h.is_ascii());
                let byte = |i: usize| hex.and_then(|h| u8::from_str_radix(&h[i..i + 2], 16).ok());
                match (byte(0), byte(2), byte(4)) {
                    (Some(r), Some(g), Some(b)) => Ok(RunColor::Rgb(r, g, b)),
                    _ => Err(serde::de::Error::custom("colour is not #rrggbb")),
                }
            }
            _ => Err(serde::de::Error::custom("colour is a number or #rrggbb")),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Topic payloads
// ---------------------------------------------------------------------------------------

/// `tree`: every session → tab → pane.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Tree {
    pub sessions: Vec<Session>,
    /// The active session's id.
    #[serde(default)]
    pub active: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    /// Stable id (`local`, `s3` …) — the join key and the `select-session` target.
    pub id: String,
    pub index: usize,
    /// Display name (what a rename changes; the id when unnamed).
    pub name: String,
    pub active: bool,
    /// git branch of the session's directory, empty when none.
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub dir: String,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Tab {
    /// Stable tab selector (`s3:t7`) — the `select-tab` target.
    pub id: String,
    pub index: usize,
    /// Custom name, empty when unnamed.
    #[serde(default)]
    pub name: String,
    /// What the TUI chip shows: the custom name, else the focused pane's process label.
    pub title: String,
    pub active: bool,
    /// Any pane in it rang an unacknowledged bell.
    #[serde(default)]
    pub bell: bool,
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Pane {
    /// `$COPAD_MUX_PANE` identity — the `jump` / `focus` target.
    pub token: String,
    pub index: usize,
    pub focused: bool,
    #[serde(default)]
    pub label: String,
    /// `agent` / `shell` / `other`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: String,
}

/// `agents`: every agent pane across sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Agents {
    pub agents: Vec<Agent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Agent {
    pub token: String,
    pub space: String,
    pub space_id: String,
    pub tab_id: String,
    pub tab_index: usize,
    /// The row title the sidebar uses (tab name, else `tab <n>`).
    pub title: String,
    pub tool: String,
    /// `working` / `ready` / `blocked` / `idle` — cached and inferred.
    pub status: String,
    /// UNIX seconds the status began (a host renders "4m" itself, so this topic does not
    /// change every second).
    pub since_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The pane is the focused pane of the active tab.
    #[serde(default)]
    pub focused: bool,
}

/// `notifications`: the notification center (`Ctrl-b a`), newest first.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Notifications {
    pub items: Vec<Notification>,
    pub unread: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    /// Monotonic within an incarnation; the `notif-read` target.
    pub id: u64,
    /// Wall-clock `HH:MM`.
    pub when: String,
    /// `done` / `blocked`.
    pub kind: String,
    pub tool: String,
    pub space: String,
    pub tab: String,
    pub body: String,
    /// The pane it came from (`jump` target); empty when that pane is gone.
    pub token: String,
    pub read: bool,
}

/// `status`: everything the TUI status bar shows besides the tabs (which are in `tree`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// Some attached client is mid-chord (`^b` armed).
    pub prefix_armed: bool,
    /// The prefix chord's label (`^b`, `^a`).
    pub prefix_label: String,
    /// The current outcome/warning line, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash: Option<Flash>,
    /// Panes with an unacknowledged bell.
    pub bells: usize,
    /// Blocked agents (`⚑N`).
    pub attention: usize,
    /// Agent panes (`● N`).
    pub agents: usize,
    pub unread: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// A newer comux release, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
    /// The machine's hostname.
    #[serde(default)]
    pub host: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Flash {
    pub text: String,
    #[serde(default)]
    pub error: bool,
}

/// Subscription rate-limit windows, percent used. `None` = not reported.
///
/// `windows` are the ones the user's `usage_windows` selects. The rest describes the readout
/// as the TUI draws it (decisions #182), so a host draws the same thing: `layout` `paged`
/// (one page at a time, `page` of `pages`, which comux rotates every `usage_rotate_secs`) or
/// `inline`; `gauge` `bar` or `text`. All empty/0 from an older comux (inline, bars).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub windows: Vec<UsageWindow>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layout: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<UsagePage>,
    #[serde(default)]
    pub page: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gauge: String,
}

/// One carousel page: `kind` `full` (gauge + percent + reset), `usage` (gauge + percent) or
/// `reset` (reset only); `windows` index into [`Usage::windows`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsagePage {
    pub kind: String,
    pub windows: Vec<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    /// `claude` / `codex`.
    pub provider: String,
    /// `5h` / `wk`.
    pub window: String,
    pub pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// A cached value shown while the live read is failing (`~` in the TUI).
    #[serde(default)]
    pub stale: bool,
    /// The reset as the TUI shows it (`usage_reset`: `2h13m` / `Wed 09:00`), empty when off
    /// or unknown. Absent from an older comux: a host then formats `resets_at` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_text: Option<String>,
}

/// `clients`: render attachments, so a host can find (and claim) its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Clients {
    pub clients: Vec<ClientInfo>,
    /// Features currently hidden from the shared frame.
    #[serde(default)]
    pub hidden: Vec<Feature>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClientInfo {
    pub id: u64,
    #[serde(default)]
    pub tag: String,
    pub cols: u16,
    pub rows: u16,
    /// Features some host currently claims for this attachment.
    #[serde(default)]
    pub claimed: Vec<Feature>,
}

/// A host's view of every topic, kept current by [`Model::apply`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    pub incarnation: String,
    pub version: String,
    pub tree: Option<Tree>,
    pub agents: Option<Agents>,
    pub notifications: Option<Notifications>,
    pub status: Option<Status>,
    pub clients: Option<Clients>,
    revs: BTreeMap<String, u64>,
}

/// What [`Model::apply`] did with a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// The server restarted (or this is the first welcome): every cached id is invalid.
    Welcome,
    /// These topics changed.
    Topics(Vec<String>),
    /// A topic went unavailable.
    Dropped(String),
    /// Older than what we hold, or not state at all.
    Ignored,
}

impl Model {
    /// Fold one server message into the model.
    pub fn apply(&mut self, msg: &PubMsg) -> Applied {
        match msg {
            PubMsg::Welcome {
                incarnation,
                version,
                ..
            } => {
                *self = Model {
                    incarnation: incarnation.clone(),
                    version: version.clone(),
                    ..Model::default()
                };
                Applied::Welcome
            }
            PubMsg::Snapshot { topics, .. } => {
                let changed = topics
                    .iter()
                    .filter(|(name, v)| self.set(name, v.rev, &v.data))
                    .map(|(name, _)| name.clone())
                    .collect();
                Applied::Topics(changed)
            }
            PubMsg::State { topic, rev, data } => {
                if self.set(topic, *rev, data) {
                    Applied::Topics(vec![topic.clone()])
                } else {
                    Applied::Ignored
                }
            }
            PubMsg::Unavailable { topic, rev, .. } => {
                if self.revs.get(topic).is_some_and(|r| r >= rev) {
                    return Applied::Ignored;
                }
                self.revs.insert(topic.clone(), *rev);
                match topic.as_str() {
                    topic::TREE => self.tree = None,
                    topic::AGENTS => self.agents = None,
                    topic::NOTIFICATIONS => self.notifications = None,
                    topic::STATUS => self.status = None,
                    topic::CLIENTS => self.clients = None,
                    _ => return Applied::Ignored,
                }
                Applied::Dropped(topic.clone())
            }
            _ => Applied::Ignored,
        }
    }

    /// Whether every topic `features` needs is held.
    pub fn can_host(&self, features: &[Feature]) -> bool {
        features.iter().flat_map(|f| f.needs()).all(|t| self.has(t))
    }

    fn has(&self, name: &str) -> bool {
        match name {
            topic::TREE => self.tree.is_some(),
            topic::AGENTS => self.agents.is_some(),
            topic::NOTIFICATIONS => self.notifications.is_some(),
            topic::STATUS => self.status.is_some(),
            topic::CLIENTS => self.clients.is_some(),
            _ => false,
        }
    }

    /// Store `data` as topic `name` at `rev` unless we already hold something as new.
    /// A value that fails to decode is treated as unavailable rather than kept stale.
    fn set(&mut self, name: &str, rev: u64, data: &Value) -> bool {
        if self.revs.get(name).is_some_and(|r| *r >= rev) {
            return false;
        }
        fn de<T: serde::de::DeserializeOwned>(v: &Value) -> Option<T> {
            serde_json::from_value(v.clone()).ok()
        }
        match name {
            topic::TREE => self.tree = de(data),
            topic::AGENTS => self.agents = de(data),
            topic::NOTIFICATIONS => self.notifications = de(data),
            topic::STATUS => self.status = de(data),
            topic::CLIENTS => self.clients = de(data),
            _ => return false,
        }
        self.revs.insert(name.to_string(), rev);
        true
    }

    /// The attachment whose tag is `tag`, newest first (the highest id is the latest attach).
    pub fn attachment_for(&self, tag: &str) -> Option<&ClientInfo> {
        self.clients
            .as_ref()?
            .clients
            .iter()
            .filter(|c| !c.tag.is_empty() && c.tag == tag)
            .max_by_key(|c| c.id)
    }
}

/// The binding tag a comux client running inside a copad tab reports, and the one copad
/// computes for its own tab. One definition so the two cannot drift.
pub fn copad_tag(panel_id: &str, socket: &str) -> String {
    format!("copad:{panel_id}@{socket}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(topic: &str, rev: u64, data: Value) -> PubMsg {
        PubMsg::State {
            topic: topic.into(),
            rev,
            data,
        }
    }

    #[test]
    fn wire_shapes_are_tagged_kebab_case() {
        let m = SubMsg::HostReady {
            attachment: 3,
            incarnation: "ab".into(),
            features: vec![Feature::Sidebar, Feature::Status],
        };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"t":"host-ready","attachment":3,"incarnation":"ab","features":["sidebar","status"]})
        );
        let p: PubMsg = serde_json::from_value(json!({"t":"pong","id":7})).unwrap();
        assert_eq!(p, PubMsg::Pong { id: 7 });
    }

    #[test]
    fn a_stale_revision_never_overwrites_a_newer_one() {
        let mut m = Model::default();
        let t = |name: &str| json!({"sessions":[], "active": name});
        assert_eq!(
            m.apply(&state("tree", 5, t("new"))),
            Applied::Topics(vec!["tree".into()])
        );
        // An owed topic retried late: older publication identity, must be ignored.
        assert_eq!(m.apply(&state("tree", 4, t("old"))), Applied::Ignored);
        assert_eq!(m.tree.as_ref().unwrap().active, "new");
        // Revisions are per topic: an agents value at a LOWER global rev still lands.
        assert_eq!(
            m.apply(&state("agents", 2, json!({"agents":[]}))),
            Applied::Topics(vec!["agents".into()])
        );
    }

    #[test]
    fn welcome_resets_everything_and_unavailable_drops_the_topic() {
        let mut m = Model::default();
        m.apply(&state("tree", 1, json!({"sessions":[],"active":"a"})));
        assert!(m.can_host(&[Feature::Overlays]));
        m.apply(&PubMsg::Unavailable {
            topic: "tree".into(),
            rev: 2,
            reason: "too large".into(),
        });
        assert!(m.tree.is_none());
        assert!(!m.can_host(&[Feature::Overlays]));
        m.apply(&state("tree", 3, json!({"sessions":[],"active":"b"})));
        assert_eq!(
            m.apply(&PubMsg::Welcome {
                proto: PROTO,
                incarnation: "x".into(),
                version: "1".into()
            }),
            Applied::Welcome
        );
        assert!(m.tree.is_none());
        // A fresh incarnation starts revisions over.
        m.apply(&state("tree", 1, json!({"sessions":[],"active":"c"})));
        assert_eq!(m.tree.unwrap().active, "c");
    }

    #[test]
    fn an_undecodable_value_is_not_kept() {
        let mut m = Model::default();
        m.apply(&state("status", 1, json!({"bogus": true})));
        assert!(m.status.is_none());
    }

    #[test]
    fn hosting_a_feature_needs_all_of_its_topics() {
        let mut m = Model::default();
        m.apply(&state("tree", 1, json!({"sessions":[],"active":""})));
        assert!(!m.can_host(&[Feature::Sidebar]));
        m.apply(&state("agents", 1, json!({"agents":[]})));
        assert!(m.can_host(&[Feature::Sidebar]));
    }

    #[test]
    fn the_newest_attachment_wins_a_shared_tag() {
        let mut m = Model::default();
        m.apply(&state(
            "clients",
            1,
            json!({"clients":[
                {"id":2,"tag":"copad:p1@s","cols":80,"rows":24},
                {"id":9,"tag":"copad:p1@s","cols":80,"rows":24},
                {"id":5,"tag":"","cols":80,"rows":24}
            ]}),
        ));
        assert_eq!(m.attachment_for("copad:p1@s").unwrap().id, 9);
        assert!(m.attachment_for("").is_none());
    }
}

#[cfg(test)]
mod run_color_tests {
    use super::RunColor;

    #[test]
    fn colours_round_trip_and_malformed_ones_are_errors_not_panics() {
        let c: RunColor = serde_json::from_str("\"#0a0b0c\"").unwrap();
        assert_eq!(c, RunColor::Rgb(10, 11, 12));
        assert_eq!(serde_json::to_string(&RunColor::Indexed(7)).unwrap(), "7");
        // Six bytes, but not six ASCII hex digits (a multi-byte char straddles the slices).
        assert!(serde_json::from_str::<RunColor>("\"#aéabc\"").is_err());
        assert!(serde_json::from_str::<RunColor>("\"#zzzzzz\"").is_err());
        assert!(serde_json::from_str::<RunColor>("300").is_err());
    }
}
