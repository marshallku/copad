//! Focus a copad TAB directly, for the notification jump path.
//!
//! [`crate::winfocus`] can only activate an *application* — a pid names the emulator, not one
//! of its windows or tabs. For most terminals that is the ceiling, and #102 documented it as
//! best-effort rather than papering over it. Copad is the exception: it exports `COPAD_SOCKET`
//! (its per-instance control socket) and `COPAD_PANEL_ID` (the pane holding the shell) into
//! every tab's environment, so a comux client running inside one can say EXACTLY which tab it
//! occupies. This asks copad to focus that tab and come forward.
//!
//! Best-effort by construction, and quiet about it: every failure — no socket, a dead copad, a
//! panel that belongs to a different copad instance — returns `false` so the caller falls back
//! to application-level activation. The alternative, guessing at a window, is the hacky
//! behaviour this exists to avoid.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The GUI answers immediately or not at all; a click should never hang on it.
const TIMEOUT: Duration = Duration::from_millis(1500);

/// Ask the copad instance at `sock` to focus `panel_id` and raise itself.
///
/// Returns whether copad reported that it actually focused the panel. A well-formed call that
/// found no such panel returns `false`, NOT an error: the panel may belong to another copad
/// instance, and the caller needs that apart from a broken request so it can fall back.
pub fn focus_panel(sock: &str, panel_id: &str) -> bool {
    let Ok(stream) = UnixStream::connect(sock) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let req = serde_json::json!({
        "id": "comux-jump",
        "method": "panel.focus",
        "params": { "panel_id": panel_id },
    });
    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return false,
    };
    if writeln!(w, "{req}").is_err() || w.flush().is_err() {
        return false;
    }
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return false;
    }
    parse_focus_reply(&line)
}

/// Whether a `panel.focus` reply says the panel was focused.
///
/// Split out so the contract is testable without a running copad: the reply carries BOTH an
/// envelope `ok` and a payload `focused`, and only the second answers "did the user end up
/// looking at the right tab".
pub fn parse_focus_reply(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    // `result.focused` when the GUI wraps its payload, else a bare `focused`.
    v.pointer("/result/focused")
        .or_else(|| v.get("focused"))
        .and_then(|f| f.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_reported_focus_counts_as_one() {
        assert!(parse_focus_reply(
            r#"{"id":"x","ok":true,"result":{"focused":true}}"#
        ));
        assert!(parse_focus_reply(r#"{"focused":true}"#));
        // The call succeeded but nothing was focused — this copad does not own that panel.
        // It must NOT read as success, or the caller skips its fallback and the user is left
        // looking at the wrong window.
        assert!(!parse_focus_reply(
            r#"{"id":"x","ok":true,"result":{"focused":false}}"#
        ));
        // An `ok` envelope with no payload says nothing about focus.
        assert!(!parse_focus_reply(r#"{"id":"x","ok":true}"#));
        assert!(!parse_focus_reply(
            r#"{"id":"x","ok":false,"error":"nope"}"#
        ));
        assert!(!parse_focus_reply("not json"));
        assert!(!parse_focus_reply(""));
    }

    #[test]
    fn an_unreachable_copad_is_a_quiet_false() {
        // The whole point: a dead socket must not panic, hang or print — it must just let the
        // caller fall back to activating the application.
        assert!(!focus_panel("/nonexistent/copad-gui.sock", "panel-1"));
    }
}
