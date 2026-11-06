//! The host side of comux's L0 API (decisions #166): what a GUI drawing comux's chrome natively
//! keeps — the subscription and claims (`muxhost`), the work graph's data for the page
//! (`graph_bridge`) and the agents list (`agent_cockpit`) — plus the C ABI for Swift (`ffi`).
//! copad-core re-exports the modules; AIDE links them through its `ios/native` crate.

pub mod agent_cockpit;
pub mod ffi;
pub mod graph_bridge;
pub mod muxhost;
