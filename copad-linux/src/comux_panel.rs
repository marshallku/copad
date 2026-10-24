//! comux's sidebar and notification center as native GTK widgets (decisions #169) — the
//! Linux half of `copad-macos/Sources/Copad/ComuxChrome.swift`, over the same shared
//! [`copad_core::muxhost::MuxHost`].
//!
//! The window reports which terminal panels it shows; a comux client running in one of them is
//! found by its binding tag (`COPAD_PANEL_ID` + this GUI's `COPAD_SOCKET`), its TUI sidebar is
//! claimed, and these widgets show spaces/agents on the left and notifications on the right.
//! A window whose visible panes run no comux shows neither.

use copad_core::muxhost::MuxHost;
use copad_mux_proto::link::Endpoint;
use gtk4::prelude::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::tabs::TabManager;

/// How often the visible-panel set is re-read (tab switches, splits and closes all change it).
const SERVE_EVERY: Duration = Duration::from_secs(1);

pub struct ComuxPanel {
    pub sidebar: gtk4::Box,
    pub notifications: gtk4::Box,
    spaces: gtk4::ListBox,
    agents: gtk4::ListBox,
    notif_list: gtk4::ListBox,
    bell: gtk4::Button,
    host: MuxHost,
    socket: String,
    last_gen: Cell<u64>,
    last_serve: Cell<Option<Instant>>,
    served: RefCell<Vec<String>>,
    notif_open: Cell<bool>,
    /// What a click on row N of each list does.
    space_actions: RefCell<Vec<Value>>,
    agent_actions: RefCell<Vec<Value>>,
    notif_actions: RefCell<Vec<Vec<Value>>>,
}

fn heading(text: &str) -> gtk4::Label {
    let l = gtk4::Label::new(Some(text));
    l.set_xalign(0.0);
    l.add_css_class("dim-label");
    l.set_margin_start(10);
    l.set_margin_top(6);
    l
}

fn two_line(title: &str, sub: &str, bold: bool) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    b.set_margin_start(10);
    b.set_margin_end(10);
    b.set_margin_top(3);
    b.set_margin_bottom(3);
    let t = gtk4::Label::new(None);
    t.set_xalign(0.0);
    t.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let esc = gtk4::glib::markup_escape_text(title);
    t.set_markup(&if bold {
        format!("<b>{esc}</b>")
    } else {
        esc.to_string()
    });
    b.append(&t);
    if !sub.is_empty() {
        let s = gtk4::Label::new(Some(sub));
        s.set_xalign(0.0);
        s.add_css_class("dim-label");
        s.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        b.append(&s);
    }
    b
}

fn clear(list: &gtk4::ListBox) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
}

fn elapsed(since: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let s = (now - since).max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{}m", s / 3600, (s / 60) % 60),
    }
}

