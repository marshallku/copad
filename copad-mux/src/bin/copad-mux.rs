//! `copad-mux` — the terminal multiplexer.
//!
//!   copad-mux              attach a client (spawning the server if needed)
//!   copad-mux attach       same as bare invocation
//!   copad-mux server       run the headless server in the foreground
//!   copad-mux ctl <cmd>    control the running server over its socket:
//!                            ctl list | list-tabs | new-tab | select-tab <i>
//!                            ctl split -h|-v
//!                            ctl focus <index> | close <index>
//!                            ctl send  <index> <text...>
//!                            ctl kill-server
//!
//! The server holds the shells; the client renders + forwards input and can detach
//! (`Ctrl-b d`) / reattach, so a session survives the terminal that launched it.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(|s| s.as_str()) {
        Some("ctl") => std::process::exit(copad_mux::control::run_client(&args[1..])),
        Some("server") => copad_mux::server::run(),
        Some("attach") | Some("run") | None => copad_mux::client::run(),
        Some(other) => {
            eprintln!("copad-mux: unknown command '{other}' (try: attach | server | ctl)");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("copad-mux: {e}");
        std::process::exit(1);
    }
}
