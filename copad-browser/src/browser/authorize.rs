//! The one gate both GUIs call before any browser RPC reaches a platform handler. The decision
//! is made here, not re-derived per platform, so Linux cannot ship a weaker credential boundary.
//!
//! - Protection freezes the PROFILE, not the tab: while any tab of a profile is protected, no
//!   agent read against any tab of that profile is served.
//! - Authorization is re-checked at result-delivery time, not only dispatch: both GUIs answer
//!   from callbacks, and a read already in flight when the tab became `Protected` must be
//!   suppressed, not answered with a stale value.

use crate::protocol::ResponseError;

use super::secrets::TabMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodClass {
    /// Returns page content or pixels. Blocked under protection: the only calls that can carry a
    /// secret back out.
    Read,
    /// Acts on the page but returns nothing readable. Allowed under protection,
    /// so "fill the password, then click submit" still works.
    Write,
    /// Credential handling. Requires protection.
    Secret,
    /// Tab/profile bookkeeping. Never blocked: an agent must always be able to discover that a
    /// tab is protected and close it.
    Meta,
}

/// Classify a browser RPC method name; `None` for unknown methods. New read-shaped methods MUST
/// go in the `Read` arm. An unrecognised `webview.*` name is `None` rather than `Write` so a
/// forgotten entry shows up as "unknown method" instead of becoming an unguarded read.
pub fn classify(method: &str) -> Option<MethodClass> {
    use MethodClass::*;
    Some(match method {
        "webview.execute_js"
        | "webview.get_content"
        | "webview.query"
        | "webview.query_all"
        | "webview.get_styles"
        | "webview.page_info"
        | "webview.screenshot"
        | "webview.net"
        | "webview.console"
        // Viewport-mode `webview.scroll` returns the live scrollX/scrollY on both platforms, so
        // it is value-returning despite its name. Losing scroll on a protected page is the right
        // trade.
        | "webview.scroll"
        // Both GUIs' `webview.state` returns the LIVE url + title, which discloses what the
        // origin-only restore policy keeps off disk (an OAuth code, a reset token). It is a read,
        // not bookkeeping.
        | "webview.state" => Read,

        "webview.navigate" | "webview.back" | "webview.forward" | "webview.reload"
        | "webview.click" | "webview.fill" | "webview.open"
        | "webview.devtools" | "webview.clear_log" => Write,

        "browser.secret.fill" | "browser.secret.save" => Secret,

        "browser.secret.list"
        | "browser.secret.delete"
        | "webview.tab.new"
        | "webview.tab.list"
        | "webview.tab.select"
        | "webview.tab.close"
        | "webview.tab.move"
        | "webview.tab.protect"
        | "webview.profile.list"
        | "webview.profile.clear" => Meta,

        _ => return None,
    })
}

/// Decide whether a browser RPC may proceed. `profile_protected` is true when any tab in the
/// target tab's profile is in [`TabMode::Protected`]; `target_mode` is the target tab's own mode.
/// Call at dispatch and again before delivering a result, so a read in flight when protection
/// began is suppressed.
pub fn authorize(
    method: &str,
    target_mode: TabMode,
    profile_protected: bool,
) -> Result<(), ResponseError> {
    let Some(class) = classify(method) else {
        return Ok(());
    };
    match class {
        MethodClass::Meta | MethodClass::Write => Ok(()),
        MethodClass::Read => {
            if profile_protected || target_mode == TabMode::Protected {
                Err(ResponseError {
                    code: "tab_protected".into(),
                    message: format!(
                        "{method} is refused while a tab in this profile is protected — \
                         turn the lock off to hand the tab back to automation"
                    ),
                })
            } else {
                Ok(())
            }
        }
        MethodClass::Secret => {
            if target_mode == TabMode::Protected {
                Ok(())
            } else {
                Err(ResponseError {
                    code: "requires_protected".into(),
                    message: format!(
                        "{method} requires the target tab to be protected — \
                         call webview.tab.protect first"
                    ),
                })
            }
        }
    }
}

/// Must a permitted write's result be replaced with a page-independent one?
///
/// `click` reports "ok" vs "not found", a selector oracle: probing `input[name=csrf][value^="a"]`,
/// then `^="ab"`, reads a protected page's DOM one character at a time through allowed writes.
/// The action still happens (so fill-then-submit works) but the caller learns nothing.
///
/// When this returns true, discard the handler's real result and return
/// [`opaque_write_response`]. Re-evaluate at delivery time too, so a write in flight when
/// protection began is also collapsed.
pub fn redacts_write_result(method: &str, target_mode: TabMode, profile_protected: bool) -> bool {
    matches!(classify(method), Some(MethodClass::Write))
        && (profile_protected || target_mode == TabMode::Protected)
}

