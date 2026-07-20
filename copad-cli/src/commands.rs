use clap::{Parser, Subcommand};
use serde_json::json;

use crate::plugin_cmds::bookmark::BookmarkCommand;
use crate::plugin_cmds::calendar::CalendarCommand;
use crate::plugin_cmds::git::GitCommand;
use crate::plugin_cmds::jira::JiraCommand;
use crate::plugin_cmds::recent::RecentArgs;
use crate::plugin_cmds::slack::SlackCommand;
use crate::plugin_cmds::todo::TodoCommand;

#[derive(Parser)]
#[command(name = "coctl", about = "copad CLI", version)]
pub struct Cli {
    /// Socket path override
    #[arg(long)]
    pub socket: Option<String>,

    /// Output JSON format
    #[arg(long, default_value_t = false, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Ping the running copad instance
    Ping,

    /// Show workflow context. Default (or `--full`) aggregates panel +
    /// cwd + git + todos + calendar + messenger auth. Bare `--json` keeps
    /// the raw `context.snapshot` shape; `--json --full` emits the aggregate.
    Context {
        /// Aggregate cross-plugin context (default in human mode;
        /// opt-in for `--json`).
        #[arg(long)]
        full: bool,
    },

    /// Manual at-keyboard / away toggle. `away` opens external-sink
    /// trigger actions (Discord notifications etc.); `active` closes
    /// them. `status` prints `active` or `away` for shell scripting.
    #[command(subcommand)]
    Presence(PresenceCommand),

    /// Panel management (a "panel" is a sub-tab; use `workspace` for the
    /// tmux-session-like level)
    #[command(subcommand)]
    Session(SessionCommand),

    /// Workspace-session management — the tmux-session-like top level (each
    /// holds its own sub-tabs). `workspace new` roots one at the current dir.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),

    /// Background image management
    #[command(subcommand)]
    Background(BackgroundCommand),

    /// Tab management
    #[command(subcommand)]
    Tab(TabCommand),

    /// Split pane management
    #[command(subcommand)]
    Split(SplitCommand),

    /// Pane focus movement (the Ctrl+Shift+N / Ctrl+Shift+Left bindings,
    /// reachable by scripts and agents)
    #[command(subcommand)]
    Pane(PaneCommand),

    /// Event stream
    #[command(subcommand)]
    Event(EventCommand),

    /// WebView panel management
    #[command(subcommand)]
    Webview(WebviewCommand),

    /// Terminal agent commands (read, exec, state)
    #[command(subcommand)]
    Terminal(TerminalCommand),

    /// AI agent commands (approval workflow + local status readout)
    #[command(subcommand)]
    Agent(AgentCommand),

    /// Claude/Codex token + cost usage, aggregated from local transcripts
    /// (`~/.claude/projects`, `~/.codex/sessions`). Runs LOCALLY — no daemon,
    /// no socket — so it works over SSH. Pipe `--oneline` into a tmux
    /// `status-right` for an always-on cost readout.
    Usage(UsageArgs),

    /// Theme management
    #[command(subcommand)]
    Theme(ThemeCommand),

    /// Plugin management
    #[command(subcommand)]
    Plugin(PluginCommand),

    /// Todo shortcuts (`todo.*` actions with prefix-resolved ids + list view)
    #[command(subcommand)]
    Todo(TodoCommand),

    /// Git shortcuts (`git.*` actions with cwd-derived workspace defaulting
    /// + table renderers for workspaces / worktrees / status)
    #[command(subcommand)]
    Git(GitCommand),

    /// Bookmark shortcuts (`bookmark.*` URL → KB capture; urlhash8 prefix)
    #[command(subcommand)]
    Bookmark(BookmarkCommand),

    /// Jira shortcuts (`jira.*` actions — `mine` / `ticket` / `transition`
    /// / `comment` / `auth-status`)
    #[command(subcommand)]
    Jira(JiraCommand),

    /// Slack shortcuts (`slack.*` actions — `send` / `get` / `auth-status`)
    #[command(subcommand)]
    Slack(SlackCommand),

    /// Calendar shortcuts (`calendar.*` actions — `today` / `next` /
    /// `event` / `auth-status`)
    #[command(subcommand)]
    Calendar(CalendarCommand),

    /// Project shortcuts (`project.*` actions — `list` / `resolve`)
    #[command(subcommand)]
    Project(ProjectCommand),

