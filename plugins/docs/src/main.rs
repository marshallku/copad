//! `copad-plugin-docs` binary scaffold. v1 is panel-only: the docs panel HTML is the whole
//! surface and data calls go through the kb plugin's actions (`kb.search` / `kb.read` /
//! `kb.list`), so there is no service to register and the binary exits if launched as one.
//! `scripts/install-plugins.sh` sees the missing `[[services]]` block in `plugin.toml` and skips
//! symlinking the binary.

fn main() {
    eprintln!(
        "copad-plugin-docs: panel-only at v1 — data access routes through kb.* actions. \
         If you reached this through the supervisor, plugin.toml is misconfigured \
         (it should have no [[services]] block until a frontmatter cache lands)."
    );
    std::process::exit(0);
}
