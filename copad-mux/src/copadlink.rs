//! Focus a copad TAB directly, for the notification jump path.
//!
//! [`crate::winfocus`] can only activate an *application* — a pid names the emulator, not one
//! of its windows or tabs. For most terminals that is the ceiling, and #102 documented it as
//! best-effort rather than papering over it. Copad is the exception: it exports `COPAD_SOCKET`
//! (its per-instance control socket) and `COPAD_PANEL_ID` (the pane holding the shell) into
//! every tab's environment, so a comux client running inside one can say EXACTLY which tab it
//! occupies. This asks copad to focus that tab and, where it can, come forward.
//!
//! Best-effort by construction, and quiet about it: every failure — no socket, a dead copad, a
//! panel that belongs to a different copad instance — reports nothing focused, so the caller
//! falls back to application-level activation. The alternative, guessing at a window, is the
//! hacky behaviour this exists to avoid.
//!
//! Focusing the tab and raising the window are reported SEPARATELY, because on Linux they come
//! apart: GTK under a Wayland compositor switches the tab fine but cannot raise itself — a
//! process holding no activation token has its `present()` accepted and ignored. So the copad
//! path owns the tab, and [`crate::winfocus`] still owns the raise whenever copad says it could
//! not do it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The GUI answers immediately or not at all; a click should never hang on it.
const TIMEOUT: Duration = Duration::from_millis(1500);

/// What a copad reported about a `panel.focus` call. Two separate outcomes, because a GUI can
/// switch to the right tab without being able to bring its window forward — which is exactly
/// what GTK does under a Wayland compositor: a process with no activation token cannot raise
/// itself, so `present()` is accepted and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusOutcome {
    /// copad found the panel and switched to its tab.
    pub focused: bool,
    /// copad also brought its window forward, so the generic raise is unnecessary.
    pub raised: bool,
}

impl FocusOutcome {
    /// Nothing happened — no socket, a dead copad, a panel owned by another instance.
    const NONE: Self = Self {
        focused: false,
        raised: false,
    };
}

/// Ask the copad instance at `sock` to focus `panel_id` and, if it can, raise itself.
///
/// A well-formed call that found no such panel reports `focused: false`, NOT an error: the
/// panel may belong to another copad instance, and the caller needs that apart from a broken
/// request so it can fall back.
pub fn focus_panel(sock: &str, panel_id: &str) -> FocusOutcome {
    let Ok(stream) = UnixStream::connect(sock) else {
        return FocusOutcome::NONE;
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
        Err(_) => return FocusOutcome::NONE,
    };
    if writeln!(w, "{req}").is_err() || w.flush().is_err() {
        return FocusOutcome::NONE;
    }
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return FocusOutcome::NONE;
    }
    parse_focus_reply(&line)
}

/// What a `panel.focus` reply reports.
///
/// Split out so the contract is testable without a running copad: the reply carries BOTH an
/// envelope `ok` and a payload `focused`, and only the second answers "did the user end up
/// looking at the right tab".
///
/// **A missing `raised` means `true`.** Every copad built before the field existed raised as a
/// side effect of focusing (macOS `NSApp.activate`), so defaulting it to false would make this
/// caller re-raise on a path that already worked — on macOS an extra `osascript` and possibly a
/// TCC prompt (#102). A GUI that cannot raise says so explicitly.
pub fn parse_focus_reply(line: &str) -> FocusOutcome {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return FocusOutcome::NONE;
    };
    // `result.<field>` when the GUI wraps its payload, else a bare `<field>`.
    let field = |name: &str| {
        v.pointer(&format!("/result/{name}"))
            .or_else(|| v.get(name))
            .and_then(|f| f.as_bool())
    };
    let focused = field("focused").unwrap_or(false);
    FocusOutcome {
        focused,
        raised: focused && field("raised").unwrap_or(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_reported_focus_counts_as_one() {
        let hit = parse_focus_reply(r#"{"id":"x","ok":true,"result":{"focused":true}}"#);
        assert!(hit.focused);
        assert!(parse_focus_reply(r#"{"focused":true}"#).focused);
        // The call succeeded but nothing was focused — this copad does not own that panel.
        // It must NOT read as success, or the caller skips its fallback and the user is left
        // looking at the wrong window.
        assert!(!parse_focus_reply(r#"{"id":"x","ok":true,"result":{"focused":false}}"#).focused);
        // An `ok` envelope with no payload says nothing about focus.
        assert!(!parse_focus_reply(r#"{"id":"x","ok":true}"#).focused);
        assert!(!parse_focus_reply(r#"{"id":"x","ok":false,"error":"nope"}"#).focused);
        assert!(!parse_focus_reply("not json").focused);
        assert!(!parse_focus_reply("").focused);
    }

    #[test]
    fn a_gui_that_cannot_raise_itself_says_so() {
        // GTK under Wayland: the tab switch lands, the window does not come forward.
        let gtk = parse_focus_reply(r#"{"ok":true,"result":{"focused":true,"raised":false}}"#);
        assert_eq!(
            gtk,
            FocusOutcome {
                focused: true,
                raised: false
            }
        );
        // A copad built before the field existed raised as a side effect of focusing, so a
        // MISSING `raised` must not make this caller re-raise a window that already came
        // forward — on macOS that is an extra osascript and possibly a TCC prompt.
        assert!(parse_focus_reply(r#"{"ok":true,"result":{"focused":true}}"#).raised);
        assert!(parse_focus_reply(r#"{"ok":true,"result":{"focused":true,"raised":true}}"#).raised);
        // Nothing was focused, so nothing was raised either, whatever the reply claims: there
        // is no window to have been brought forward for a panel this copad does not own.
        assert_eq!(
            parse_focus_reply(r#"{"ok":true,"result":{"focused":false,"raised":true}}"#),
            FocusOutcome {
                focused: false,
                raised: false
            }
        );
    }

    #[test]
    fn an_unreachable_copad_is_a_quiet_false() {
        // The whole point: a dead socket must not panic, hang or print — it must just let the
        // caller fall back to activating the application.
        assert_eq!(
            focus_panel("/nonexistent/copad-gui.sock", "panel-1"),
            FocusOutcome::NONE
        );
    }
}
