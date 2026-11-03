//! The comux work graph in the window's right panel on Linux (decisions #204) — the GTK twin of
//! macOS `ComuxGraphPanel`: the shared page (`copad-graph-web`) in a transparent WebKitGTK view,
//! fed from the window's comux, its messages validated (`copad_core::graph_bridge`) before
//! anything acts on them.

use crate::comux_panel::Style;
use copad_core::graph_bridge::{self, GraphMsg};
use copad_core::muxhost::MuxHost;
use gtk4::prelude::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};
use webkit6::prelude::*;

/// How long a pick in the page's agent picker holds before the panel follows focus again.
const PICK_HOLDS: Duration = Duration::from_secs(30);

pub struct GraphPanel {
    /// The panel: a rule on its left, the page.
    pub root: gtk4::Box,
    web: webkit6::WebView,
    /// The page said `ready`.
    loaded: Cell<bool>,
    /// What the page posted since the last [`drain`](Self::drain).
    inbox: Rc<RefCell<Vec<Value>>>,
    fed_epoch: RefCell<String>,
    fed_rev: Cell<Option<u64>>,
    fed_agents: Cell<Option<u64>>,
    fed_page: Cell<u64>,
    fed_theme: RefCell<Option<Style>>,
    followed: RefCell<Option<String>>,
    picked_at: Cell<Option<Instant>>,
}

