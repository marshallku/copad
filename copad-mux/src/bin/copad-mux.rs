//! `copad-mux` — the terminal multiplexer.
//!
//!   copad-mux              run the TUI (a multi-pane shell workspace)
//!   copad-mux ctl <cmd>    control a running instance over its socket:
//!                            ctl list
//!                            ctl split -h|-v
//!                            ctl focus <index>
//!                            ctl close <index>
//!                            ctl send  <index> <text...>

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("ctl") => std::process::exit(copad_mux::control::run_client(&args[1..])),
        Some("run") | None => {
            if let Err(e) = copad_mux::tui::run() {
                eprintln!("copad-mux: {e}");
                std::process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("copad-mux: unknown command '{other}' (try: run | ctl)");
            std::process::exit(2);
        }
    }
}
