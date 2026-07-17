mod app;
mod background;
mod cockpit_panel;
mod command_palette;
mod daemon_forward;
mod gui_client;
mod panel;
mod plugin_panel;
mod search;
mod session;
mod socket;
mod split;
mod statusbar;
mod tabs;
mod terminal;
mod url_click;
mod webview;
mod window;

// service_supervisor + trigger_sink live in `copad-daemon`; this crate
// imports them via `copad_daemon::{...}` and `crate::socket` re-exports
// the shared transport types.

fn main() {
    // Default to `warn` so a no-daemon launch is silent on stderr;
    // RUST_LOG=info / debug surfaces gui_client register/reconnect
    // diagnostics and other log:: messages when needed.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("copad {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.iter().any(|a| a == "--init-config") {
        match copad_core::config::CopadConfig::write_default() {
            Ok(path) => {
                println!("Config written to: {}", path.display());
                return;
            }
            Err(e) => {
                eprintln!("Failed to write config: {e}");
                std::process::exit(1);
            }
        }
    }

    if args.iter().any(|a| a == "--config-path") {
        println!(
            "{}",
            copad_core::config::CopadConfig::config_path().display()
        );
        return;
    }

    app::run();
}
