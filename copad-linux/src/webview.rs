use gtk4::prelude::*;
use webkit6::prelude::*;

use crate::panel::Panel;

fn build_webview_css(theme: &copad_core::theme::Theme) -> String {
    format!(
        r#"
.copad-url-bar {{
    background-color: transparent;
    padding: 4px 8px;
}}
.copad-url-entry {{
    background-color: {bg};
    color: {text};
    border: 1px solid {overlay0};
    border-radius: 4px;
    padding: 4px 8px;
    font-size: 12px;
}}
.copad-url-entry:focus {{
    border-color: {accent};
}}
.copad-nav-btn {{
    min-width: 24px;
    min-height: 24px;
    padding: 2px;
    border-radius: 4px;
    color: {text};
}}
.copad-nav-btn:hover {{
    background-color: {overlay0};
}}
"#,
        bg = theme.background,
        text = theme.text,
        overlay0 = theme.overlay0,
        accent = theme.accent,
    )
}

pub struct WebViewPanel {
    pub id: String,
    pub container: gtk4::Box,
    pub webview: webkit6::WebView,
}

impl WebViewPanel {
    pub fn new(url: &str, theme: &copad_core::theme::Theme) -> Self {
        // Dedicated web context for process isolation + sandbox paths
        let web_context = webkit6::WebContext::new();
        web_context.add_path_to_sandbox("/tmp", false);

        let network_session = webkit6::NetworkSession::new_ephemeral();

        let webview = webkit6::WebView::builder()
            .web_context(&web_context)
            .network_session(&network_session)
            .build();

        // Sane defaults
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&webview) {
            settings.set_enable_javascript(true);
            settings.set_allow_file_access_from_file_urls(false);
            settings.set_allow_universal_access_from_file_urls(false);
            settings.set_enable_developer_extras(true);
            settings.set_hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Always);
        }

        webview.set_hexpand(true);
        webview.set_vexpand(true);
        // Match plugin webviews — composite transparently so the
        // window-level BackgroundLayer shows through. Most external
        // pages paint their own solid bg so this is a no-op visually
        // for them; the consistency only matters for blank/about: pages.
        webview.set_background_color(&gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));

        // Note on Hyprland workspace-switch panel freeze — see
        // plugin_panel.rs and docs/troubleshooting.md for the full
        // history. Confirmed upstream WebKitGTK ↔ Hyprland
        // limitation, reproduced with the official MiniBrowser and
        // on integrated graphics. No application-level workaround
        // works; user mitigation is to click the panel or refocus
        // copad to revive frame production.

        webview.load_uri(url);

        // -- Toolbar --
        let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        toolbar.add_css_class("copad-url-bar");

        let back_btn = gtk4::Button::from_icon_name("go-previous-symbolic");
        back_btn.add_css_class("flat");
        back_btn.add_css_class("copad-nav-btn");
        back_btn.set_tooltip_text(Some("Back"));
        back_btn.set_sensitive(false);

        let forward_btn = gtk4::Button::from_icon_name("go-next-symbolic");
        forward_btn.add_css_class("flat");
        forward_btn.add_css_class("copad-nav-btn");
        forward_btn.set_tooltip_text(Some("Forward"));
        forward_btn.set_sensitive(false);

        let reload_btn = gtk4::Button::from_icon_name("view-refresh-symbolic");
        reload_btn.add_css_class("flat");
        reload_btn.add_css_class("copad-nav-btn");
        reload_btn.set_tooltip_text(Some("Reload"));

        let url_entry = gtk4::Entry::new();
        url_entry.set_hexpand(true);
        url_entry.add_css_class("copad-url-entry");
        url_entry.set_text(url);

        let devtools_btn = gtk4::Button::from_icon_name("preferences-system-symbolic");
        devtools_btn.add_css_class("flat");
        devtools_btn.add_css_class("copad-nav-btn");
        devtools_btn.set_tooltip_text(Some("DevTools"));

        toolbar.append(&back_btn);
        toolbar.append(&forward_btn);
        toolbar.append(&reload_btn);
        toolbar.append(&url_entry);
        toolbar.append(&devtools_btn);

        // -- Container --
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.append(&toolbar);
        container.append(&webview);

        // -- CSS --
        let css = gtk4::CssProvider::new();
        css.load_from_string(&build_webview_css(theme));
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().unwrap(),
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
        );

        // -- Signal wiring --

        // URL entry → navigate on Enter
        {
            let wv = webview.clone();
            url_entry.connect_activate(move |entry| {
                let text = entry.text();
                let url = if text.contains("://") || text.starts_with("about:") {
                    text.to_string()
                } else if text.contains('.') && !text.contains(' ') {
                    format!("https://{text}")
                } else {
                    text.to_string()
                };
                wv.load_uri(&url);
            });
        }

        // Back button
        {
            let wv = webview.clone();
            back_btn.connect_clicked(move |_| {
                wv.go_back();
            });
        }

        // Forward button
        {
            let wv = webview.clone();
            forward_btn.connect_clicked(move |_| {
                wv.go_forward();
            });
        }

        // Reload/Stop button
        {
            let wv = webview.clone();
            reload_btn.connect_clicked(move |btn| {
                if wv.is_loading() {
                    wv.stop_loading();
                    btn.set_icon_name("view-refresh-symbolic");
                    btn.set_tooltip_text(Some("Reload"));
                } else {
                    wv.reload();
                }
            });
        }

        // DevTools button
        {
            let wv = webview.clone();
            devtools_btn.connect_clicked(move |_| {
                if let Some(inspector) = wv.inspector() {
                    inspector.show();
                }
            });
        }

        // Update URL entry when URI changes
        {
            let entry = url_entry.clone();
            webview.connect_notify_local(Some("uri"), move |wv, _| {
                if let Some(uri) = wv.uri() {
                    let uri_str = uri.to_string();
                    if !uri_str.is_empty() && uri_str != "about:blank" {
                        entry.set_text(&uri_str);
                    }
                }
            });
        }

        // Handle load failures — preserve URL in bar
        {
            let entry = url_entry.clone();
            let reload = reload_btn.clone();
            webview.connect_load_failed(move |_wv, _event, uri, error| {
                let uri = uri.to_string();
                eprintln!("[webview] Load failed for {uri}: {error}");
                entry.set_text(&uri);
                reload.set_icon_name("view-refresh-symbolic");
                reload.set_tooltip_text(Some("Reload"));
                false // let WebKit show its error page
            });
        }

        // Recover from web process crashes — reload the page
        {
            webview.connect_web_process_terminated(move |wv, reason| {
                let reason_str = match reason {
                    webkit6::WebProcessTerminationReason::Crashed => "crashed",
                    webkit6::WebProcessTerminationReason::ExceededMemoryLimit => {
                        "exceeded memory limit"
                    }
                    _ => "unknown",
                };
                eprintln!("[webview] Web process {reason_str}, reloading...");

                // WebKit requires a fresh load after crash — try_close + reload won't work
                // We need to reload the URI that was being displayed
                if let Some(uri) = wv.uri() {
                    let uri = uri.to_string();
                    if !uri.is_empty() && uri != "about:blank" {
                        gtk4::glib::timeout_add_local_once(
                            std::time::Duration::from_millis(500),
                            {
                                let wv = wv.clone();
                                move || wv.load_uri(&uri)
                            },
                        );
                    }
                }
            });
        }

        // Update back/forward sensitivity + reload/stop icon on load-changed
        {
            let back = back_btn.clone();
            let fwd = forward_btn.clone();
            let reload = reload_btn.clone();
            webview.connect_load_changed(move |wv, event| {
                back.set_sensitive(wv.can_go_back());
                fwd.set_sensitive(wv.can_go_forward());

                match event {
                    webkit6::LoadEvent::Started | webkit6::LoadEvent::Redirected => {
                        reload.set_icon_name("process-stop-symbolic");
                        reload.set_tooltip_text(Some("Stop"));
                    }
                    webkit6::LoadEvent::Committed | webkit6::LoadEvent::Finished => {
                        reload.set_icon_name("view-refresh-symbolic");
                        reload.set_tooltip_text(Some("Reload"));
                    }
                    _ => {}
                }
            });
        }

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            container,
            webview,
        }
    }

    pub fn navigate(&self, url: &str) {
        self.webview.load_uri(url);
    }

    pub fn go_back(&self) {
        self.webview.go_back();
    }

    pub fn go_forward(&self) {
        self.webview.go_forward();
    }

    pub fn reload(&self) {
        self.webview.reload();
    }

    pub fn execute_js(&self, code: &str, callback: impl FnOnce(Result<String, String>) + 'static) {
        self.webview.evaluate_javascript(
            code,
            None,
            None,
            gtk4::gio::Cancellable::NONE,
            move |result| {
                let outcome = match result {
                    Ok(value) => {
                        if value.is_undefined() || value.is_null() {
                            Ok("null".to_string())
                        } else {
                            Ok(value.to_str().to_string())
                        }
                    }
                    Err(e) => Err(Self::friendly_webview_error(e)),
                };
                callback(outcome);
            },
        );
    }

    pub fn snapshot(&self, callback: impl FnOnce(Result<String, String>) + 'static) {
        self.webview.snapshot(
            webkit6::SnapshotRegion::Visible,
            webkit6::SnapshotOptions::NONE,
            gtk4::gio::Cancellable::NONE,
            move |result| {
                let outcome = match result {
                    Ok(texture) => {
                        let bytes = texture.save_to_png_bytes();
                        Ok(gtk4::glib::base64_encode(&bytes).to_string())
                    }
                    Err(e) => Err(Self::friendly_webview_error(e)),
                };
                callback(outcome);
            },
        );
    }

    fn friendly_webview_error(e: gtk4::glib::Error) -> String {
        let msg = e.to_string();
        if msg.contains("Unsupported result type") {
            "Page failed to load — cannot execute JavaScript".to_string()
        } else if msg.contains("error creating the snapshot") {
            "Page failed to load — cannot take screenshot".to_string()
        } else {
            msg
        }
    }

    pub fn current_url(&self) -> String {
        self.webview
            .uri()
            .map(|u| u.to_string())
            .unwrap_or_default()
    }
}

