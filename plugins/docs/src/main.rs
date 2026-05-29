//! Phase 22.3 — `copad-plugin-docs` binary scaffold.
//!
//! v1 ships as panel-only — the docs panel HTML is the entire
//! user-facing surface, and every data call routes through the kb
//! plugin's existing actions (`kb.search` / `kb.read` / `kb.list`).
//! There is no service for this binary to register, so it exits
//! immediately if launched as a service.
//!
//! The crate exists so future per-plugin services (e.g., a long-running
//! frontmatter index cache once dn graph deltas surface as events) have
//! a place to land without restructuring. Today, the panel-only path in
//! `scripts/install-plugins.sh` recognizes the missing `[[services]]`
//! block in `plugin.toml` and skips binary symlinking entirely.

fn main() {
    eprintln!(
        "copad-plugin-docs: panel-only at v1 — data access routes through kb.* actions. \
         If you reached this through the supervisor, plugin.toml is misconfigured \
         (it should have no [[services]] block until a frontmatter cache lands)."
    );
    std::process::exit(0);
}
