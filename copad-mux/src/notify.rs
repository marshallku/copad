//! Desktop notifications for agent turn events — the piece that lets the scattered
//! `~/.claude` notify hooks (notify-stop/notification/attention.sh) be retired: the
//! server watches each agent's status TRANSITIONS itself (no Claude hook needed) and
//! fires a native toast. Best-effort + non-blocking; the server fires it, so toasts
//! arrive even while detached.
//!
//! Opt out with `COPAD_MUX_NOTIFY=0` (or `off`/`false`).

use std::process::{Command, Stdio};

/// Is desktop notification enabled? (Default on; the whole point is to replace the
/// hook-based notifier.)
pub fn enabled() -> bool {
    !matches!(
        std::env::var("COPAD_MUX_NOTIFY").ok().as_deref(),
        Some("0") | Some("off") | Some("false") | Some("no")
    )
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

/// Fire a native desktop toast (best-effort, non-blocking). macOS: `terminal-notifier`
/// (with the `Glass` sound, matching the retired hook) → `osascript` fallback. Linux:
/// `notify-send`.
pub fn desktop(title: &str, body: &str) {
    if !enabled() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let mut tn = Command::new("terminal-notifier");
        tn.args(["-title", title, "-message", body, "-sound", "Glass"]);
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
            spawn_reaped(os);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut ns = Command::new("notify-send");
        ns.args(["-a", "copad-mux", title, body]);
        spawn_reaped(ns);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, body);
    }
}

#[cfg(test)]
mod tests {
    use super::enabled;

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
}
