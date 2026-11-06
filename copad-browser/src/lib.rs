//! copad's browser core (decisions #210): the rules under the webview panes (`browser`), Design
//! Grab's trusted half (`design_grab`), and their C ABI for Swift (`ffi`). copad-core re-exports
//! the modules at their old paths; AIDE links them through its `ios/native` crate.
//!
//! The host owns where state lives: call [`browser::set_state_root`] before anything that touches
//! history, logs, credentials or profiles.

pub mod browser;
pub mod design_grab;
pub mod ffi;
pub mod fs_atomic;
pub mod protocol;
