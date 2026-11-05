//! Focus a copad TAB directly, for the notification jump path.
//!
//! [`crate::winfocus`] can only activate an application, not one window or tab. Copad exports
//! `COPAD_SOCKET` and `COPAD_PANEL_ID` into every tab, so a comux client inside one can name
//! its exact tab. Every failure (no socket, dead copad, panel of another instance) reports
//! nothing focused, so the caller falls back to application-level activation.
//!
//! Focus and raise are reported separately: on Linux the copad path owns the tab and
//! [`crate::winfocus`] owns the raise whenever copad says it could not.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(1500);

/// What a copad reported about a `panel.focus` call. Focus and raise are separate because GTK
/// under Wayland switches the tab but cannot raise itself (no activation token).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusOutcome {
    pub focused: bool,
    pub raised: bool,
}

impl FocusOutcome {
    const NONE: Self = Self {
        focused: false,
        raised: false,
    };
}

/// Ask the copad instance at `sock` to focus `panel_id` and, if it can, raise itself.
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

/// The contract is testable without a running copad: the reply carries BOTH an envelope `ok` and
/// a payload `focused`, and only the second says the user ended up on the right tab.
///
/// A missing `raised` means `true`: copads built before the field existed raised as a side
/// effect of focusing, and defaulting to false would make the caller re-raise (on macOS an extra
/// `osascript` and possibly a TCC prompt, #102).
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
        assert!(!parse_focus_reply(r#"{"id":"x","ok":true,"result":{"focused":false}}"#).focused);
        // An `ok` envelope with no payload says nothing about focus.
        assert!(!parse_focus_reply(r#"{"id":"x","ok":true}"#).focused);
        assert!(!parse_focus_reply(r#"{"id":"x","ok":false,"error":"nope"}"#).focused);
        assert!(!parse_focus_reply("not json").focused);
        assert!(!parse_focus_reply("").focused);
    }

    #[test]
    fn a_gui_that_cannot_raise_itself_says_so() {
        let gtk = parse_focus_reply(r#"{"ok":true,"result":{"focused":true,"raised":false}}"#);
        assert_eq!(
            gtk,
            FocusOutcome {
                focused: true,
                raised: false
            }
        );
        // A MISSING `raised` must read as true: older copads raised as a side effect of focusing.
        assert!(parse_focus_reply(r#"{"ok":true,"result":{"focused":true}}"#).raised);
        assert!(parse_focus_reply(r#"{"ok":true,"result":{"focused":true,"raised":true}}"#).raised);
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
        assert_eq!(
            focus_panel("/nonexistent/copad-gui.sock", "panel-1"),
            FocusOutcome::NONE
        );
    }
}
