//! The Linux half of the Browser Workbench dispatcher gate (work unit B1c).
//!
//! The rule itself is not here: `copad_core::browser::authorize` decides, and this module only
//! resolves the target and applies the decision, so there is one implementation of the security
//! rule. See `docs/browser-workbench-plan.md` §3.2 and decision #100.
//!
//! - The gate runs before `ActionRegistry::try_dispatch`, not before the legacy match; a browser
//!   method registered as an action would otherwise route around it.
//! - Authorization is re-checked at delivery. Value-returning methods answer from a WebKit
//!   callback, so a read in flight when the tab entered [`TabMode::Protected`] must be
//!   suppressed, not answered with a stale value.
//! - A permitted write's result is replaced while protection is on, because `click` reporting
//!   ok-vs-not-found is a selector oracle. `copad_core` owns the predicate
//!   ([`browser::authorize::redacts_write_result`]) and the replacement, and it applies only
//!   under protection.
//!
//! Target state lives on the [`WebViewPanel`], not in a parallel registry: a registry would need
//! unregistering on every pane-close path, and a missed one is a stale-read leak. Reading through
//! [`TabManager::find_panel_by_id`] makes "pane gone" and "entry gone" the same fact.

use std::rc::Rc;

use copad_core::browser::{self, TabMode};
use copad_core::protocol::{Request, Response, ResponseError};

use crate::tabs::TabManager;

const PLATFORM: &str = "Linux";

/// Browser methods that are wired but not built on Linux yet. Registering them gives a `coctl`
/// script a machine-readable "not implemented yet" instead of `unknown_method`.
///
/// Every name here must also be in `copad_daemon::socket::BROWSER_RESERVED_METHODS`, or a
/// service plugin can claim it in `provides[]` and intercept the daemon-routed call before the
/// gate; a test holds the two lists together.
pub const UNIMPLEMENTED: &[&str] = &[
    "webview.tab.new",
    "webview.tab.list",
    "webview.tab.select",
    "webview.tab.close",
    "webview.tab.move",
    "webview.tab.protect",
    "webview.profile.list",
    "webview.profile.clear",
    "browser.secret.list",
    "browser.secret.fill",
    "browser.secret.save",
    "browser.secret.delete",
    "webview.net",
    "webview.console",
    "webview.clear_log",
];

pub enum Gate {
    NotBrowser,
    Refused(ResponseError),
    Allowed(GateCtx),
}

/// Holds ids, not resolved state: replaying a mode captured at dispatch is the bug the delivery
/// check exists to catch.
#[derive(Clone)]
pub struct GateCtx {
    pub method: String,
    pub panel_id: Option<String>,
    /// Whether the target resolved to a live webview when the request was admitted. [`finalize`]
    /// fails closed only when it did: a request that never had a target read nothing, and
    /// replacing its `not_found` with `tab_closed` would report a pane closing that never existed.
    resolved_at_dispatch: bool,
}

struct Resolved {
    mode: TabMode,
    profile_protected: bool,
}

/// `browser.*` vs `webview.*` is naming, not a boundary (that is [`TabMode`]); both are covered.
fn is_browser_method(method: &str) -> bool {
    method.starts_with("webview.") || method.starts_with("browser.")
}

/// `Ok(None)` means no resolvable webview target (bad panel id, or not a webview). At dispatch
/// the existing handler answers that (`not_found` / `wrong_panel_type`, text callers depend on);
/// at delivery [`finalize`] treats it as fail-closed, since a vanished pane must not hand back
/// what it read.
fn resolve(
    mgr: &Rc<TabManager>,
    panel_id: Option<&str>,
    tab_id: Option<&str>,
) -> Result<Option<Resolved>, ResponseError> {
    let profile_protected = |profile: &str| mgr.any_protected_webview_in_profile(profile);

    let Some(panel_id) = panel_id else {
        let profile = mgr.default_browser_profile();
        return Ok(Some(Resolved {
            mode: TabMode::Automation,
            profile_protected: profile_protected(&profile),
        }));
    };

    let Some(panel) = mgr.find_panel_by_id(panel_id) else {
        return Ok(None);
    };
    let Some(wv) = panel.as_webview() else {
        return Ok(None);
    };

    // A `tab_id` naming a tab this pane does not hold is `tab_closed`, never a silent retarget
    // onto the active tab (plan §3.2).
    if let Some(requested) = tab_id
        && requested != wv.tab_id
    {
        return Err(ResponseError {
            code: "tab_closed".into(),
            message: format!("tab {requested} is not open in panel {panel_id}"),
        });
    }

    Ok(Some(Resolved {
        mode: wv.mode(),
        profile_protected: profile_protected(&wv.profile),
    }))
}

