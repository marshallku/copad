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

use crate::panel::Panel;
use crate::tabs::TabManager;

/// How often the visible-panel set is re-read (tab switches, splits and closes all change it).
const SERVE_EVERY: Duration = Duration::from_secs(1);

pub struct ComuxPanel {
    pub sidebar: gtk4::Box,
    pub notifications: gtk4::Box,
    /// comux's status bar, natively (decisions #170): under the notebook.
    pub status: gtk4::Box,
    /// The overlay currently shown as a dialog, and that dialog.
    overlay_shown: Cell<Option<u64>>,
    overlay_window: RefCell<Option<gtk4::Window>>,
    /// The spec that dialog was built from: the same overlay id with a new spec means its
    /// rows landed (decisions #172) and the dialog is rebuilt.
    overlay_spec: RefCell<Value>,
    spaces: gtk4::ListBox,
    agents: gtk4::ListBox,
    notif_list: gtk4::ListBox,
    bell: gtk4::Button,
    /// Every comux this window hosts: the local one first, then remote ones copad opened
    /// over SSH (decisions #171), by key.
    hosts: RefCell<Vec<(String, Rc<MuxHost>)>>,
    /// The one the visible panes host (the local one when none does).
    current: RefCell<Rc<MuxHost>>,
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

/// What a native context-menu item does.
#[derive(Clone)]
enum Act {
    /// Open a comux overlay (`rename-session`, …) for our client, with an optional target.
    Open(&'static str, Option<String>),
    /// Run a control request.
    Cmd(Value),
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
        let new_session = gtk4::Button::with_label("+");
        new_session.add_css_class("flat");
        new_session.set_tooltip_text(Some("New session… (Ctrl-b C)"));
        new_session.set_widget_name("comux-new-session");
        head.append(&new_session);
        let resume = gtk4::Button::with_label("⟲");
        resume.add_css_class("flat");
        resume.set_tooltip_text(Some("Resume a conversation… (Ctrl-b R)"));
        resume.set_widget_name("comux-resume");
        head.append(&resume);
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
        let notif_head = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let notif_title = heading("notifications");
        notif_title.set_hexpand(true);
        notif_head.append(&notif_title);
        let clear_all = gtk4::Button::with_label("Clear all");
        clear_all.add_css_class("flat");
        notif_head.append(&clear_all);
        notifications.append(&notif_head);
        let notif_list = gtk4::ListBox::new();
        let nscroll = gtk4::ScrolledWindow::new();
        nscroll.set_vexpand(true);
        nscroll.set_child(Some(&notif_list));
        notifications.append(&nscroll);
        sidebar.set_visible(false);
        notifications.set_visible(false);
        let status = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        status.set_widget_name("comux-status");
        status.set_margin_start(6);
        status.set_margin_end(6);
        status.set_visible(false);

        let local = Rc::new(MuxHost::start(Endpoint::local()));
        let this = Rc::new(Self {
            sidebar,
            notifications,
            status,
            overlay_shown: Cell::new(None),
            overlay_window: RefCell::new(None),
            overlay_spec: RefCell::new(Value::Null),
            spaces,
            agents,
            notif_list,
            bell,
            hosts: RefCell::new(vec![(String::new(), local.clone())]),
            current: RefCell::new(local),
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
        for (b, what) in [(&new_session, "new-session"), (&resume, "resume")] {
            let w = Rc::downgrade(&this);
            b.connect_clicked(move |_| {
                if let Some(p) = w.upgrade() {
                    p.open(what, None);
                }
            });
        }
        let w = Rc::downgrade(&this);
        clear_all.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.cur().command(json!({"cmd": "notif-dismiss"}));
            }
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
                    p.cur().command(r);
                }
            });
        }
        this
    }

    /// Called from the window's main-loop timer: re-report the visible panels now and then,
    /// and rebuild the lists when the host's state moved.
    pub fn tick(self: &Rc<Self>, mgr: &TabManager, window: &gtk4::ApplicationWindow) {
        let window_visible = window.is_visible();
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
            let local = self.hosts.borrow()[0].1.clone();
            for p in &panels {
                local.bind(p, &copad_mux_proto::copad_tag(p, &self.socket));
            }
            if *self.served.borrow() != panels {
                for (_, h) in self.hosts.borrow().iter() {
                    h.serve(&panels);
                }
                *self.served.borrow_mut() = panels;
            }
        }
        // Any host moving is a change; then pick the one the visible panes host.
        let generation: u64 = self
            .hosts
            .borrow()
            .iter()
            .map(|(_, h)| h.generation())
            .sum();
        if generation != self.last_gen.get() {
            self.last_gen.set(generation);
            let pick = self
                .hosts
                .borrow()
                .iter()
                .find(|(_, h)| self.hosting(&h.state_json()))
                .map(|(_, h)| h.clone());
            let pick = pick.unwrap_or_else(|| self.hosts.borrow()[0].1.clone());
            *self.current.borrow_mut() = pick;
            self.rebuild();
            self.present_overlay(window);
        }
    }

    fn state(&self) -> Value {
        self.cur().state_json()
    }

    fn cur(&self) -> Rc<MuxHost> {
        self.current.borrow().clone()
    }

    /// Prepare a REMOTE comux: start (or reuse) the L0 host for `dest` and mint a tag. Returns
    /// the host, the tag (bind the panel to it once it exists) and the command the panel runs.
    pub fn prepare_remote(
        &self,
        dest: &str,
        comux: &str,
    ) -> Result<(Rc<MuxHost>, String, String), String> {
        let refused = || format!("refused destination or command: {dest} / {comux}");
        let key = format!("{dest}|{comux}");
        let existing = self
            .hosts
            .borrow()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, h)| h.clone());
        let host = match existing {
            Some(h) => h,
            None => {
                let ep = copad_core::muxhost::remote_endpoint(dest, comux).ok_or_else(refused)?;
                let h = Rc::new(MuxHost::start(ep));
                h.serve(&self.served.borrow());
                self.hosts.borrow_mut().push((key, h.clone()));
                h
            }
        };
        let tag = copad_core::muxhost::new_remote_tag();
        let cmd =
            copad_core::muxhost::remote_attach_command(dest, comux, &tag).ok_or_else(refused)?;
        Ok((host, tag, cmd))
    }

    /// Ask comux to open one of its overlays for the visible client we host (decisions #172);
    /// it comes back as a delegated overlay.
    fn open(&self, what: &str, target: Option<String>) {
        let s = self.state();
        let claimed: Vec<u64> = s["claimed"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        let panel = self
            .served
            .borrow()
            .iter()
            .find(|p| {
                s["bound"][p.as_str()]
                    .as_u64()
                    .is_some_and(|a| claimed.contains(&a))
            })
            .cloned();
        if let Some(p) = panel {
            self.cur().open(&p, what, target);
        }
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
        self.status.set_visible(on);
    }

    /// comux's status bar: session, its tabs, the armed prefix, the outcome line, bells,
    /// attention, agents, unread, usage, update, host — every field the TUI bar carries.
    fn rebuild_status(self: &Rc<Self>, s: &Value) {
        clear_box(&self.status);
        let st = &s["status"];
        let sess = s["tree"]["sessions"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["active"] == true));
        let pill = gtk4::Label::new(None);
        pill.set_markup(&format!(
            "<b>{}</b>",
            gtk4::glib::markup_escape_text(sess.and_then(|x| x["name"].as_str()).unwrap_or(""))
        ));
        self.status.append(&pill);
        for t in sess
            .and_then(|x| x["tabs"].as_array())
            .into_iter()
            .flatten()
        {
            let mut label = format!(
                "{} {}",
                t["index"].as_u64().unwrap_or(0) + 1,
                t["title"].as_str().unwrap_or_default()
            );
            if t["bell"] == true {
                label.push_str(" !");
            }
            let b = gtk4::Button::with_label(&label);
            b.add_css_class("flat");
            if t["active"] == true {
                b.add_css_class("suggested-action");
            }
            let id = t["id"].clone();
            let w = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(p) = w.upgrade() {
                    p.cur().command(json!({"cmd": "select-tab", "target": id}));
                }
            });
            let target = format!(
                "{}/{}",
                sess.and_then(|x| x["id"].as_str()).unwrap_or_default(),
                t["id"].as_str().unwrap_or_default()
            );
            self.context_menu(
                &b,
                vec![
                    ("Rename tab…", Act::Open("rename-tab", Some(target.clone()))),
                    ("New tab", Act::Cmd(json!({"cmd": "new-tab"}))),
                    ("Close tab…", Act::Open("close-tab", Some(target))),
                ],
            );
            self.status.append(&b);
        }
        let mut middle: Vec<String> = Vec::new();
        if st["prefix_armed"] == true {
            middle.push(format!("[{}]", st["prefix_label"].as_str().unwrap_or("^b")));
        }
        if let Some(f) = st["flash"]["text"].as_str() {
            middle.push(f.to_string());
        }
        let line = gtk4::Label::new(Some(&middle.join("  ")));
        line.set_hexpand(true);
        line.set_xalign(0.0);
        line.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        self.status.append(&line);
        let mut tail: Vec<String> = Vec::new();
        let n = |k: &str| st[k].as_u64().unwrap_or(0);
        if n("bells") > 0 {
            tail.push(format!("! {}", n("bells")));
        }
        if n("attention") > 0 {
            tail.push(format!("⚑{}", n("attention")));
        }
        if n("agents") > 0 {
            tail.push(format!("● {}", n("agents")));
        }
        if n("unread") > 0 {
            tail.push(format!("✉ {}", n("unread")));
        }
        for w in st["usage"]["windows"].as_array().into_iter().flatten() {
            tail.push(format!(
                "{}{} {} {:.0}%",
                if w["stale"] == true { "~" } else { "" },
                w["provider"].as_str().unwrap_or_default(),
                w["window"].as_str().unwrap_or_default(),
                w["pct"].as_f64().unwrap_or(0.0)
            ));
        }
        if let Some(u) = st["update"].as_str() {
            tail.push(format!("⬆ {u}"));
        }
        if let Some(h) = st["host"].as_str() {
            tail.push(h.to_string());
        }
        self.status
            .append(&gtk4::Label::new(Some(&tail.join("  "))));
    }

    /// Show the oldest overlay a comux client in a VISIBLE pane opened, as a modal dialog.
    fn present_overlay(self: &Rc<Self>, window: &gtk4::ApplicationWindow) {
        let s = self.state();
        let served = self.served.borrow().clone();
        let mine = s["overlays"].as_array().and_then(|a| {
            a.iter()
                .find(|o| {
                    o["panel"]
                        .as_str()
                        .is_some_and(|p| served.iter().any(|x| x == p))
                })
                .cloned()
        });
        let mine_id = mine.as_ref().and_then(|o| o["overlay"].as_u64());
        if let Some(shown) = self.overlay_shown.get()
            && mine_id != Some(shown)
        {
            // Answered elsewhere, or comux closed it.
            if let Some(w) = self.overlay_window.borrow_mut().take() {
                w.destroy();
            }
            self.overlay_shown.set(None);
        }
        // The same overlay with a new spec: its rows landed — rebuild the dialog. `destroy`
        // emits no close-request, so this is not a cancel.
        if let Some(o) = &mine
            && mine_id == self.overlay_shown.get()
            && o["spec"] != *self.overlay_spec.borrow()
        {
            if let Some(w) = self.overlay_window.borrow_mut().take() {
                w.destroy();
            }
            self.overlay_shown.set(None);
        }
        let (Some(o), None) = (mine, self.overlay_shown.get()) else {
            return;
        };
        let Some(id) = o["overlay"].as_u64() else {
            return;
        };
        self.overlay_shown.set(Some(id));
        *self.overlay_spec.borrow_mut() = o["spec"].clone();
        let dlg = overlay_dialog(self, window, id, &o["spec"]);
        *self.overlay_window.borrow_mut() = Some(dlg);
    }

    /// Send the user's answer and close the dialog (once: a close after an answer is not a
    /// second, cancelling answer).
    fn answer(&self, id: u64, choice: Option<String>, text: Option<String>) {
        if self.overlay_shown.get() != Some(id) {
            return;
        }
        self.overlay_shown.set(None);
        self.cur().answer(id, choice, text);
        if let Some(w) = self.overlay_window.borrow_mut().take() {
            w.destroy();
        }
    }

    /// A right-click menu on `widget` (decisions #172): each item opens a comux overlay for our
    /// client or runs a command.
    fn context_menu(self: &Rc<Self>, widget: &impl IsA<gtk4::Widget>, items: Vec<(&str, Act)>) {
        let click = gtk4::GestureClick::new();
        click.set_button(3);
        let w = Rc::downgrade(self);
        let anchor = widget.as_ref().clone();
        let items: Rc<Vec<(String, Act)>> =
            Rc::new(items.into_iter().map(|(l, a)| (l.to_string(), a)).collect());
        click.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk4::EventSequenceState::Claimed);
            let pop = gtk4::Popover::new();
            pop.set_parent(&anchor);
            pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            for (label, act) in items.iter() {
                let b = gtk4::Button::with_label(label);
                b.add_css_class("flat");
                let (w, act, pop2) = (w.clone(), act.clone(), pop.clone());
                b.connect_clicked(move |_| {
                    pop2.popdown();
                    if let Some(p) = w.upgrade() {
                        match &act {
                            Act::Open(what, target) => p.open(what, target.clone()),
                            Act::Cmd(req) => p.cur().command(req.clone()),
                        }
                    }
                });
                vb.append(&b);
            }
            pop.set_child(Some(&vb));
            pop.connect_closed(|p| p.unparent());
            pop.popup();
        });
        widget.add_controller(click);
    }

    fn rebuild(self: &Rc<Self>) {
        let s = self.state();
        self.rebuild_status(&s);
        clear(&self.spaces);
        let mut sa = Vec::new();
        for sess in s["tree"]["sessions"].as_array().into_iter().flatten() {
            let name = sess["name"].as_str().unwrap_or_default();
            let branch = sess["branch"].as_str().unwrap_or_default();
            let row = two_line(name, branch, sess["active"].as_bool() == Some(true));
            let sid = sess["id"].as_str().map(str::to_string);
            self.context_menu(
                &row,
                vec![
                    ("Rename session…", Act::Open("rename-session", sid.clone())),
                    ("New session…", Act::Open("new-session", None)),
                    ("Kill session…", Act::Open("kill-session", sid)),
                ],
            );
            self.spaces.append(&row);
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
            let row = two_line(&title, sub, a["status"] == "blocked");
            let tab = format!(
                "{}/{}",
                a["space_id"].as_str().unwrap_or_default(),
                a["tab_id"].as_str().unwrap_or_default()
            );
            self.context_menu(
                &row,
                vec![
                    (
                        "Jump to pane",
                        Act::Cmd(json!({"cmd": "jump", "target": a["token"]})),
                    ),
                    ("Rename tab…", Act::Open("rename-tab", Some(tab))),
                ],
            );
            self.agents.append(&row);
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
            let row = two_line(&title, body, n["read"].as_bool() != Some(true));
            let dismiss = json!({"cmd": "notif-dismiss", "id": n["id"]});
            self.context_menu(&row, vec![("Dismiss", Act::Cmd(dismiss.clone()))]);
            self.notif_list.append(&row);
            // Opening one consumes it and its pane's others (`consume`, which an older comux
            // ignores while still jumping); one whose pane is gone is just dismissed.
            let mut acts = vec![json!({"cmd": "notif-read", "id": n["id"]})];
            if n["token"].as_str().is_some_and(|t| !t.is_empty()) {
                acts.push(json!({"cmd": "jump", "target": n["token"], "consume": true}));
            } else {
                acts.push(dismiss);
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

fn clear_box(b: &gtk4::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

/// A delegated comux overlay as a modal GTK window (decisions #170): a filterable picker, a
/// text prompt or a confirm. Whatever the user picks goes back to comux, which runs it.
fn overlay_dialog(
    panel: &Rc<ComuxPanel>,
    parent: &gtk4::ApplicationWindow,
    id: u64,
    spec: &Value,
) -> gtk4::Window {
    let win = gtk4::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(460)
        .title(spec["title"].as_str().unwrap_or("comux"))
        .build();
    win.set_widget_name("comux-overlay");
    let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    vb.set_margin_top(12);
    vb.set_margin_bottom(12);
    vb.set_margin_start(12);
    vb.set_margin_end(12);
    let w = Rc::downgrade(panel);
    let answer: Rc<dyn Fn(Option<String>, Option<String>)> = Rc::new(move |choice, text| {
        if let Some(p) = w.upgrade() {
            p.answer(id, choice, text);
        }
    });
    match spec["kind"].as_str() {
        Some("prompt") => {
            vb.append(&heading(spec["title"].as_str().unwrap_or_default()));
            let entry = gtk4::Entry::new();
            entry.set_text(spec["initial"].as_str().unwrap_or_default());
            let a = answer.clone();
            entry.connect_activate(move |e| a(None, Some(e.text().to_string())));
            vb.append(&entry);
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            row.set_halign(gtk4::Align::End);
            let cancel = gtk4::Button::with_label("Cancel");
            let a = answer.clone();
            cancel.connect_clicked(move |_| a(None, None));
            let ok = gtk4::Button::with_label("OK");
            ok.add_css_class("suggested-action");
            let a = answer.clone();
            let e2 = entry.clone();
            ok.connect_clicked(move |_| a(None, Some(e2.text().to_string())));
            row.append(&cancel);
            row.append(&ok);
            vb.append(&row);
            entry.grab_focus();
        }
        Some("menu") => {
            if let Some(t) = spec["title"].as_str().filter(|t| !t.is_empty()) {
                vb.append(&heading(t));
            }
            for item in spec["items"].as_array().into_iter().flatten() {
                let b = gtk4::Button::with_label(item["label"].as_str().unwrap_or_default());
                b.add_css_class("flat");
                if item["destructive"] == true {
                    b.add_css_class("destructive-action");
                }
                let a = answer.clone();
                let id = item["id"].as_str().unwrap_or_default().to_string();
                b.connect_clicked(move |_| a(Some(id.clone()), None));
                vb.append(&b);
            }
            let cancel = gtk4::Button::with_label("Cancel");
            let a = answer.clone();
            cancel.connect_clicked(move |_| a(None, None));
            vb.append(&cancel);
        }
        Some("picker") => {
            if spec["loading"] == true {
                let spin = gtk4::Spinner::new();
                spin.start();
                spin.set_widget_name("comux-overlay-loading");
                vb.append(&spin);
            }
            if let Some(n) = spec["note"].as_str().filter(|n| !n.is_empty()) {
                let l = gtk4::Label::new(Some(n));
                l.add_css_class("warning");
                l.set_wrap(true);
                vb.append(&l);
            }
            let search = gtk4::SearchEntry::new();
            vb.append(&search);
            let list = gtk4::ListBox::new();
            let mut ids: Vec<String> = Vec::new();
            let actions: Vec<String> = spec["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["id"].as_str().map(str::to_string))
                .collect();
            let tabs = spec["tabs"].as_array().cloned().unwrap_or_default();
            for tab in &tabs {
                if tabs.len() > 1 {
                    let r = gtk4::ListBoxRow::new();
                    r.set_child(Some(&heading(tab["name"].as_str().unwrap_or_default())));
                    r.set_activatable(false);
                    r.set_selectable(false);
                    list.append(&r);
                    ids.push(String::new());
                }
                for row in tab["rows"].as_array().into_iter().flatten() {
                    let rid = row["id"].as_str().unwrap_or_default().to_string();
                    let label = row["label"].as_str().unwrap_or_default();
                    let detail = row["detail"].as_str().unwrap_or_default();
                    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
                    let txt = two_line(label, detail, row["current"] == true);
                    txt.set_hexpand(true);
                    b.append(&txt);
                    if let Some(badge) = row["badge"].as_str() {
                        b.append(&gtk4::Label::new(Some(badge)));
                    }
                    for act in actions.iter().filter(|_| row["no_actions"] != true) {
                        let btn = gtk4::Button::with_label(act);
                        btn.add_css_class("flat");
                        let a = answer.clone();
                        let choice = format!("{act}:{rid}");
                        btn.connect_clicked(move |_| a(Some(choice.clone()), None));
                        b.append(&btn);
                    }
                    let r = gtk4::ListBoxRow::new();
                    r.set_child(Some(&b));
                    // The text the filter matches, kept on the row.
                    r.set_widget_name(&format!("{label} {detail}").to_lowercase());
                    list.append(&r);
                    ids.push(rid);
                }
            }
            // The TUI's own default row (Ctrl-f opens on the previous session).
            if let Some(sel) = spec["selected"].as_str()
                && let Some(i) = ids.iter().position(|x| x == sel)
            {
                list.select_row(list.row_at_index(i as i32).as_ref());
            }
            let ids = Rc::new(ids);
            let a = answer.clone();
            let ids2 = ids.clone();
            list.connect_row_activated(move |_, row| {
                if let Some(id) = ids2.get(row.index() as usize).filter(|s| !s.is_empty()) {
                    a(Some(id.clone()), None);
                }
            });
            let s2 = search.clone();
            list.set_filter_func(move |row| {
                let f = s2.text().to_lowercase();
                f.is_empty() || row.widget_name().contains(f.as_str())
            });
            let l2 = list.clone();
            search.connect_search_changed(move |_| l2.invalidate_filter());
            let a = answer.clone();
            let l3 = list.clone();
            let s3 = search.clone();
            let ids3 = ids.clone();
            search.connect_activate(move |_| {
                let f = s3.text().to_lowercase();
                // The highlighted row first, if the filter still shows it.
                if let Some(r) = l3.selected_row()
                    && (f.is_empty() || r.widget_name().contains(f.as_str()))
                    && let Some(id) = ids3.get(r.index() as usize).filter(|s| !s.is_empty())
                {
                    a(Some(id.clone()), None);
                    return;
                }
                let mut i = 0;
                while let Some(r) = l3.row_at_index(i) {
                    if (f.is_empty() || r.widget_name().contains(f.as_str()))
                        && let Some(id) = ids3.get(i as usize).filter(|s| !s.is_empty())
                    {
                        a(Some(id.clone()), None);
                        return;
                    }
                    i += 1;
                }
            });
            // ↑/↓ in the filter move the highlight over the rows the filter shows.
            let keys = gtk4::EventControllerKey::new();
            let l4 = list.clone();
            let s4 = search.clone();
            let ids4 = ids.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                let d: i32 = match key {
                    gtk4::gdk::Key::Down => 1,
                    gtk4::gdk::Key::Up => -1,
                    _ => return gtk4::glib::Propagation::Proceed,
                };
                let f = s4.text().to_lowercase();
                let visible: Vec<i32> = (0..ids4.len() as i32)
                    .filter(|i| {
                        !ids4[*i as usize].is_empty()
                            && l4.row_at_index(*i).is_some_and(|r| {
                                f.is_empty() || r.widget_name().contains(f.as_str())
                            })
                    })
                    .collect();
                if visible.is_empty() {
                    return gtk4::glib::Propagation::Stop;
                }
                let cur = l4
                    .selected_row()
                    .and_then(|r| visible.iter().position(|i| *i == r.index()))
                    .unwrap_or(0) as i32;
                let next = visible[((cur + d).rem_euclid(visible.len() as i32)) as usize];
                l4.select_row(l4.row_at_index(next).as_ref());
                gtk4::glib::Propagation::Stop
            });
            search.add_controller(keys);
            let scroll = gtk4::ScrolledWindow::new();
            scroll.set_min_content_height(320);
            scroll.set_child(Some(&list));
            vb.append(&scroll);
            let close = gtk4::Button::with_label("Close");
            let a = answer.clone();
            close.connect_clicked(move |_| a(None, None));
            vb.append(&close);
            search.grab_focus();
        }
        Some("confirm") => {
            vb.append(&gtk4::Label::new(spec["message"].as_str()));
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            row.set_halign(gtk4::Align::End);
            let cancel = gtk4::Button::with_label("Cancel");
            let a = answer.clone();
            cancel.connect_clicked(move |_| a(None, None));
            let yes = gtk4::Button::with_label("Confirm");
            yes.add_css_class("destructive-action");
            let a = answer.clone();
            yes.connect_clicked(move |_| a(Some("yes".into()), None));
            row.append(&cancel);
            row.append(&yes);
            vb.append(&row);
        }
        // A kind this build doesn't know: answered as cancelled, never guessed at.
        _ => {
            let a = answer.clone();
            gtk4::glib::idle_add_local_once(move || a(None, None));
        }
    }
    win.set_child(Some(&vb));
    // Closing the window any other way is a cancel (a no-op after an answer).
    let a = answer.clone();
    win.connect_close_request(move |_| {
        a(None, None);
        gtk4::glib::Propagation::Proceed
    });
    win.present();
    win
}

thread_local! {
    /// The window's panel, for socket commands (GTK main thread only).
    static PANEL: RefCell<Option<Rc<ComuxPanel>>> = const { RefCell::new(None) };
}

pub fn set_global(p: &Rc<ComuxPanel>) {
    PANEL.with(|g| *g.borrow_mut() = Some(p.clone()));
}

/// `comux.open_remote` (decisions #171): a new tab running the remote comux client with a
/// fresh binding tag, and the L0 link to the same host, so the native chrome follows it.
pub fn open_remote(
    mgr: &Rc<TabManager>,
    window: &gtk4::ApplicationWindow,
    dest: &str,
    comux: &str,
) -> Result<(), String> {
    let panel = PANEL
        .with(|g| g.borrow().clone())
        .ok_or("no comux panel in this window")?;
    if !copad_core::muxhost::valid_dest(dest) || !copad_core::muxhost::valid_comux(comux) {
        return Err(format!("refused destination or command: {dest} / {comux}"));
    }
    let (host, tag, cmd) = panel.prepare_remote(dest, comux)?;
    // Handed to the tab as INITIAL input, which the terminal feeds once its shell has spawned —
    // input written before that can be lost (codex P4 C1).
    let (term, _) = mgr.add_tab_with_cwd_and_initial_input(window, None, Some(format!("{cmd}\n")));
    host.bind(term.id(), &tag);
    Ok(())
}

/// Ask for the destination, then [`open_remote`].
pub fn prompt_open_remote(mgr: &Rc<TabManager>, window: &gtk4::ApplicationWindow) {
    let win = gtk4::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Open Remote comux")
        .default_width(360)
        .build();
    let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    vb.set_margin_top(12);
    vb.set_margin_bottom(12);
    vb.set_margin_start(12);
    vb.set_margin_end(12);
    vb.append(&heading(
        "An ~/.ssh/config host or user@host (comux must be installed there)",
    ));
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("user@host"));
    vb.append(&entry);
    win.set_child(Some(&vb));
    let (m, w2, win2) = (mgr.clone(), window.clone(), win.clone());
    entry.connect_activate(move |e| {
        let dest = e.text().trim().to_string();
        win2.close();
        if !dest.is_empty()
            && let Err(err) = open_remote(&m, &w2, &dest, "comux")
        {
            log::warn!("comux.open_remote: {err}");
        }
    });
    win.present();
    entry.grab_focus();
}