impl GraphPanel {
    pub fn new() -> Rc<Self> {
        let ucm = webkit6::UserContentManager::new();
        ucm.register_script_message_handler("comux", None);
        let inbox: Rc<RefCell<Vec<Value>>> = Rc::default();
        let sink = inbox.clone();
        ucm.connect_script_message_received(Some("comux"), move |_, v| {
            if let Some(j) = v.to_json(0)
                && let Ok(val) = serde_json::from_str::<Value>(&j)
            {
                sink.borrow_mut().push(val);
            }
        });
        let web = webkit6::WebView::builder()
            .user_content_manager(&ucm)
            .build();
        web.set_background_color(&gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
        web.set_hexpand(true);
        web.set_vexpand(true);
        web.set_widget_name("comux-graph-web");
        // The page is the only document this view ever shows: a load WE start is armed here and
        // allowed once; every other navigation (a link, a script) is refused.
        let first = Rc::new(Cell::new(true));
        let armed = first.clone();
        web.connect_decide_policy(move |_, decision, kind| {
            use webkit6::PolicyDecisionType as T;
            match kind {
                T::NavigationAction | T::NewWindowAction => {
                    if first.replace(false) && kind == T::NavigationAction {
                        decision.use_();
                    } else {
                        decision.ignore();
                    }
                    true
                }
                _ => false,
            }
        });
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.set_widget_name("comux-graph");
        let rule = gtk4::Separator::new(gtk4::Orientation::Vertical);
        rule.add_css_class("comux-rule");
        root.append(&rule);
        root.append(&web);
        let this = Rc::new(Self {
            root,
            web,
            loaded: Cell::new(false),
            inbox,
            fed_epoch: RefCell::default(),
            fed_rev: Cell::new(None),
            fed_agents: Cell::new(None),
            fed_page: Cell::new(0),
            fed_theme: RefCell::new(None),
            followed: RefCell::new(None),
            picked_at: Cell::new(None),
        });
        let w = Rc::downgrade(&this);
        this.web.connect_web_process_terminated(move |web, _| {
            // Start over: the page is gone, and with it everything that was fed.
            if let Some(p) = w.upgrade() {
                p.loaded.set(false);
            }
            armed.set(true);
            web.load_html(&copad_graph_web::page(), None);
        });
        this.web.load_html(&copad_graph_web::page(), None);
        this
    }

    pub fn loaded(&self) -> bool {
        self.loaded.get()
    }

    /// What the page asked for since the last call, validated against `current` — the window's
    /// connection NOW (`key#epoch`), not the one the page was last fed: a message queued for a
    /// connection the window has since left (a remote comux's `open`) is dropped, also while the
    /// page has not been fed the new one yet. `ready` makes the next [`feed`](Self::feed) send
    /// everything again.
    pub fn drain(&self, current: &str) -> Vec<GraphMsg> {
        let raw = std::mem::take(&mut *self.inbox.borrow_mut());
        let epoch = if *self.fed_epoch.borrow() == current {
            current.to_string()
        } else {
            String::new()
        };
        let mut out = Vec::new();
        for v in raw {
            match graph_bridge::parse(&v, &epoch) {
                Some(GraphMsg::Ready) => {
                    self.loaded.set(true);
                    self.fed_epoch.borrow_mut().clear();
                    *self.fed_theme.borrow_mut() = None;
                    out.push(GraphMsg::Ready);
                }
                Some(GraphMsg::Agent { token }) => {
                    // Held for a while; when it ends the panel follows focus again.
                    self.picked_at.set(Some(Instant::now()));
                    *self.followed.borrow_mut() = None;
                    out.push(GraphMsg::Agent { token });
                }
                Some(m) => out.push(m),
                None => {}
            }
        }
        out
    }

    /// Bring the page up to `host` (`key`, `consumer` = this window's page): a new connection
    /// resets it, a changed graph, agent list or history answer is fed, and agent mode follows
    /// the focused agent.
    pub fn feed(&self, host: &MuxHost, key: &str, state: &Value, style: &Style, consumer: &str) {
        if !self.loaded.get() {
            return;
        }
        self.theme(style);
        let meta = &state["graph"];
        let Some(conn) = meta["epoch"]
            .as_u64()
            .filter(|_| state["connected"] == true)
        else {
            if !self.fed_epoch.borrow().is_empty() {
                self.js("comuxGraph.reset()");
                self.fed_epoch.borrow_mut().clear();
            }
            return;
        };
        let epoch = format!("{key}#{conn}");
        let page = meta["pages"][consumer].as_u64().unwrap_or(0);
        if *self.fed_epoch.borrow() != epoch {
            self.js("comuxGraph.reset()");
            self.js(&format!(
                "comuxGraph.setHost({})",
                json!({"local": key.is_empty(), "epoch": epoch})
            ));
            *self.fed_epoch.borrow_mut() = epoch;
            self.fed_rev.set(None);
            self.fed_agents.set(None);
            // An answer to a request of the previous connection is not this page's.
            self.fed_page.set(page);
            *self.followed.borrow_mut() = None;
            self.picked_at.set(None);
        }
        let (rev, agents) = (meta["rev"].as_u64(), meta["agents_rev"].as_u64());
        if rev != self.fed_rev.get()
            || agents != self.fed_agents.get()
            || page != self.fed_page.get()
        {
            let all = host.graph_json(consumer);
            if rev != self.fed_rev.get() || agents != self.fed_agents.get() {
                self.js(&format!("comuxGraph.setData({})", all["data"]));
                self.fed_rev.set(rev);
                self.fed_agents.set(agents);
            }
            if page != self.fed_page.get() && !all["page"].is_null() {
                let answer = &all["page"];
                if answer["ok"] == true {
                    self.js(&format!("comuxGraph.addHistory({})", answer["page"]));
                } else {
                    self.js(&format!(
                        "comuxGraph.historyFailed({})",
                        json!({"code": answer["code"]})
                    ));
                }
            }
            self.fed_page.set(page);
        }
        self.follow_focus(state);
    }

    /// Agent mode shows the agent the user is looking at, unless they picked another one in the
    /// page a moment ago.
    fn follow_focus(&self, state: &Value) {
        let focused = state["agents"]["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["focused"] == true)
            .and_then(|a| a["token"].as_str());
        let Some(focused) = focused else { return };
        if self
            .picked_at
            .get()
            .is_some_and(|t| t.elapsed() < PICK_HOLDS)
        {
            return;
        }
        self.picked_at.set(None);
        if self.followed.borrow().as_deref() == Some(focused) {
            return;
        }
        *self.followed.borrow_mut() = Some(focused.to_string());
        self.js(&format!("comuxGraph.setAgent({})", json!(focused)));
    }

    fn theme(&self, st: &Style) {
        if self.fed_theme.borrow().as_ref() == Some(st) {
            return;
        }
        *self.fed_theme.borrow_mut() = Some(st.clone());
        let theme = json!({
            "base": st.base, "text": st.text, "subtext": st.subtext, "dim": st.dim, "rule": st.rule,
            "hover": st.hover, "accent": st.accent, "mauve": st.mauve, "peach": st.peach,
            "green": st.green, "yellow": st.yellow, "blue": st.blue, "red": st.red,
            "font": st.family, "fontSize": st.size,
        });
        self.js(&format!("comuxGraph.setTheme({theme})"));
        let animations = gtk4::Settings::default().is_none_or(|s| s.is_gtk_enable_animations());
        self.js(&format!("comuxGraph.setReduceMotion({})", !animations));
    }

    pub fn set_mode(&self, mode: &str) {
        if self.loaded.get() {
            self.js(&format!("comuxGraph.setMode({})", json!(mode)));
        }
    }

    /// The page's own read-out (`comuxGraph.state()`), for the e2e.
    pub fn page_state(&self, done: impl FnOnce(Option<Value>) + 'static) {
        if !self.loaded.get() {
            return done(None);
        }
        self.web.evaluate_javascript(
            "JSON.stringify(comuxGraph.state())",
            None,
            None,
            gtk4::gio::Cancellable::NONE,
            move |r| {
                let v = r
                    .ok()
                    .map(|v| v.to_str().to_string())
                    .and_then(|s| serde_json::from_str(&s).ok());
                done(v);
            },
        );
    }

    fn js(&self, source: &str) {
        self.web
            .evaluate_javascript(source, None, None, gtk4::gio::Cancellable::NONE, |_| {});
    }
}

/// The work-graph panel's remembered state: open, mode, the width the user dragged it to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Remembered {
    pub open: bool,
    pub mode: String,
    #[serde(default)]
    pub width: Option<i32>,
}

impl Default for Remembered {
    fn default() -> Self {
        Self {
            open: false,
            mode: "agent".into(),
            width: None,
        }
    }
}

fn remembered_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("copad").join("comux-graph.json"))
}