/// Decide whether a request may run, before the action registry sees it.
///
/// Check order is load-bearing: "not built yet" is answered BEFORE the policy gate, because
/// `browser.secret.fill` on an unprotected tab answers `requires_protected` and sends the caller
/// to `webview.tab.protect`, which is itself unimplemented here.
pub fn gate(req: &Request, mgr: &Rc<TabManager>) -> Gate {
    if !is_browser_method(&req.method) {
        return Gate::NotBrowser;
    }

    if UNIMPLEMENTED.contains(&req.method.as_str()) {
        return Gate::Refused(browser::authorize::unsupported_capability(
            &req.method,
            PLATFORM,
        ));
    }

    // Only methods core knows about are gated; an unrecognised `webview.*` name falls through to
    // the dispatcher's unknown-method handling, so a typo still reads as a typo.
    if browser::classify(&req.method).is_none() {
        return Gate::NotBrowser;
    }

    let panel_id = req.params.get("id").and_then(|v| v.as_str());
    let tab_id = req.params.get("tab_id").and_then(|v| v.as_str());

    let resolved = match resolve(mgr, panel_id, tab_id) {
        Ok(r) => r,
        Err(err) => return Gate::Refused(err),
    };

    if let Some(r) = &resolved
        && let Err(err) = browser::authorize(&req.method, r.mode, r.profile_protected)
    {
        return Gate::Refused(err);
    }

    Gate::Allowed(GateCtx {
        method: req.method.clone(),
        panel_id: panel_id.map(str::to_string),
        resolved_at_dispatch: resolved.is_some(),
    })
}

/// Apply the gate to a response on its way out, for both synchronous and WebKit-callback arms:
/// a write can leak through its error as a read does through its result.
pub fn finalize(mgr: &Rc<TabManager>, ctx: &GateCtx, resp: Response) -> Response {
    let id = resp.id.clone();

    let resolved = match resolve(mgr, ctx.panel_id.as_deref(), None) {
        Ok(Some(r)) => r,
        // Fail closed, but only for a target that WAS live at dispatch: its mode can no longer be
        // established. A request that never resolved is left to the handler's own `not_found` /
        // `wrong_panel_type`.
        Ok(None) => {
            if ctx.resolved_at_dispatch {
                return Response::error(
                    id,
                    "tab_closed",
                    "the target pane closed before the result was delivered",
                );
            }
            return resp;
        }
        Err(err) => return Response::error(id, &err.code, &err.message),
    };

    if let Err(err) = browser::authorize(&ctx.method, resolved.mode, resolved.profile_protected) {
        return Response::error(id, &err.code, &err.message);
    }

    if browser::authorize::redacts_write_result(
        &ctx.method,
        resolved.mode,
        resolved.profile_protected,
    ) {
        return Response::success(id, browser::authorize::opaque_write_response());
    }

    resp
}

/// Reply path for callback-deferred handlers, which move `cmd.reply` into a closure and cannot
/// reach [`SocketCommand::reply_with_completion`]. Routing them through here makes the
/// delivery-time re-check unskippable.
pub struct BrowserReply {
    reply: std::sync::mpsc::Sender<Response>,
    bus: copad_daemon::socket::EventBus,
    silent: bool,
    ctx: GateCtx,
    mgr: Rc<TabManager>,
}

impl BrowserReply {
    pub fn new(
        reply: std::sync::mpsc::Sender<Response>,
        bus: copad_daemon::socket::EventBus,
        silent: bool,
        ctx: GateCtx,
        mgr: Rc<TabManager>,
    ) -> Self {
        Self {
            reply,
            bus,
            silent,
            ctx,
            mgr,
        }
    }

