mod app;
mod background;
mod browser;
mod cockpit_panel;
mod command_palette;
mod comux_graph;
mod comux_nav;
mod comux_overlay;
mod comux_panel;
mod daemon_forward;
mod gui_client;
mod image_layers;
mod image_shadow;
mod kitty_keys;
mod kitty_relay;
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

fn main() {
    // `warn` keeps a no-daemon launch silent on stderr; RUST_LOG=info / debug surfaces
    // gui_client register/reconnect diagnostics.
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
