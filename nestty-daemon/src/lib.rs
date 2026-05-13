//! nestty headless daemon library.
//!
//! v0 scaffolding only: socket listen + ping. Step 2 of the daemon-first
//! migration per `docs/harness-integration.md` § Migration path. Subsequent
//! commits will relocate `service_supervisor`, `trigger_sink`, and the
//! daemon-owned half of `socket.rs` from `nestty-linux`.
//!
//! Wire protocol invariants are spelled out in `docs/gui-daemon-protocol.md`.

pub mod service_supervisor;
pub mod socket;
pub mod trigger_sink;