    pub fn send(self, resp: Response) {
        let resp = finalize(&self.mgr, &self.ctx, resp);
        copad_daemon::socket::publish_legacy_completion(
            &self.bus,
            &self.ctx.method,
            self.silent,
            &resp,
        );
        let _ = self.reply.send(resp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure decisions only (check ORDER, method-name partition). Target resolution and the
    // delivery re-check need a live `TabManager`; `scripts/e2e-browser-linux-gate.sh` covers them.

    #[test]
    fn every_unimplemented_name_is_one_core_classifies() {
        // A name here that core does not know would answer `unsupported_capability` forever
        // while the real method fell through to `unknown_method`.
        for m in UNIMPLEMENTED {
            assert!(
                browser::classify(m).is_some(),
                "{m} is not a method copad-core classifies"
            );
        }
    }

    /// Every `webview.*` method the dispatcher has a match arm for. Each arm reads `browser_ctx`
    /// with `.expect()`, sound only while `gate` classifies the method; an unclassified one
    /// leaves it `None` and PANICS the GUI. A new arm needs its name in
    /// `copad_core::browser::classify`, and this list turns forgetting into a failing test.
    const DISPATCHED_ON_LINUX: &[&str] = &[
        "webview.open",
        "webview.navigate",
        "webview.back",
        "webview.forward",
        "webview.reload",
        "webview.execute_js",
        "webview.get_content",
        "webview.screenshot",
        "webview.query",
        "webview.query_all",
        "webview.get_styles",
        "webview.click",
        "webview.fill",
        "webview.scroll",
        "webview.page_info",
        "webview.state",
        "webview.devtools",
    ];

    #[test]
    fn every_registered_stub_is_reserved_against_plugin_shadowing() {
        // Daemon dispatch checks the action registry BEFORE routing by capability, so a service
        // plugin listing `browser.secret.fill` in `provides[]` would answer the call itself and
        // skip the gate. Registering without reserving opens that window.
        for m in UNIMPLEMENTED {
            assert!(
                copad_daemon::socket::BROWSER_RESERVED_METHODS.contains(m),
                "{m} is registered on Linux but not reserved against plugin `provides[]`"
            );
        }
    }

    #[test]
    fn a_dispatched_method_is_reserved_too() {
        // The built ones are covered by `LEGACY_DISPATCH_METHODS`; asserted here so the two
        // reservation lists together cover the whole browser surface.
        for m in DISPATCHED_ON_LINUX {
            assert!(
                copad_daemon::socket::LEGACY_DISPATCH_METHODS.contains(m)
                    || copad_daemon::socket::BROWSER_RESERVED_METHODS.contains(m),
                "{m} is dispatched but not reserved against plugin `provides[]`"
            );
        }
    }

    #[test]
    fn every_dispatched_method_is_classified_so_the_arms_cannot_panic() {
        for m in DISPATCHED_ON_LINUX {
            assert!(
                browser::classify(m).is_some(),
                "{m} has a dispatcher arm but copad-core does not classify it — \
                 `browser_ctx.expect()` in that arm would panic the GUI"
            );
        }
    }

    #[test]
    fn a_dispatched_method_is_never_also_registered_as_unimplemented() {
        // The two lists partition the surface: an overlap would answer `unsupported_capability`
        // for a method that is dispatched.
        for m in DISPATCHED_ON_LINUX {
            assert!(
                !UNIMPLEMENTED.contains(m),
                "{m} is both dispatched and stubbed"
            );
        }
    }

    #[test]
    fn implemented_read_and_write_methods_are_not_on_the_unimplemented_list() {
        for m in [
            "webview.execute_js",
            "webview.get_content",
            "webview.screenshot",
            "webview.query",
            "webview.query_all",
            "webview.get_styles",
            "webview.page_info",
            "webview.scroll",
            "webview.state",
            "webview.navigate",
            "webview.back",
            "webview.forward",
            "webview.reload",
            "webview.click",
            "webview.fill",
            "webview.open",
            "webview.devtools",
        ] {
            assert!(!UNIMPLEMENTED.contains(&m), "{m} is implemented on Linux");
        }
    }

    #[test]
    fn browser_namespace_is_covered_alongside_webview() {
        assert!(is_browser_method("webview.click"));
        assert!(is_browser_method("browser.secret.fill"));
        assert!(!is_browser_method("tab.switch"));
        assert!(!is_browser_method("terminal.read"));
    }

    #[test]
    fn an_unclassified_webview_name_is_left_to_the_unknown_method_path() {
        assert!(browser::classify("webview.opne").is_none());
        assert!(!UNIMPLEMENTED.contains(&"webview.opne"));
    }
}
