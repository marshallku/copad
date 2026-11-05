//! copad-mux — the agent-orchestration terminal multiplexer.

pub mod agentlog;
pub mod agentpoll;
pub mod agentsessions;
pub mod agentstate;
pub mod cliargs;
pub mod client;
pub mod codexdaemon;
pub mod compactgate;
pub mod config;
pub mod control;
pub mod copadlink;
pub mod dirbrowse;
pub mod doctor;
pub mod fdlimit;
pub mod fleet;
pub mod gitinfo;
pub mod gitpoll;
pub mod graphpoll;
pub mod hostapi;
pub mod hostmetrics;
pub mod images;
pub mod keys;
pub mod layoutfile;
pub mod model;
pub mod navhist;
pub mod notify;
pub mod persist;
pub mod picker;
pub mod procinfo;
pub mod proto;
pub mod push;
pub mod selector;
pub mod server;
pub mod shellcmd;
pub mod state;
pub mod term;
pub mod tui;
pub mod usagepoll;
pub mod versionpoll;
pub mod winfocus;
pub mod workgraph;
pub mod worktree;

pub use model::{
    AgentState, ClientId, Dir, PaneId, Rect, Role, SplitTree, Tab, TabId, Terminal, TerminalId,
    Workspace, WorkspaceId,
};
pub use state::{Command, Event, MuxError, Origin, State};
