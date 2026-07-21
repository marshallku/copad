//! copad-mux — the agent-orchestration terminal multiplexer.
//!
//! This crate starts at the crux the spec (`docs/agent-mux-spec.md`) and codex
//! flagged as the real risk: the **authoritative state model** — one single-writer
//! actor whose control-lease, geometry, and mutation rules are deterministic under
//! concurrent, differently-sized, mutating clients. PTY hosting, the render
//! protocol, the ratatui client, and the CLI are layered on top in later slices.
//!
//! Foundation only for now: [`model`] (data + tree helpers) and [`state`] (the
//! actor + `Command`/`Event` + invariant checks). No I/O, no PTYs — so the state
//! machines are unit- and property-testable in isolation.

pub mod model;
pub mod state;

pub use model::{
    AgentState, ClientId, Dir, PaneId, Rect, Role, SplitTree, Tab, TabId, Terminal, TerminalId,
    Workspace, WorkspaceId,
};
pub use state::{Command, Event, MuxError, Origin, State};