impl ComuxPanel {
    pub fn new() -> Rc<Self> {
        let sidebar = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        sidebar.set_width_request(240);
        sidebar.set_widget_name("comux-sidebar");
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        let title = heading("comux");
        title.set_hexpand(true);
        head.append(&title);
        let bell = gtk4::Button::with_label("🔔");
        bell.add_css_class("flat");
        head.append(&bell);
        sidebar.append(&head);
        sidebar.append(&heading("spaces"));
        let spaces = gtk4::ListBox::new();
        sidebar.append(&spaces);
        sidebar.append(&heading("agents"));
        let agents = gtk4::ListBox::new();
        let scroll = gtk4::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_child(Some(&agents));
        sidebar.append(&scroll);

        let notifications = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        notifications.set_width_request(300);
        notifications.set_widget_name("comux-notifications");
        notifications.append(&heading("notifications"));
        let notif_list = gtk4::ListBox::new();
        let nscroll = gtk4::ScrolledWindow::new();
        nscroll.set_vexpand(true);
        nscroll.set_child(Some(&notif_list));
        notifications.append(&nscroll);
        sidebar.set_visible(false);
        notifications.set_visible(false);

        let this = Rc::new(Self {
            sidebar,
            notifications,
            spaces,
            agents,
            notif_list,
            bell,
            host: MuxHost::start(Endpoint::local()),
            socket: copad_core::paths::gui_socket_path(std::process::id())
                .display()
                .to_string(),
            last_gen: Cell::new(0),
            last_serve: Cell::new(None),
            served: RefCell::new(Vec::new()),
            notif_open: Cell::new(false),
            space_actions: RefCell::new(Vec::new()),
            agent_actions: RefCell::new(Vec::new()),
            notif_actions: RefCell::new(Vec::new()),
        });
        let w = Rc::downgrade(&this);
        this.bell.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.notif_open.set(!p.notif_open.get());
                p.refresh_visibility();
            }
        });
        for (list, which) in [(&this.spaces, 0), (&this.agents, 1), (&this.notif_list, 2)] {
            let w = Rc::downgrade(&this);
            list.connect_row_activated(move |_, row| {
                let Some(p) = w.upgrade() else { return };
                let i = row.index() as usize;
                let reqs: Vec<Value> = match which {
                    0 => p
                        .space_actions
                        .borrow()
                        .get(i)
                        .cloned()
                        .into_iter()
                        .collect(),
                    1 => p
                        .agent_actions
                        .borrow()
                        .get(i)
                        .cloned()
                        .into_iter()
                        .collect(),
                    _ => p.notif_actions.borrow().get(i).cloned().unwrap_or_default(),
                };
                for r in reqs {
                    p.host.command(r);
                }
            });
        }
        this
    }

    /// Called from the window's main-loop timer: re-report the visible panels now and then,
    /// and rebuild the lists when the host's state moved.
    pub fn tick(&self, mgr: &TabManager, window_visible: bool) {
        if self
            .last_serve
            .get()
            .is_none_or(|t| t.elapsed() >= SERVE_EVERY)
        {
            self.last_serve.set(Some(Instant::now()));
            let panels = if window_visible {
                mgr.visible_terminal_ids()
            } else {
                Vec::new()
            };
            for p in &panels {
                self.host
                    .bind(p, &copad_mux_proto::copad_tag(p, &self.socket));
            }
            if *self.served.borrow() != panels {
                self.host.serve(&panels);
                *self.served.borrow_mut() = panels;
            }
        }
        let generation = self.host.generation();
        if generation != self.last_gen.get() {
            self.last_gen.set(generation);
            self.rebuild();
        }
    }

    fn state(&self) -> Value {
        self.host.state_json()
    }

    /// Whether a visible panel runs a comux client whose sidebar we hold.
    fn hosting(&self, s: &Value) -> bool {
        let claimed: Vec<u64> = s["claimed"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        self.served
            .borrow()
            .iter()
            .any(|p| s["bound"][p].as_u64().is_some_and(|a| claimed.contains(&a)))
    }

    fn refresh_visibility(&self) {
        let on = self.hosting(&self.state());
        self.sidebar.set_visible(on);
        self.notifications.set_visible(on && self.notif_open.get());
    }

    fn rebuild(&self) {
        let s = self.state();
        clear(&self.spaces);
        let mut sa = Vec::new();
        for sess in s["tree"]["sessions"].as_array().into_iter().flatten() {
            let name = sess["name"].as_str().unwrap_or_default();
            let branch = sess["branch"].as_str().unwrap_or_default();
            self.spaces.append(&two_line(
                name,
                branch,
                sess["active"].as_bool() == Some(true),
            ));
            sa.push(json!({"cmd": "select-session", "target": sess["id"]}));
        }
        *self.space_actions.borrow_mut() = sa;

        clear(&self.agents);
        let mut aa = Vec::new();
        let mut agents: Vec<&Value> = s["agents"]["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        agents.sort_by_key(|a| a["status"] != "blocked");
        for a in agents {
            let title = format!(
                "{}/{}  {} {}",
                a["space"].as_str().unwrap_or_default(),
                a["title"].as_str().unwrap_or_default(),
                a["status"].as_str().unwrap_or_default(),
                elapsed(a["since_unix"].as_i64().unwrap_or(0))
            );
            let sub = a["detail"]
                .as_str()
                .or(a["tool"].as_str())
                .unwrap_or_default();
            self.agents
                .append(&two_line(&title, sub, a["status"] == "blocked"));
            aa.push(json!({"cmd": "jump", "target": a["token"]}));
        }
        *self.agent_actions.borrow_mut() = aa;

        clear(&self.notif_list);
        let mut na = Vec::new();
        for n in s["notifications"]["items"].as_array().into_iter().flatten() {
            let title = format!(
                "{} · {}/{}  {}",
                n["tool"].as_str().unwrap_or_default(),
                n["space"].as_str().unwrap_or_default(),
                n["tab"].as_str().unwrap_or_default(),
                n["when"].as_str().unwrap_or_default()
            );
            let body = n["body"].as_str().unwrap_or_default();
            self.notif_list
                .append(&two_line(&title, body, n["read"].as_bool() != Some(true)));
            let mut acts = vec![json!({"cmd": "notif-read", "id": n["id"]})];
            if n["token"].as_str().is_some_and(|t| !t.is_empty()) {
                acts.push(json!({"cmd": "jump", "target": n["token"]}));
            }
            na.push(acts);
        }
        *self.notif_actions.borrow_mut() = na;
        let unread = s["notifications"]["unread"].as_u64().unwrap_or(0);
        self.bell.set_label(&if unread > 0 {
            format!("🔔 {unread}")
        } else {
            "🔔".into()
        });
        self.refresh_visibility();
    }
}