impl Panel for WebViewPanel {
    fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    fn title(&self) -> String {
        self.webview
            .title()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "WebView".to_string())
    }

    fn panel_type(&self) -> &str {
        "webview"
    }

    fn grab_focus(&self) {
        self.webview.grab_focus();
    }

    fn id(&self) -> &str {
        &self.id
    }
}

/// Pre-built JS snippets for AI agent DOM inspection.
/// These return JSON strings so results are structured.
pub mod js {
    /// Query a single element, return its text, tag, attributes, bounding rect
    pub fn query_selector(selector: &str) -> String {
        format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return JSON.stringify(null);
                const r = el.getBoundingClientRect();
                return JSON.stringify({{
                    tag: el.tagName.toLowerCase(),
                    text: el.innerText?.slice(0, 2000) || "",
                    value: el.value || null,
                    href: el.href || null,
                    src: el.src || null,
                    class: el.className || "",
                    id: el.id || "",
                    rect: {{ x: r.x, y: r.y, w: r.width, h: r.height }},
                    visible: r.width > 0 && r.height > 0,
                }});
            }})()"#,
            sel = serde_json::to_string(selector).unwrap()
        )
    }

    /// Query all matching elements, return array of summaries
    pub fn query_selector_all(selector: &str, limit: u32) -> String {
        format!(
            r#"(() => {{
                const els = [...document.querySelectorAll({sel})].slice(0, {limit});
                return JSON.stringify(els.map((el, i) => {{
                    const r = el.getBoundingClientRect();
                    return {{
                        index: i,
                        tag: el.tagName.toLowerCase(),
                        text: el.innerText?.slice(0, 500) || "",
                        value: el.value || null,
                        href: el.href || null,
                        class: el.className || "",
                        id: el.id || "",
                        rect: {{ x: r.x, y: r.y, w: r.width, h: r.height }},
                    }};
                }}));
            }})()"#,
            sel = serde_json::to_string(selector).unwrap(),
            limit = limit,
        )
    }

    /// Get computed styles for an element
    pub fn get_styles(selector: &str, properties: &[&str]) -> String {
        let props_json = serde_json::to_string(properties).unwrap();
        format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return JSON.stringify(null);
                const cs = getComputedStyle(el);
                const props = {props};
                const result = {{}};
                props.forEach(p => result[p] = cs.getPropertyValue(p));
                return JSON.stringify(result);
            }})()"#,
            sel = serde_json::to_string(selector).unwrap(),
            props = props_json,
        )
    }

    /// Click an element by selector
    pub fn click(selector: &str) -> String {
        format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return JSON.stringify({{ ok: false, error: "not found" }});
                el.click();
                return JSON.stringify({{ ok: true }});
            }})()"#,
            sel = serde_json::to_string(selector).unwrap(),
        )
    }

    /// Type text into an input element
    pub fn fill(selector: &str, value: &str) -> String {
        format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return JSON.stringify({{ ok: false, error: "not found" }});
                el.focus();
                el.value = {val};
                el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                return JSON.stringify({{ ok: true }});
            }})()"#,
            sel = serde_json::to_string(selector).unwrap(),
            val = serde_json::to_string(value).unwrap(),
        )
    }

    /// Scroll to position or element
    pub fn scroll(selector: Option<&str>, x: i32, y: i32) -> String {
        match selector {
            Some(sel) => format!(
                r#"(() => {{
                    const el = document.querySelector({sel});
                    if (!el) return JSON.stringify({{ ok: false, error: "not found" }});
                    el.scrollIntoView({{ behavior: "smooth", block: "center" }});
                    return JSON.stringify({{ ok: true }});
                }})()"#,
                sel = serde_json::to_string(sel).unwrap(),
            ),
            None => format!(
                r#"(() => {{
                    window.scrollTo({x}, {y});
                    return JSON.stringify({{ ok: true, scrollX: window.scrollX, scrollY: window.scrollY }});
                }})()"#,
                x = x,
                y = y,
            ),
        }
    }

    /// Get page metadata (title, url, dimensions, forms, links count)
    pub fn page_info() -> String {
        r#"(() => {
            return JSON.stringify({
                title: document.title,
                url: location.href,
                width: document.documentElement.scrollWidth,
                height: document.documentElement.scrollHeight,
                viewportWidth: window.innerWidth,
                viewportHeight: window.innerHeight,
                scrollX: window.scrollX,
                scrollY: window.scrollY,
                forms: document.forms.length,
                links: document.links.length,
                images: document.images.length,
                inputs: document.querySelectorAll('input, textarea, select').length,
                buttons: document.querySelectorAll('button, [role="button"], input[type="submit"]').length,
            });
        })()"#.to_string()
    }
}
