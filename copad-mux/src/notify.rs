//! Desktop notifications for agent turn events — the piece that lets the scattered
//! `~/.claude` notify hooks (notify-stop/notification/attention.sh) be retired: the
//! server watches each agent's status TRANSITIONS itself (no Claude hook needed) and
//! fires a native toast. Best-effort + non-blocking; the server fires it, so toasts
//! arrive even while detached.
//!
//! A toast can carry an **action**: a shell command run when the user clicks it, which
//! the caller sets to `comux jump <pane-token>` so the click lands in the pane that
//! raised the notification (macOS `terminal-notifier -execute`, Linux `dunstify
//! --action` / `notify-send --action`).
//!
//! Two environment notes, both load-bearing:
//!
//! * The server SCRUBS `DISPLAY`/`XAUTHORITY`/`DBUS_SESSION_BUS_ADDRESS`/… out of its
//!   own environment at startup (tmux `update-environment`), so a notifier spawned with
//!   the bare daemon environment has no session bus to publish to — on Linux that is the
//!   difference between a toast and silence. Callers pass the attached client's refreshed
//!   session vars as `env`.
//! * The action command is run by whatever we spawn here (the `dunstify` waiter, or
//!   `terminal-notifier`), so it is a child of the SERVER, not of the notification
//!   daemon. It therefore needs the same `env` carried explicitly — which is why the
//!   command is built as `env K=V … comux jump …` by [`action_command`].
//!
//! Opt out with `COPAD_MUX_NOTIFY=0` (or `off`/`false`).

use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// The `COPAD_MUX_NOTIFY` override, if set to a RECOGNIZED value: `Some(false)` for
/// `0`/`off`/`false`/`no`, `Some(true)` for `1`/`on`/`true`/`yes`, `None` otherwise (unset
/// or unrecognized). The env value takes precedence over the config `notify` flag; when it
/// is `None` the caller falls back to config (see `MuxConfig::notify`).
pub fn env_override() -> Option<bool> {
    match std::env::var("COPAD_MUX_NOTIFY").ok().as_deref() {
        Some("0") | Some("off") | Some("false") | Some("no") => Some(false),
        Some("1") | Some("on") | Some("true") | Some("yes") => Some(true),
        _ => None,
    }
}

/// Is desktop notification enabled? (Default on; the whole point is to replace the
/// hook-based notifier.) Env-only; config-aware callers compute
/// `env_override().unwrap_or(cfg.notify)` and gate before calling [`desktop`].
pub fn enabled() -> bool {
    env_override().unwrap_or(true)
}

/// Spawn a command detached (stdio nulled) and REAP it in a short-lived thread so a
/// long-lived server never accumulates zombie notifier processes. Returns whether it
/// spawned (so the caller can fall back).
#[allow(unused)]
fn spawn_reaped(mut cmd: Command) -> bool {
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// POSIX single-quote one argument for embedding in a shell command line: wrap in `'…'`
/// and turn any embedded `'` into `'\''`. The action command is handed to a shell by both
/// notifier backends, and its pieces include user-controlled data (a `COPAD_MUX_SOCK`
/// path), so every element goes through this.
pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Build the shell command a toast click should run: `env K=V … <exe> jump <target>`.
///
/// `env` carries the session variables the server no longer holds (see the module note);
/// entries whose NAME is not a plain `[A-Za-z_][A-Za-z0-9_]*` are dropped rather than
/// quoted, since `env` would treat them as a command instead of an assignment.
pub fn action_command(exe: &str, target: &str, env: &[(String, String)]) -> String {
    let mut parts = vec!["env".to_string()];
    for (k, v) in env {
        if !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit())
        {
            parts.push(shell_quote(&format!("{k}={v}")));
        }
    }
    parts.push(shell_quote(exe));
    parts.push("jump".to_string());
    parts.push(shell_quote(target));
    parts.join(" ")
}

/// Does the installed `notify-send` understand `--action`? libnotify gained it in 0.8;
/// older builds ERROR OUT on the flag, which would swap a plain toast for no toast at
/// all. Probed once and cached — the answer cannot change while the server runs.
#[cfg(target_os = "linux")]
fn notify_send_has_actions() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        Command::new("notify-send")
            .arg("--help")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| {
                let text = String::from_utf8_lossy(&o.stdout);
                text.contains("--action")
            })
            .unwrap_or(false)
    })
}

/// Run a notifier that BLOCKS until the toast is clicked or dismissed, printing the
/// chosen action id on stdout, and run `action` through `sh -c` when it printed one.
/// The wait happens on its own thread so the server's single-writer loop never parks.
#[cfg(target_os = "linux")]
fn wait_for_action(mut cmd: Command, action: String, env: Vec<(String, String)>) -> bool {
    let spawned = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(child) => {
            std::thread::spawn(move || {
                let Ok(out) = child.wait_with_output() else {
                    return;
                };
                // Dismissed (or timed out) → no action id on stdout → nothing to do.
                if String::from_utf8_lossy(&out.stdout).trim() != "default" {
                    return;
                }
                let mut run = Command::new("sh");
                run.args(["-c", &action]);
                for (k, v) in &env {
                    run.env(k, v);
                }
                spawn_reaped(run);
            });
            true
        }
        Err(_) => false,
    }
}

