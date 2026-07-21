//! `copad-mux` — the terminal multiplexer front-end. Work-unit 2 runs a single
//! pane (one shell in a ratatui TUI); splits, sidebar, server, and CLI follow.

fn main() {
    if let Err(e) = copad_mux::tui::run() {
        eprintln!("copad-mux: {e}");
        std::process::exit(1);
    }
}