    /// Workflow shortcuts (`workflow.*` actions — `list` / `get` / `run`)
    #[command(subcommand)]
    Workflow(WorkflowCommand),

    /// Runledger replay (`events.replay`). Phase 22.6.
    #[command(subcommand)]
    Runledger(RunledgerCommand),

    /// Recent bus events ("what happened?" — wraps `event.history`).
    Recent(RecentArgs),

    /// Status bar management
    #[command(subcommand)]
    Statusbar(StatusBarCommand),

    /// Check for updates or update copad
    #[command(subcommand)]
    Update(UpdateCommand),

    /// Invoke a registry action by name (escape hatch for any action,
    /// including service-plugin actions like `echo.ping` or `kb.search`).
    Call {
        /// Action name (e.g. `system.ping`, `echo.ping`, `kb.search`)
        method: String,
        /// JSON params object passed verbatim to the action
        #[arg(long, default_value = "{}")]
        params: String,
    },
}

#[derive(Subcommand)]
pub enum UpdateCommand {
    /// Check if a newer version is available
    Check,
    /// Download and install the latest version
    Apply {
        /// Install a specific version (e.g., v0.1.0)
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PresenceCommand {
    /// Mark presence as away — external-sink trigger actions fire.
    Away,
    /// Mark presence as active — external-sink trigger actions stay quiet.
    Active,
    /// Print the current presence (`active` or `away`) on stdout.
    Status,
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List all panels
    List,
    /// Get detailed info for a panel
    Info {
        /// Panel ID
        id: String,
    },
}

#[derive(Subcommand)]
pub enum WorkspaceCommand {
    /// Create a new workspace-session rooted at a directory (defaults to the
    /// current directory — `cd` somewhere, then `coctl workspace new`, the way
    /// `tmux new-session` roots a session at $PWD).
    New {
        /// Directory to root the workspace at (default: current directory)
        dir: Option<String>,
    },
    /// List all workspace-sessions
    List,
    /// Switch to a workspace-session by id
    Switch {
        /// Workspace id (from `workspace list`)
        id: String,
    },
    /// Jump to the next workspace-session (wraps)
    Cycle,
}

#[derive(Subcommand)]
pub enum BackgroundCommand {
    /// Set background image
    Set { path: String },
    /// Clear background image
    Clear,
    /// Set tint opacity (0.0 - 1.0)
    SetTint { opacity: f64 },
    /// Switch to next random background
    Next,
    /// Toggle background visibility
    Toggle,
    /// Delete the current list-picked wallpaper (disk + list) and rotate
    DeleteCurrent,
}

#[derive(Subcommand)]
pub enum TabCommand {
    /// Create a new tab
    New,
    /// Close the focused tab/panel
    Close,
    /// List tabs
    List,
    /// Extended tab info with panel counts
    Info,
    /// Switch to a tab by zero-based index (the index `tab info` reports)
    Switch {
        /// Zero-based tab index
        index: usize,
    },
    /// Toggle tab bar visibility
    ToggleBar,
    /// Rename a tab
    Rename {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// New title
        title: String,
    },
}

#[derive(Subcommand)]
pub enum PaneCommand {
    /// Focus the next pane in the active tab
    FocusNext,
    /// Focus the previous pane in the active tab
    FocusPrev,
}

#[derive(Subcommand)]
pub enum SplitCommand {
    /// Split horizontally
    Horizontal,
    /// Split vertically
    Vertical,
}

#[derive(Subcommand)]
pub enum EventCommand {
    /// Subscribe to terminal events (streams JSON lines)
    Subscribe,
    /// Publish an event onto the daemon's bus. Useful for firing
    /// `[[triggers]]` from shell scripts. Source is daemon-stamped
    /// (`client.<pid>` via SO_PEERCRED); timestamp is daemon-stamped.
    /// Events from this entry point are tagged `External` origin and
    /// reach only triggers with `[security] accept_external = true`.
    Publish {
        /// Event kind (e.g. `panel.focused`, `my.custom`). Cannot end
        /// in `.completed` or `.failed` — those are reserved for the
        /// action-registry completion contract.
        kind: String,
        /// Optional JSON payload. Defaults to `{}`. Use shell quoting
        /// (single quotes) to pass spaces / nested JSON.
        payload: Option<String>,
        /// Silence transport errors and exit 0 when the daemon socket
        /// is missing or unreachable. Intended for shell hook callers
        /// that should never break the host command when copadd is
        /// down. Schema errors (invalid JSON, reserved kind) still
        /// exit non-zero.
        #[arg(long, default_value_t = false)]
        quiet: bool,
    },
}

#[derive(Subcommand)]
pub enum TerminalCommand {
    /// Read visible terminal screen text
    Read {
        /// Panel ID (defaults to active terminal)
        #[arg(long)]
        id: Option<String>,
        /// Start row (0-based, for range read)
        #[arg(long)]
        start_row: Option<i64>,
        /// Start column (0-based, for range read)
        #[arg(long)]
        start_col: Option<i64>,
        /// End row (0-based, for range read)
        #[arg(long)]
        end_row: Option<i64>,
        /// End column (0-based, for range read)
        #[arg(long)]
        end_col: Option<i64>,
    },
    /// Get terminal state (cursor, dimensions, CWD, title)
    State {
        /// Panel ID (defaults to active terminal)
        #[arg(long)]
        id: Option<String>,
    },
    /// Execute a command in the terminal (sends text + newline)
    Exec {
        /// Panel ID (defaults to active terminal)
        #[arg(long)]
        id: Option<String>,
        /// Command to execute
        command: String,
    },
    /// Send raw text to the terminal (no newline appended)
    Feed {
        /// Panel ID (defaults to active terminal)
        #[arg(long)]
        id: Option<String>,
        /// Text to send
        text: String,
    },
    /// Read terminal scrollback history
    History {
        /// Panel ID (defaults to active terminal)
        #[arg(long)]
        id: Option<String>,
        /// Number of scrollback lines to read
        #[arg(long, default_value_t = 100)]
        lines: i64,
    },
    /// Get combined terminal context (state + screen + scrollback)
    Context {
        /// Panel ID (defaults to active terminal)
        #[arg(long)]
        id: Option<String>,
        /// Number of scrollback history lines to include
        #[arg(long, default_value_t = 50)]
        history_lines: i64,
    },
}

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// List configured projects
    List,
    /// Resolve a project by name/alias, cwd, git_remote, or active context
    Resolve {
        /// Match by canonical name or alias
        #[arg(long)]
        name: Option<String>,
        /// Match by walking up cwd ancestors
        #[arg(long)]
        cwd: Option<String>,
        /// Match by canonical "owner/repo" git remote
        #[arg(long)]
        git_remote: Option<String>,
        /// Resolve active project (pane_context → active_cwd fallback);
        /// mutually exclusive with the explicit flags above.
        #[arg(long)]
        active: bool,
    },
}

#[derive(Subcommand)]
pub enum WorkflowCommand {
    /// List available workflows
    List,
    /// Show full WorkflowSpec for one id (includes prompt + form_fields)
    Get {
        /// Workflow id (e.g. `ship`)
        id: String,
    },
    /// Run a workflow (opens a new tab with claude.start dispatched against
    /// the resolved project + substituted prompt template).
    Run {
        /// Workflow id (e.g. `ship`)
        #[arg(long)]
        id: String,
        /// Explicit project name/alias (overrides active-pane resolution)
        #[arg(long)]
        project: Option<String>,
        /// JSON object of form values (takes precedence over --value
        /// repeated flags when both are provided)
        #[arg(long)]
        values: Option<String>,
        /// Repeatable `name=value` form field. Multiple uses build a JSON
        /// object. Ignored if `--values` is also given.
        #[arg(long = "value", value_parser = parse_kv)]
        kv: Vec<(String, String)>,
    },
}

#[derive(Subcommand)]
pub enum RunledgerCommand {
    /// Replay events from the durable ledger
    Query {
        /// Lower bound timestamp (epoch millis) — default 0 (everything)
        #[arg(long, default_value_t = 0)]
        since_ms: i64,
        /// Comma-separated kind globs (e.g. `mission.*,goal.tick.*`)
        #[arg(long)]
        kinds: Option<String>,
        /// Max entries
        #[arg(long)]
        limit: Option<u64>,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected `name=value`, got `{s}`"))
}

fn kv_to_object(kv: &[(String, String)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in kv {
        map.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    serde_json::Value::Object(map)
}

/// `coctl usage` flags. Window defaults to the current local day; `--since`
/// overrides it with a rolling wall-clock window (honest naming — copad can't
/// know Anthropic's exact 5h reset). `--oneline` is the tmux `status-right`
/// form; `--json` (global) is the machine shape.
#[derive(clap::Args)]
pub struct UsageArgs {
    /// Time window: `today` (local midnight → now, default) or `all`.
    /// Ignored when `--since` is given.
    #[arg(long, default_value = "today")]
    pub window: String,
    /// Rolling window ending now, e.g. `5h`, `30m`, `2d`. Overrides `--window`.
    #[arg(long)]
    pub since: Option<String>,
    /// Restrict to one tool: `claude` or `codex` (default: both).
    #[arg(long)]
    pub tool: Option<String>,
    /// One-line compact form for a tmux status bar.
    #[arg(long, default_value_t = false)]
    pub oneline: bool,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Show running Claude/Codex agents (shells out to `tmx agents --json`,
    /// the ecosystem's classification source of truth). Runs LOCALLY — no
    /// socket — so it composes into a tmux status bar. `--oneline` for that.
    Status {
        /// One-line compact form for a tmux status bar.
        #[arg(long, default_value_t = false)]
        oneline: bool,
    },
    /// Request user approval for an action (shows dialog, blocks until response)
    Approve {
        /// Dialog message describing the action
        message: String,
        /// Dialog title
        #[arg(long, default_value = "Agent Action")]
        title: String,
        /// Custom button labels (comma-separated, first = approve)
        #[arg(long)]
        actions: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ThemeCommand {
    /// List available themes
    List,
}

#[derive(Subcommand)]
pub enum PluginCommand {
    /// List installed plugins
    List,
    /// Open a plugin panel in a new tab
    Open {
        /// Plugin name
        plugin: String,
        /// Panel name within the plugin
        #[arg(long, default_value = "main")]
        panel: String,
    },
    /// Run a plugin command
    Run {
        /// Command in format: plugin.command (e.g., my-plugin.greet)
        command: String,
        /// JSON params to pass to the command
        #[arg(long, default_value = "{}")]
        params: String,
    },
}

#[derive(Subcommand)]
pub enum StatusBarCommand {
    /// Show the status bar
    Show,
    /// Hide the status bar
    Hide,
    /// Toggle status bar visibility
    Toggle,
}

#[derive(Subcommand)]
pub enum WebviewCommand {
    /// Open a URL in a new webview panel
    Open {
        /// URL to open
        url: String,
        /// Panel mode: tab, split_h, split_v
        #[arg(long, default_value = "tab")]
        mode: String,
    },
    /// Navigate an existing webview to a new URL
    Navigate {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// URL to navigate to
        url: String,
    },
    /// Go back in webview history
    Back {
        /// Panel ID
        #[arg(long)]
        id: String,
    },
    /// Go forward in webview history
    Forward {
        /// Panel ID
        #[arg(long)]
        id: String,
    },
    /// Reload webview
    Reload {
        /// Panel ID
        #[arg(long)]
        id: String,
    },
    /// Execute JavaScript in a webview
    ExecJs {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// JavaScript code to execute
        code: String,
    },
    /// Get page content from a webview
    GetContent {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// Content format: text or html
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Take a screenshot of a webview (returns base64 PNG or saves to file)
    Screenshot {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// Save to file path (omit for base64 in response)
        #[arg(long)]
        path: Option<String>,
    },
    /// Query a single DOM element by CSS selector
    Query {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// CSS selector
        selector: String,
    },
    /// Query all matching DOM elements by CSS selector
    QueryAll {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// CSS selector
        selector: String,
        /// Max results
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Get computed CSS styles for an element
    GetStyles {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// CSS selector
        selector: String,
        /// CSS property names (comma-separated)
        properties: String,
    },
    /// Click a DOM element by CSS selector
    Click {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// CSS selector
        selector: String,
    },
    /// Type text into an input element
    Fill {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// CSS selector for the input element
        selector: String,
        /// Value to type
        value: String,
    },
    /// Scroll to position or element
    Scroll {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// CSS selector to scroll to (overrides x/y)
        #[arg(long)]
        selector: Option<String>,
        /// X scroll position
        #[arg(long, default_value_t = 0)]
        x: i32,
        /// Y scroll position
        #[arg(long, default_value_t = 0)]
        y: i32,
    },
    /// Get page metadata (title, dimensions, element counts)
    PageInfo {
        /// Panel ID
        #[arg(long)]
        id: String,
    },
    /// Toggle DevTools inspector
    Devtools {
        /// Panel ID
        #[arg(long)]
        id: String,
        /// Action: show, close, attach, detach
        #[arg(default_value = "show")]
        action: String,
    },
}

impl Cli {
    pub fn method(&self) -> String {
        match &self.command {
            Command::Ping => "system.ping".to_string(),
            Command::Context { .. } => "context.snapshot".to_string(),
            Command::Presence(cmd) => match cmd {
                PresenceCommand::Away | PresenceCommand::Active => "presence.set",
                PresenceCommand::Status => "presence.get",
            }
            .to_string(),
            Command::Session(cmd) => match cmd {
                SessionCommand::List => "session.list",
                SessionCommand::Info { .. } => "session.info",
            }
            .to_string(),
            Command::Workspace(cmd) => match cmd {
                WorkspaceCommand::New { .. } => "workspace.new",
                WorkspaceCommand::List => "workspace.list",
                WorkspaceCommand::Switch { .. } => "workspace.switch",
                WorkspaceCommand::Cycle => "workspace.cycle",
            }
            .to_string(),
            Command::Background(cmd) => match cmd {
                BackgroundCommand::Set { .. } => "background.set",
                BackgroundCommand::Clear => "background.clear",
                BackgroundCommand::SetTint { .. } => "background.set_tint",
                BackgroundCommand::Next => "background.next",
                BackgroundCommand::Toggle => "background.toggle",
                BackgroundCommand::DeleteCurrent => "background.delete_current",
            }
            .to_string(),
            Command::Tab(cmd) => match cmd {
                TabCommand::New => "tab.new",
                TabCommand::Close => "tab.close",
                TabCommand::List => "tab.list",
                TabCommand::Info => "tab.info",
                TabCommand::Switch { .. } => "tab.switch",
                TabCommand::ToggleBar => "tabs.toggle_bar",
                TabCommand::Rename { .. } => "tab.rename",
            }
            .to_string(),
            Command::Split(cmd) => match cmd {
                SplitCommand::Horizontal => "split.horizontal",
                SplitCommand::Vertical => "split.vertical",
            }
            .to_string(),
            Command::Pane(cmd) => match cmd {
                PaneCommand::FocusNext => "pane.focus_next",
                PaneCommand::FocusPrev => "pane.focus_prev",
            }
            .to_string(),
            Command::Event(cmd) => match cmd {
                EventCommand::Subscribe => "event.subscribe",
                EventCommand::Publish { .. } => "events.publish",
            }
            .to_string(),
            Command::Webview(cmd) => match cmd {
                WebviewCommand::Open { .. } => "webview.open",
                WebviewCommand::Navigate { .. } => "webview.navigate",
                WebviewCommand::Back { .. } => "webview.back",
                WebviewCommand::Forward { .. } => "webview.forward",
                WebviewCommand::Reload { .. } => "webview.reload",
                WebviewCommand::ExecJs { .. } => "webview.execute_js",
                WebviewCommand::GetContent { .. } => "webview.get_content",
                WebviewCommand::Screenshot { .. } => "webview.screenshot",
                WebviewCommand::Query { .. } => "webview.query",
                WebviewCommand::QueryAll { .. } => "webview.query_all",
                WebviewCommand::GetStyles { .. } => "webview.get_styles",
                WebviewCommand::Click { .. } => "webview.click",
                WebviewCommand::Fill { .. } => "webview.fill",
                WebviewCommand::Scroll { .. } => "webview.scroll",
                WebviewCommand::PageInfo { .. } => "webview.page_info",
                WebviewCommand::Devtools { .. } => "webview.devtools",
            }
            .to_string(),
            Command::Terminal(cmd) => match cmd {
                TerminalCommand::Read { .. } => "terminal.read",
                TerminalCommand::State { .. } => "terminal.state",
                TerminalCommand::Exec { .. } => "terminal.exec",
                TerminalCommand::Feed { .. } => "terminal.feed",
                TerminalCommand::History { .. } => "terminal.history",
                TerminalCommand::Context { .. } => "terminal.context",
            }
            .to_string(),
            Command::Agent(cmd) => match cmd {
                AgentCommand::Approve { .. } => "agent.approve",
                AgentCommand::Status { .. } => {
                    unreachable!("agent status is dispatched locally in main.rs")
                }
            }
            .to_string(),
            Command::Usage(_) => unreachable!("usage is dispatched locally in main.rs"),
            Command::Theme(cmd) => match cmd {
                ThemeCommand::List => "theme.list",
            }
            .to_string(),
            Command::Plugin(cmd) => match cmd {
                PluginCommand::List => "plugin.list".to_string(),
                PluginCommand::Open { .. } => "plugin.open".to_string(),
                PluginCommand::Run { command, .. } => format!("plugin.{command}"),
            },
            Command::Project(cmd) => match cmd {
                ProjectCommand::List => "project.list",
                ProjectCommand::Resolve { .. } => "project.resolve",
            }
            .to_string(),
            Command::Workflow(cmd) => match cmd {
                WorkflowCommand::List => "workflow.list",
                WorkflowCommand::Get { .. } => "workflow.get",
                WorkflowCommand::Run { .. } => "workflow.run",
            }
            .to_string(),
            Command::Runledger(cmd) => match cmd {
                RunledgerCommand::Query { .. } => "events.replay",
            }
            .to_string(),
            Command::Statusbar(cmd) => match cmd {
                StatusBarCommand::Show => "statusbar.show",
                StatusBarCommand::Hide => "statusbar.hide",
                StatusBarCommand::Toggle => "statusbar.toggle",
            }
            .to_string(),
            Command::Update(_) => unreachable!("update commands are handled locally"),
            Command::Todo(_) => {
                unreachable!("todo commands are dispatched via plugin_cmds::todo")
            }
            Command::Git(_) => {
                unreachable!("git commands are dispatched via plugin_cmds::git")
            }
            Command::Bookmark(_) => {
                unreachable!("bookmark commands are dispatched via plugin_cmds::bookmark")
            }
            Command::Jira(_) => {
                unreachable!("jira commands are dispatched via plugin_cmds::jira")
            }
            Command::Slack(_) => {
                unreachable!("slack commands are dispatched via plugin_cmds::slack")
            }
            Command::Calendar(_) => {
                unreachable!("calendar commands are dispatched via plugin_cmds::calendar")
            }
            Command::Recent(_) => {
                unreachable!("recent commands are dispatched via plugin_cmds::recent")
            }
            Command::Call { method, .. } => method.clone(),
        }
    }

    pub fn params(&self) -> serde_json::Value {
        match &self.command {
            Command::Ping => json!({}),
            Command::Context { .. } => json!({}),
            Command::Presence(cmd) => match cmd {
                PresenceCommand::Away => json!({ "state": "away" }),
                PresenceCommand::Active => json!({ "state": "active" }),
                PresenceCommand::Status => json!({}),
            },
            Command::Session(cmd) => match cmd {
                SessionCommand::List => json!({}),
                SessionCommand::Info { id } => json!({ "id": id }),
            },
            Command::Workspace(cmd) => match cmd {
                WorkspaceCommand::New { dir } => {
                    // Root the workspace at an ABSOLUTE, existing directory,
                    // resolved against the CALLER's cwd — mirrors `tmux
                    // new-session` inheriting $PWD. Relative `dir` joins the
                    // caller's cwd (never the GUI process's), and a missing /
                    // non-directory target is rejected before we send, so we
                    // never create a workspace rooted at a bad or GUI-relative
                    // path.
                    let cwd = std::env::current_dir().unwrap_or_else(|e| {
                        eprintln!("coctl: cannot read current directory: {e}");
                        std::process::exit(1);
                    });
                    let target = match dir {
                        Some(d) => {
                            let p = std::path::Path::new(d);
                            if p.is_absolute() {
                                p.to_path_buf()
                            } else {
                                cwd.join(p)
                            }
                        }
                        None => cwd,
                    };
                    let abs = target.canonicalize().unwrap_or_else(|_| {
                        eprintln!("coctl: no such directory: {}", target.display());
                        std::process::exit(1);
                    });
                    if !abs.is_dir() {
                        eprintln!("coctl: not a directory: {}", abs.display());
                        std::process::exit(1);
                    }
                    json!({ "workspace": abs.to_string_lossy() })
                }
                WorkspaceCommand::List | WorkspaceCommand::Cycle => json!({}),
                WorkspaceCommand::Switch { id } => json!({ "id": id }),
            },
            Command::Background(cmd) => match cmd {
                BackgroundCommand::Set { path } => {
                    let abs = std::path::Path::new(path)
                        .canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(path));
                    json!({ "path": abs.to_string_lossy() })
                }
                BackgroundCommand::Clear => json!({}),
                BackgroundCommand::SetTint { opacity } => json!({ "opacity": opacity }),
                BackgroundCommand::Next
                | BackgroundCommand::Toggle
                | BackgroundCommand::DeleteCurrent => json!({}),
            },
            Command::Tab(cmd) => match cmd {
                TabCommand::Rename { id, title } => json!({ "id": id, "title": title }),
                TabCommand::Switch { index } => json!({ "index": index }),
                _ => json!({}),
            },
            Command::Pane(_) => json!({}),
            Command::Terminal(cmd) => match cmd {
                TerminalCommand::Read {
                    id,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                } => {
                    let mut p = json!({});
                    if let Some(id) = id {
                        p["id"] = json!(id);
                    }
                    if let Some(sr) = start_row {
                        p["start_row"] = json!(sr);
                        p["start_col"] = json!(start_col.unwrap_or(0));
                        p["end_row"] = json!(end_row.unwrap_or(*sr));
                        p["end_col"] = json!(end_col.unwrap_or(999));
                    }
                    p
                }
                TerminalCommand::State { id } => match id {
                    Some(id) => json!({ "id": id }),
                    None => json!({}),
                },
                TerminalCommand::Exec { id, command } => match id {
                    Some(id) => json!({ "id": id, "command": command }),
                    None => json!({ "command": command }),
                },
                TerminalCommand::Feed { id, text } => match id {
                    Some(id) => json!({ "id": id, "text": text }),
                    None => json!({ "text": text }),
                },
                TerminalCommand::History { id, lines } => {
                    let mut p = json!({ "lines": lines });
                    if let Some(id) = id {
                        p["id"] = json!(id);
                    }
                    p
                }
                TerminalCommand::Context { id, history_lines } => {
                    let mut p = json!({ "history_lines": history_lines });
                    if let Some(id) = id {
                        p["id"] = json!(id);
                    }
                    p
                }
            },
            Command::Agent(cmd) => match cmd {
                AgentCommand::Approve {
                    message,
                    title,
                    actions,
                } => {
                    let mut p = json!({ "message": message, "title": title });
                    if let Some(actions) = actions {
                        let acts: Vec<&str> = actions.split(',').map(|s| s.trim()).collect();
                        p["actions"] = json!(acts);
                    }
                    p
                }
                AgentCommand::Status { .. } => {
                    unreachable!("agent status is dispatched locally in main.rs")
                }
            },
            Command::Usage(_) => unreachable!("usage is dispatched locally in main.rs"),
            Command::Plugin(cmd) => match cmd {
                PluginCommand::List => json!({}),
                PluginCommand::Open { plugin, panel } => {
                    json!({ "plugin": plugin, "panel": panel })
                }
                PluginCommand::Run { params, .. } => {
                    serde_json::from_str(params).unwrap_or_else(|_| json!({}))
                }
            },
            Command::Theme(_)
            | Command::Split(_)
            | Command::Event(_)
            | Command::Update(_)
            | Command::Statusbar(_) => {
                json!({})
            }
            Command::Todo(_) => {
                unreachable!("todo commands are dispatched via plugin_cmds::todo")
            }
            Command::Git(_) => {
                unreachable!("git commands are dispatched via plugin_cmds::git")
            }
            Command::Bookmark(_) => {
                unreachable!("bookmark commands are dispatched via plugin_cmds::bookmark")
            }
            Command::Jira(_) => {
                unreachable!("jira commands are dispatched via plugin_cmds::jira")
            }
            Command::Slack(_) => {
                unreachable!("slack commands are dispatched via plugin_cmds::slack")
            }
            Command::Calendar(_) => {
                unreachable!("calendar commands are dispatched via plugin_cmds::calendar")
            }
            Command::Recent(_) => {
                unreachable!("recent commands are dispatched via plugin_cmds::recent")
            }
            Command::Call { params, .. } => {
                serde_json::from_str(params).unwrap_or_else(|_| json!({}))
            }
            Command::Project(cmd) => match cmd {
                ProjectCommand::List => json!({}),
                ProjectCommand::Resolve {
                    name,
                    cwd,
                    git_remote,
                    active,
                } => {
                    let mut obj = serde_json::Map::new();
                    if let Some(n) = name {
                        obj.insert("name".into(), json!(n));
                    }
                    if let Some(c) = cwd {
                        obj.insert("cwd".into(), json!(c));
                    }
                    if let Some(g) = git_remote {
                        obj.insert("git_remote".into(), json!(g));
                    }
                    if *active {
                        obj.insert("active".into(), json!(true));
                    }
                    serde_json::Value::Object(obj)
                }
            },
            Command::Runledger(cmd) => match cmd {
                RunledgerCommand::Query {
                    since_ms,
                    kinds,
                    limit,
                } => {
                    let mut o = serde_json::Map::new();
                    o.insert("since_ms".into(), json!(since_ms));
                    if let Some(k) = kinds {
                        let arr: Vec<serde_json::Value> = k
                            .split(',')
                            .map(|s| serde_json::Value::String(s.trim().to_string()))
                            .collect();
                        o.insert("kinds".into(), serde_json::Value::Array(arr));
                    }
                    if let Some(l) = limit {
                        o.insert("limit".into(), json!(l));
                    }
                    serde_json::Value::Object(o)
                }
            },
            Command::Workflow(cmd) => match cmd {
                WorkflowCommand::List => json!({}),
                WorkflowCommand::Get { id } => json!({ "id": id }),
                WorkflowCommand::Run {
                    id,
                    project,
                    values,
                    kv,
                } => {
                    let mut obj = serde_json::Map::new();
                    obj.insert("id".into(), json!(id));
                    if let Some(p) = project {
                        obj.insert("project".into(), json!(p));
                    }
                    // `--values <json>` precedence; falls back to building
                    // the object from repeated `--value name=val` flags
                    // (per codex-plan Q8). Both omitted → empty object.
                    let values_obj: serde_json::Value = if let Some(s) = values {
                        match serde_json::from_str::<serde_json::Value>(s) {
                            Ok(v) if v.is_object() => v,
                            Ok(_) => {
                                eprintln!(
                                    "warn: --values must be a JSON object; falling back to --value kv pairs"
                                );
                                kv_to_object(kv)
                            }
                            Err(e) => {
                                eprintln!(
                                    "warn: --values JSON parse error ({e}); falling back to --value kv pairs"
                                );
                                kv_to_object(kv)
                            }
                        }
                    } else {
                        kv_to_object(kv)
                    };
                    obj.insert("values".into(), values_obj);
                    serde_json::Value::Object(obj)
                }
            },
            Command::Webview(cmd) => match cmd {
                WebviewCommand::Open { url, mode } => json!({ "url": url, "mode": mode }),
                WebviewCommand::Navigate { id, url } => json!({ "id": id, "url": url }),
                WebviewCommand::Back { id } => json!({ "id": id }),
                WebviewCommand::Forward { id } => json!({ "id": id }),
                WebviewCommand::Reload { id } => json!({ "id": id }),
                WebviewCommand::ExecJs { id, code } => json!({ "id": id, "code": code }),
                WebviewCommand::GetContent { id, format } => json!({ "id": id, "format": format }),
                WebviewCommand::Screenshot { id, path } => json!({ "id": id, "path": path }),
                WebviewCommand::Query { id, selector } => json!({ "id": id, "selector": selector }),
                WebviewCommand::QueryAll {
                    id,
                    selector,
                    limit,
                } => json!({ "id": id, "selector": selector, "limit": limit }),
                WebviewCommand::GetStyles {
                    id,
                    selector,
                    properties,
                } => {
                    let props: Vec<&str> = properties.split(',').map(|s| s.trim()).collect();
                    json!({ "id": id, "selector": selector, "properties": props })
                }
                WebviewCommand::Click { id, selector } => json!({ "id": id, "selector": selector }),
                WebviewCommand::Fill {
                    id,
                    selector,
                    value,
                } => json!({ "id": id, "selector": selector, "value": value }),
                WebviewCommand::Scroll { id, selector, x, y } => {
                    json!({ "id": id, "selector": selector, "x": x, "y": y })
                }
                WebviewCommand::PageInfo { id } => json!({ "id": id }),
                WebviewCommand::Devtools { id, action } => json!({ "id": id, "action": action }),
            },
        }
    }
}
