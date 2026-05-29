//! Phase 22.2 — `copad-plugin-projects` binary scaffold.
//!
//! v1 ships as panel-only — the projects panel HTML is the entire
//! user-facing surface, and every `project.*` / `workflow.*` action is
//! registered GUI-side (`copad-linux/src/window.rs` for the registry
//! handlers, `copad-linux/src/socket.rs` for `workflow.run`). There is
//! no service for this binary to register, so it exits immediately if
//! launched as a service.
//!
//! The crate exists so future per-plugin services (e.g., a durable
//! run-cache for the recent-runs panel slot once Phase 22.6's
//! runledger ships) have a place to land without restructuring.
//! Today, the panel-only path in `scripts/install-plugins.sh:78`
//! recognizes the missing `[[services]]` block in `plugin.toml` and
//! skips binary symlinking entirely.

fn main() {
    eprintln!(
        "copad-plugin-projects: panel-only at v1 — actions live GUI-side. \
         If you reached this through the supervisor, plugin.toml is misconfigured \
         (it should have no [[services]] block until the durable run-cache lands)."
    );
    std::process::exit(0);
}
