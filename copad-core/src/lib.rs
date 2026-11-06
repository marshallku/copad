pub mod action_registry;
pub mod background;
pub mod browser;
pub mod condition;
pub mod config;
pub mod context;
pub mod design_grab;
pub mod error;
pub mod event_bus;
pub mod fs_atomic;
pub mod notifier;
pub mod paths;
pub mod plugin;
pub mod project;
pub mod protocol;
pub mod runledger;
pub mod session;
pub mod theme;
pub mod thread_pool;
pub mod trigger;
pub mod workflow;

// Moved to the comux-host crate (shared with AIDE); the paths stay.
pub use comux_host::{agent_cockpit, graph_bridge, muxhost};