/// The single response every protected write returns, whatever happened.
pub fn opaque_write_response() -> serde_json::Value {
    serde_json::json!({ "status": "ok", "protected": true })
}

/// Response for a method a platform has wired but not implemented, so a `coctl` script gets a
/// machine-readable `unsupported_capability` instead of `unknown_method`.
pub fn unsupported_capability(method: &str, platform: &str) -> ResponseError {
    ResponseError {
        code: "unsupported_capability".into(),
        message: format!("{method} is not implemented on {platform} yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_value_returning_method_is_classified_as_a_read() {
        for m in [
            "webview.execute_js",
            "webview.get_content",
            "webview.query",
            "webview.query_all",
            "webview.get_styles",
            "webview.page_info",
            "webview.screenshot",
            "webview.net",
            "webview.console",
        ] {
            assert_eq!(classify(m), Some(MethodClass::Read), "{m}");
        }
    }

    #[test]
    fn protection_blocks_reads_but_not_the_submit_click() {
        assert!(authorize("webview.click", TabMode::Protected, true).is_ok());
        assert!(authorize("webview.navigate", TabMode::Protected, true).is_ok());
        assert_eq!(
            authorize("webview.execute_js", TabMode::Protected, true)
                .unwrap_err()
                .code,
            "tab_protected"
        );
        assert_eq!(
            authorize("webview.screenshot", TabMode::Protected, true)
                .unwrap_err()
                .code,
            "tab_protected"
        );
    }

    #[test]
    fn a_sibling_tab_in_a_protected_profile_is_frozen_too() {
        // A concurrent same-profile tab shares storage, so its own mode being Automation is not
        // enough.
        assert_eq!(
            authorize("webview.get_content", TabMode::Automation, true)
                .unwrap_err()
                .code,
            "tab_protected"
        );
        assert!(authorize("webview.get_content", TabMode::Automation, false).is_ok());
    }

    #[test]
    fn a_fill_into_an_unprotected_tab_is_refused() {
        assert_eq!(
            authorize("browser.secret.fill", TabMode::Automation, false)
                .unwrap_err()
                .code,
            "requires_protected"
        );
        assert!(authorize("browser.secret.fill", TabMode::Protected, true).is_ok());
    }

    #[test]
    fn meta_methods_stay_available_so_a_tab_can_always_be_inspected_and_closed() {
        for m in [
            "webview.tab.list",
            "webview.tab.close",
            "webview.tab.protect",
        ] {
            assert!(authorize(m, TabMode::Protected, true).is_ok(), "{m}");
        }
    }

    #[test]
    fn a_protected_write_returns_nothing_the_page_could_have_influenced() {
        assert!(redacts_write_result(
            "webview.click",
            TabMode::Protected,
            true
        ));
        assert!(redacts_write_result(
            "webview.fill",
            TabMode::Automation,
            true
        ));
        assert_eq!(
            opaque_write_response(),
            serde_json::json!({ "status": "ok", "protected": true })
        );
    }

    #[test]
    fn an_unprotected_write_keeps_its_real_result() {
        assert!(!redacts_write_result(
            "webview.click",
            TabMode::Automation,
            false
        ));
    }

    #[test]
    fn reads_and_meta_are_not_routed_through_the_opaque_write_response() {
        assert!(!redacts_write_result(
            "webview.get_content",
            TabMode::Protected,
            true
        ));
        assert!(!redacts_write_result(
            "webview.tab.list",
            TabMode::Protected,
            true
        ));
    }

    #[test]
    fn an_unknown_method_is_passed_through_for_normal_handling() {
        assert_eq!(classify("terminal.write"), None);
        assert!(authorize("terminal.write", TabMode::Protected, true).is_ok());
    }

    #[test]
    fn webview_scroll_is_a_read_because_it_returns_live_scroll_coordinates() {
        assert_eq!(classify("webview.scroll"), Some(MethodClass::Read));
        assert_eq!(
            authorize("webview.scroll", TabMode::Protected, true)
                .unwrap_err()
                .code,
            "tab_protected"
        );
    }

    #[test]
    fn webview_state_is_a_read_because_it_returns_the_live_url_and_title() {
        assert_eq!(classify("webview.state"), Some(MethodClass::Read));
        assert_eq!(
            authorize("webview.state", TabMode::Protected, true)
                .unwrap_err()
                .code,
            "tab_protected"
        );
    }

    #[test]
    fn log_reads_are_reads_because_the_files_can_hold_page_output() {
        assert_eq!(classify("webview.net"), Some(MethodClass::Read));
        assert_eq!(classify("webview.console"), Some(MethodClass::Read));
    }
}
