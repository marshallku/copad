//! `comux` — the copad terminal multiplexer.
//!
//!   comux                    attach a client (spawning the server if needed)
//!   comux attach             same as bare invocation
//!   comux server             run the headless server in the foreground
//!   comux ctl <cmd> …        control the running server (explicit form)
//!   comux <cmd> …            shorthand: any other verb is a control command, so
//!                            `comux new-session work` == `comux ctl new-session work`
//!
//! Control commands: list | split | resize | focus | close | send | list-tabs | new-tab |
//! select-tab | list-sessions | new-session [name] | rename-session | select-session |
//! kill-server.
//!
//! The server holds the shells; the client renders + forwards input and can detach
//! (`Ctrl-b d`) / reattach, so a session survives the terminal that launched it.

fn print_usage() {
    eprintln!(
        "comux — copad terminal multiplexer\n\
         \n\
         usage:\n\
         \x20 comux                       attach (spawns the server if needed)\n\
         \x20 comux server                run the headless server in the foreground\n\
         \x20 comux <cmd> [args]          run a control command (shorthand for `comux ctl <cmd>`)\n\
         \x20 comux ctl <cmd> [args]      run a control command (explicit)\n\
         \n\
         common commands:\n\
         \x20 comux new-session [name]    create a session (optionally named)\n\
         \x20 comux list-sessions         list sessions\n\
         \x20 comux select-session <i>    switch to a session\n\
         \x20 comux new-tab               create a tab\n\
         \x20 comux split -h|-v           split the focused pane\n\
         \x20 comux kill-server           stop the server\n\
         \n\
         inside the TUI: Ctrl-b C new session (name prompt) · Ctrl-b c new tab · \
         Ctrl-b % / \" split"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(|s| s.as_str()) {
        Some("ctl") => std::process::exit(copad_mux::control::run_client(&args[1..])),
        Some("server") => copad_mux::server::run(),
        Some("attach") | Some("run") | None => copad_mux::client::run(),
        Some("help" | "-h" | "--help") => {
            print_usage();
            std::process::exit(0);
        }
        // Any other verb is a shorthand control command (tmux-style: `comux new-session`).
        Some(_) => std::process::exit(copad_mux::control::run_client(&args)),
    };
    if let Err(e) = result {
        eprintln!("comux: {e}");
        std::process::exit(1);
    }
}