/// Fire a native desktop toast (best-effort, non-blocking).
///
/// `action`, when set, is a shell command run if the user clicks the notification.
/// `env` is overlaid on every spawned process (see the module note on the scrubbed
/// daemon environment).
///
/// macOS: `terminal-notifier` (with the `Glass` sound, matching the retired hook) →
/// `osascript` fallback (no action support). Linux: `dunstify` → `notify-send`
/// (`--action` only when the installed libnotify supports it).
pub fn desktop(title: &str, body: &str, action: Option<&str>, env: &[(String, String)]) {
    if !enabled() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let mut tn = Command::new("terminal-notifier");
        tn.args(["-title", title, "-message", body, "-sound", "Glass"]);
        if let Some(cmd) = action {
            tn.args(["-execute", cmd]);
        }
        for (k, v) in env {
            tn.env(k, v);
        }
        if !spawn_reaped(tn) {
            // Escape embedded quotes for the AppleScript string literals.
            let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                esc(body),
                esc(title)
            );
            let mut os = Command::new("osascript");
            os.args(["-e", &script]);
            for (k, v) in env {
                os.env(k, v);
            }
            spawn_reaped(os);
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(cmd) = action {
            let mut dn = Command::new("dunstify");
            dn.args([
                "-a",
                "copad-mux",
                "--action=default,Jump to pane",
                title,
                body,
            ]);
            for (k, v) in env {
                dn.env(k, v);
            }
            if wait_for_action(dn, cmd.to_string(), env.to_vec()) {
                return;
            }
            if notify_send_has_actions() {
                // NOTE the `=` where dunstify takes a `,`: libnotify's notify-send parses
                // `--action=[NAME=]TEXT`, so the comma form would name the action `0` and
                // the waiter — which only honors `default` — would never fire the jump.
                let mut ns = Command::new("notify-send");
                ns.args([
                    "-a",
                    "copad-mux",
                    "--action=default=Jump to pane",
                    title,
                    body,
                ]);
                for (k, v) in env {
                    ns.env(k, v);
                }
                if wait_for_action(ns, cmd.to_string(), env.to_vec()) {
                    return;
                }
            }
        }
        // No action wanted, or no backend that supports one: plain toast.
        let mut ns = Command::new("notify-send");
        ns.args(["-a", "copad-mux", title, body]);
        for (k, v) in env {
            ns.env(k, v);
        }
        spawn_reaped(ns);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, body, action, env);
    }
}

#[cfg(test)]
mod tests {
    use super::{action_command, enabled, shell_quote};

    #[test]
    fn env_gating() {
        // Serialized within one test to avoid cross-test env races.
        for (val, want) in [
            ("0", false),
            ("off", false),
            ("false", false),
            ("no", false),
            ("1", true),
            ("", true),
        ] {
            unsafe { std::env::set_var("COPAD_MUX_NOTIFY", val) };
            assert_eq!(enabled(), want, "COPAD_MUX_NOTIFY={val:?}");
        }
        unsafe { std::env::remove_var("COPAD_MUX_NOTIFY") };
        assert!(enabled(), "default (unset) is enabled");
    }

    #[test]
    fn quotes_neutralize_shell_metacharacters() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b;rm -rf /"), "'a b;rm -rf /'");
        // The one character single-quoting cannot contain must be broken out.
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    }

    /// The click command is handed to a shell, and the socket path is user-controlled
    /// (`COPAD_MUX_SOCK`) — a path with a quote must not end the quoted string.
    #[test]
    fn action_command_quotes_every_element() {
        let env = vec![(
            "COPAD_MUX_SOCK".to_string(),
            "/tmp/x'; touch /tmp/pwned; '".to_string(),
        )];
        let cmd = action_command("/usr/local/bin/comux", "a1b2-7", &env);
        assert!(!cmd.contains("touch /tmp/pwned; '\""), "{cmd}");
        assert!(
            cmd.starts_with("env 'COPAD_MUX_SOCK=/tmp/x'\\''; touch"),
            "{cmd}"
        );
        assert!(
            cmd.ends_with("'/usr/local/bin/comux' jump 'a1b2-7'"),
            "{cmd}"
        );
    }

    /// `env` parses `K=V` positionally: a name that isn't a valid identifier would be
    /// taken as the COMMAND to run, so those entries are dropped instead.
    #[test]
    fn action_command_drops_unusable_env_names() {
        let env = vec![
            ("BASH_FUNC_x%%".to_string(), "() { :; }".to_string()),
            ("9BAD".to_string(), "v".to_string()),
            ("".to_string(), "v".to_string()),
            ("GOOD_1".to_string(), "v".to_string()),
        ];
        let cmd = action_command("comux", "t", &env);
        assert!(cmd.contains("'GOOD_1=v'"), "{cmd}");
        assert!(!cmd.contains("BASH_FUNC"), "{cmd}");
        assert!(!cmd.contains("9BAD"), "{cmd}");
        assert_eq!(cmd.matches('=').count(), 1, "{cmd}");
    }
}