pub fn load_remembered() -> Remembered {
    remembered_file()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_remembered(r: &Remembered) {
    if let Some(f) = remembered_file() {
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(b) = serde_json::to_vec(r) {
            let _ = std::fs::write(f, b);
        }
    }
}

/// The widths of the sidebar and the graph dock within one budget (decisions #204): the
/// terminal keeps `min_term`; past that the dock gives way first (to 260), then the sidebar (to
/// 160), and if that is still too much the dock is hidden (0) — not closed.
pub fn fit_widths(width: i32, side: i32, graph: i32, min_term: i32) -> (i32, i32) {
    let budget = width - min_term;
    let (mut side, mut graph) = (side, graph);
    if side + graph > budget && graph > 0 {
        graph = (budget - side).max(260);
    }
    if side + graph > budget {
        side = (budget - graph).max(160);
    }
    if side + graph > budget {
        graph = 0;
    }
    (side, graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_keeps_its_minimum() {
        assert_eq!(fit_widths(2000, 300, 500, 400), (300, 500));
        // The dock gives way first, to its floor…
        assert_eq!(fit_widths(1100, 300, 500, 400), (300, 400));
        // …then the sidebar…
        assert_eq!(fit_widths(900, 300, 500, 400), (240, 260));
        // …then the dock goes.
        assert_eq!(fit_widths(700, 300, 500, 400), (160, 0));
        assert_eq!(fit_widths(1000, 300, 0, 400), (300, 0));
    }
}
