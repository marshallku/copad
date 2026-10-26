//! comux's sidebar, status bar and notification center as native GTK widgets (decisions #169,
//! #170) — the Linux half of `copad-macos/Sources/Copad/ComuxChrome.swift`, over the same shared
//! [`copad_core::muxhost::MuxHost`].
//!
//! The window reports which terminal panels it shows; a comux client running in one of them is
//! found by its binding tag (`COPAD_PANEL_ID` + this GUI's `COPAD_SOCKET`), its TUI chrome is
//! claimed, and these widgets show spaces/agents on the left, the status bar under the
//! terminal and notifications on the right. A window whose visible panes run no comux shows
//! none of them.
//!
//! They are drawn like the comux TUI they replace (decisions #179): the terminal's font, the
//! theme mapped onto the TUI's colour roles, transparent over the window's wallpaper, spaces and
//! agents scrolling separately, usage as gauges.

use copad_core::config::CopadConfig;
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
/// Usage gauge length, in pixels.
const GAUGE_W: i32 = 56;

/// The chrome's font and colours: the terminal's font and the copad theme mapped onto the roles
/// the comux TUI paints with (macOS `ComuxStyle`).
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    family: String,
    size: u32,
    base: String,
    text: String,
    subtext: String,
    dim: String,
    rule: String,
    hover: String,
    mauve: String,
    peach: String,
    green: String,
    yellow: String,
    blue: String,
    red: String,
}

fn rgb(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim_start_matches('#');
    let v = u32::from_str_radix(h.get(..6).unwrap_or("000000"), 16).unwrap_or(0);
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// The colour halfway between `a` and `b` (the TUI's mauve and peach are not ANSI colours).
fn mix(a: &str, b: &str) -> String {
    let (x, y) = (rgb(a), rgb(b));
    let m = |p: u8, q: u8| ((p as u16 + q as u16) / 2) as u8;
    format!("#{:02x}{:02x}{:02x}", m(x.0, y.0), m(x.1, y.1), m(x.2, y.2))
}

impl Style {
    pub fn from_config(cfg: &CopadConfig) -> Self {
        let t = copad_core::theme::Theme::by_name(&cfg.theme.name).unwrap_or_default();
        let p = &t.palette;
        Self {
            family: cfg.terminal.font_family.clone(),
            size: cfg.terminal.font_size.max(7),
            base: t.background.clone(),
            text: t.text.clone(),
            subtext: t.subtext1.clone(),
            dim: t.subtext0.clone(),
            rule: t.surface2.clone(),
            hover: t.surface1.clone(),
            mauve: mix(&p[5], &p[4]),
            peach: mix(&p[1], &p[3]),
            green: p[2].clone(),
            yellow: p[3].clone(),
            blue: p[4].clone(),
            red: t.red.clone(),
        }
    }

    /// Every rule the chrome needs, scoped to its three containers (class rules carry the
    /// container id too: the font/colour rule on `#… *` would outrank a bare class).
    fn css(&self) -> String {
        let Style {
            family,
            size,
            base,
            text,
            subtext,
            rule,
            hover,
            mauve,
            yellow,
            blue,
            red,
            ..
        } = self;
        let family = family.replace('"', "");
        format!(
            r#"
#comux-sidebar, #comux-sidebar *, #comux-notifications, #comux-notifications *,
#comux-status, #comux-status * {{ font-family: "{family}"; font-size: {size}pt; color: {subtext}; }}
#comux-sidebar, #comux-sidebar list, #comux-sidebar row, #comux-sidebar scrolledwindow,
#comux-sidebar viewport, #comux-notifications list, #comux-notifications row,
#comux-notifications scrolledwindow, #comux-notifications viewport,
#comux-status scrolledwindow, #comux-status viewport {{ background: transparent; box-shadow: none; }}
#comux-sidebar {{ border-right: 1px solid {rule}; }}
#comux-notifications {{ background: transparent; border-left: 1px solid {rule}; }}
#comux-notifications.floating {{ background: {base}; box-shadow: -4px 0 12px alpha(black, 0.35); }}
#comux-sidebar row, #comux-notifications row {{ padding: 0; min-height: 0; border-radius: 4px; }}
#comux-sidebar row:hover, #comux-notifications row:hover {{ background: alpha({hover}, 0.8); }}
#comux-sidebar row:selected, #comux-notifications row:selected {{ background: transparent; }}
#comux-status {{ background: alpha({base}, 0.92); border-top: 1px solid {rule}; padding: 2px 8px; }}
.comux-rule {{ background: {rule}; min-height: 1px; min-width: 1px; border: none; }}
.comux-flat {{ background: none; border: none; box-shadow: none; padding: 0 4px; min-height: 0; min-width: 0; }}
.comux-flat:hover {{ background: alpha({hover}, 0.8); }}
#comux-status .comux-pill {{ background: {mauve}; color: {base}; font-weight: bold; border-radius: 4px; padding: 0 6px; }}
#comux-status .comux-chip {{ background: alpha({rule}, 0.7); border-radius: 4px; padding: 0 7px; min-height: 0; border: none; box-shadow: none; }}
#comux-status .comux-chip.active {{ background: {blue}; }}
#comux-status .comux-chip.active label {{ color: {base}; font-weight: bold; }}
#comux-status .comux-badge {{ color: {base}; font-weight: bold; border-radius: 4px; padding: 0 6px; }}
#comux-status .comux-badge.red {{ background: {red}; }}
#comux-status .comux-badge.yellow {{ background: {yellow}; }}
.comux-text {{ color: {text}; }}
"#
        )
    }

    fn status(&self, s: &str) -> (&'static str, &str) {
        match s {
            "working" => ("●", &self.green),
            "ready" => ("◐", &self.blue),
            "blocked" => ("▲", &self.yellow),
            _ => ("○", &self.dim),
        }
    }

    /// green < 70% ≤ yellow < 90% ≤ red, like the TUI's gauges.
    fn usage(&self, pct: f64) -> &str {
        if pct >= 90.0 {
            &self.red
        } else if pct >= 70.0 {
            &self.yellow
        } else {
            &self.green
        }
    }
}

/// `text` in `color`, escaped for Pango markup.
fn span(color: &str, text: &str) -> String {
    format!(
        "<span foreground=\"{color}\">{}</span>",
        gtk4::glib::markup_escape_text(text)
    )
}

fn bold(markup: String) -> String {
    format!("<b>{markup}</b>")
}

/// One agents-list row (decisions #179): every row the list shows, in order, so a row's index
/// is also its action's index — headers and rules are rows too.
#[derive(Debug, PartialEq, Eq)]
enum AgentRow {
    /// A blocked agent pinned under the header (agents index). It is duplicated, not moved,
    /// so the list below never shifts under the pointer.
    Band(usize),
    /// The rule closing the band off.
    Rule,
    /// A space's header (index of its first agent), only when agents span several spaces.
    Header(usize),
    Agent(usize),
}

/// The agents list's rows, and whether they are grouped under space headers.
fn agent_rows(agents: &[Value]) -> (Vec<AgentRow>, bool) {
    let mut out: Vec<AgentRow> = agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a["status"] == "blocked")
        .map(|(i, _)| AgentRow::Band(i))
        .collect();
    if !out.is_empty() {
        out.push(AgentRow::Rule);
    }
    // Groups in the order comux lists them; one space needs no header.
    let mut order: Vec<&str> = Vec::new();
    for a in agents {
        let sid = a["space_id"].as_str().unwrap_or_default();
        if !order.contains(&sid) {
            order.push(sid);
        }
    }
    let grouped = order.len() > 1;
    for sid in order {
        let mut first = true;
        for (i, a) in agents.iter().enumerate() {
            if a["space_id"].as_str().unwrap_or_default() != sid {
                continue;
            }
            if grouped && first {
                out.push(AgentRow::Header(i));
            }
            first = false;
            out.push(AgentRow::Agent(i));
        }
    }
    (out, grouped)
}

/// How the sidebar's height is shared (the TUI's `split_sidebar`): a half that needs less than
/// its share gives the rest to the other; when both overflow they split evenly. Each keeps its
/// floor; when even the floors do not fit, spaces yields first, down to its header.
fn split_sidebar(
    total: i32,
    spaces: i32,
    agents: i32,
    floor_spaces: i32,
    floor_agents: i32,
    header: i32,
) -> i32 {
    let half = total / 2;
    let top = if spaces + agents <= total || spaces <= half {
        spaces
    } else if agents <= half {
        total - agents
    } else {
        half
    };
    top.max(floor_spaces)
        .min((total - floor_agents).max(header))
        .max(header.min(total))
}

/// A space's subtitle, like the TUI's: its git branch, else what its focused pane runs.
fn space_subtitle(branch: &str, tabs: &[Value]) -> String {
    if !branch.is_empty() {
        return branch.to_string();
    }
    tabs.iter()
        .find(|t| t["active"] == true)
        .and_then(|t| t["panes"].as_array())
        .and_then(|p| p.iter().find(|x| x["focused"] == true))
        .and_then(|x| x["label"].as_str())
        .unwrap_or(" ")
        .to_string()
}

/// `2h13m` / `5d19h` until `at` (unix seconds).
fn until(at: i64) -> String {
    let m = ((at - now_unix()).max(0)) / 60;
    match m {
        0..60 => format!("{m}m"),
        60..1440 => format!("{}h{}m", m / 60, m % 60),
        _ => format!("{}d{}h", m / 1440, (m / 60) % 24),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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

/// A two-line row for the overlay dialogs' pickers.
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

/// A label showing `markup`, left-aligned and ellipsized.
fn markup(m: &str) -> gtk4::Label {
    let l = gtk4::Label::new(None);
    l.set_markup(m);
    l.set_xalign(0.0);
    l.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    l
}

/// A label showing `markup` at its full width: what the layout budgets for, never cut.
fn fixed(m: &str) -> gtk4::Label {
    let l = gtk4::Label::new(None);
    l.set_markup(m);
    l
}

fn rule() -> gtk4::Separator {
    let s = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    s.add_css_class("comux-rule");
    s.set_hexpand(true);
    s.set_valign(gtk4::Align::Center);
    s
}

/// `name ───`: a section or group header.
fn header_row(m: &str, badge: Option<String>) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    b.set_margin_start(8);
    b.set_margin_end(8);
    b.set_margin_top(2);
    b.set_margin_bottom(2);
    let l = markup(m);
    l.set_hexpand(false);
    b.append(&l);
    if let Some(badge) = badge {
        b.append(&markup(&badge));
    }
    b.append(&rule());
    b
}

/// The TUI's two-row entry: `left … right` over an indented subtitle.
fn entry(left: &str, right: &str, sub: Option<&str>, indent: i32) -> gtk4::Box {
    let v = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    v.set_margin_start(8);
    v.set_margin_end(8);
    v.set_margin_top(3);
    v.set_margin_bottom(3);
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let l = markup(left);
    l.set_hexpand(true);
    top.append(&l);
    if !right.is_empty() {
        top.append(&markup(right));
    }
    v.append(&top);
    if let Some(sub) = sub {
        let s = markup(sub);
        s.set_margin_start(indent);
        v.append(&s);
    }
    v
}

fn list_row(child: &impl IsA<gtk4::Widget>, activatable: bool) -> gtk4::ListBoxRow {
    let r = gtk4::ListBoxRow::new();
    r.set_child(Some(child));
    r.set_activatable(activatable);
    r.set_selectable(false);
    r
}

fn clear(list: &gtk4::ListBox) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
}

fn elapsed(since: i64) -> String {
    let s = (now_unix() - since).max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{}m", s / 3600, (s / 60) % 60),
    }
}

fn flat_button(icon: &str, tip: &str, name: &str) -> gtk4::Button {
    let b = gtk4::Button::from_icon_name(icon);
    b.add_css_class("comux-flat");
    b.set_tooltip_text(Some(tip));
    b.set_widget_name(name);
    b
}

pub struct ComuxPanel {
    pub sidebar: gtk4::Box,
    /// Where the notification panel docks (beside the terminal); while it floats over a narrow
    /// window it lives in the overlay [`mount`](Self::mount) was given instead.
    pub notif_dock: gtk4::Box,
    notifications: gtk4::Box,
    /// comux's status bar, natively (decisions #170): under the notebook.
    pub status: gtk4::Box,
    /// The overlay currently shown as a dialog, and that dialog.
    overlay_shown: Cell<Option<u64>>,
    overlay_window: RefCell<Option<gtk4::Window>>,
    /// The spec that dialog was built from: the same overlay id with a new spec means its
    /// rows landed (decisions #172) and the dialog is rebuilt.
    overlay_spec: RefCell<Value>,
    spaces: gtk4::ListBox,
    spaces_scroll: gtk4::ScrolledWindow,
    spaces_head: gtk4::Box,
    agents: gtk4::ListBox,
    agents_head: gtk4::Box,
    notif_list: gtk4::ListBox,
    bell_count: gtk4::Label,
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
    /// What a click on row N of each list does (empty = nothing): aligned with EVERY row.
    space_actions: RefCell<Vec<Vec<Value>>>,
    agent_actions: RefCell<Vec<Vec<Value>>>,
    notif_actions: RefCell<Vec<Vec<Value>>>,
    style: RefCell<Style>,
    css: gtk4::CssProvider,
    /// The overlay the notification panel floats in, and whether it does now.
    float_in: RefCell<Option<gtk4::Overlay>>,
    floating: Cell<bool>,
    /// Inputs of the last layout pass (window width, sidebar height, generation), so the
    /// measuring work runs only when one of them moved.
    laid_out: Cell<(i32, i32, u64)>,
    /// The usage readout's level (0 full … 3 one window, 4 hidden), the page shown at level 3,
    /// and the status width + generation it was chosen for.
    usage_level: Cell<u8>,
    usage_page: Cell<usize>,
    usage_fit: Cell<(i32, u64)>,
    usage_box: RefCell<Option<gtk4::Box>>,
    chips: RefCell<Option<gtk4::ScrolledWindow>>,
    /// The wall-clock minute last drawn: ages and the clock move on.
    minute: Cell<i64>,
}

impl ComuxPanel {
    pub fn new(config: &CopadConfig) -> Rc<Self> {
        let style = Style::from_config(config);
        let css = gtk4::CssProvider::new();
        css.load_from_string(&style.css());
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }

        let sidebar = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        sidebar.set_widget_name("comux-sidebar");
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        head.set_margin_start(8);
        head.set_margin_end(4);
        head.set_margin_top(4);
        head.set_margin_bottom(2);
        let title = markup(&bold(span(&style.mauve, "comux")));
        title.set_hexpand(true);
        head.append(&title);
        let new_session = flat_button(
            "list-add-symbolic",
            "New session… (Ctrl-b C)",
            "comux-new-session",
        );
        head.append(&new_session);
        let resume = flat_button(
            "document-open-recent-symbolic",
            "Resume a conversation… (Ctrl-b R)",
            "comux-resume",
        );
        head.append(&resume);
        let bell = gtk4::Button::new();
        bell.add_css_class("comux-flat");
        bell.set_tooltip_text(Some("Notifications (Ctrl-b a)"));
        bell.set_widget_name("comux-bell");
        let bell_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        bell_box.append(&gtk4::Image::from_icon_name(
            "preferences-system-notifications-symbolic",
        ));
        let bell_count = gtk4::Label::new(None);
        bell_box.append(&bell_count);
        bell.set_child(Some(&bell_box));
        head.append(&bell);
        sidebar.append(&head);

        let spaces_head = header_row(&bold(span(&style.dim, "spaces")), None);
        sidebar.append(&spaces_head);
        let spaces = gtk4::ListBox::new();
        let spaces_scroll = gtk4::ScrolledWindow::new();
        spaces_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        spaces_scroll.set_child(Some(&spaces));
        sidebar.append(&spaces_scroll);
        let agents_head = header_row(&bold(span(&style.dim, "agents")), None);
        sidebar.append(&agents_head);
        let agents = gtk4::ListBox::new();
        let agents_scroll = gtk4::ScrolledWindow::new();
        agents_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        agents_scroll.set_vexpand(true);
        agents_scroll.set_child(Some(&agents));
        sidebar.append(&agents_scroll);

        let notifications = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        notifications.set_widget_name("comux-notifications");
        let notif_head = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        notif_head.set_margin_start(10);
        notif_head.set_margin_end(4);
        notif_head.set_margin_top(4);
        notif_head.set_margin_bottom(4);
        let notif_title = markup(&bold(span(&style.dim, "notifications")));
        notif_title.set_hexpand(true);
        notif_head.append(&notif_title);
        let clear_all = gtk4::Button::with_label("Clear all");
        clear_all.add_css_class("comux-flat");
        notif_head.append(&clear_all);
        notifications.append(&notif_head);
        notifications.append(&rule());
        let notif_list = gtk4::ListBox::new();
        let nscroll = gtk4::ScrolledWindow::new();
        nscroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        nscroll.set_vexpand(true);
        nscroll.set_child(Some(&notif_list));
        notifications.append(&nscroll);
        let notif_dock = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        notif_dock.append(&notifications);
        sidebar.set_visible(false);
        notif_dock.set_visible(false);
        notifications.set_visible(false);
        let status = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        status.set_widget_name("comux-status");
        status.set_visible(false);

        let local = Rc::new(MuxHost::start(Endpoint::local()));
        let this = Rc::new(Self {
            sidebar,
            notif_dock,
            notifications,
            status,
            overlay_shown: Cell::new(None),
            overlay_window: RefCell::new(None),
            overlay_spec: RefCell::new(Value::Null),
            spaces,
            spaces_scroll,
            spaces_head,
            agents,
            agents_head,
            notif_list,
            bell_count,
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
            style: RefCell::new(style),
            css,
            float_in: RefCell::new(None),
            floating: Cell::new(false),
            laid_out: Cell::new((0, 0, 0)),
            usage_level: Cell::new(0),
            usage_page: Cell::new(0),
            usage_fit: Cell::new((0, 0)),
            usage_box: RefCell::new(None),
            chips: RefCell::new(None),
            minute: Cell::new(0),
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
        bell.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.notif_open.set(!p.notif_open.get());
                p.relayout(true);
            }
        });
        for (list, which) in [(&this.spaces, 0), (&this.agents, 1), (&this.notif_list, 2)] {
            let w = Rc::downgrade(&this);
            list.connect_row_activated(move |_, row| {
                let Some(p) = w.upgrade() else { return };
                let i = row.index() as usize;
                let acts = match which {
                    0 => &p.space_actions,
                    1 => &p.agent_actions,
                    _ => &p.notif_actions,
                };
                let reqs = acts.borrow().get(i).cloned().unwrap_or_default();
                for r in reqs {
                    p.cur().command(r);
                }
            });
        }
        this
    }

    /// Hand the panel the window and the overlay over the terminal area: the notification
    /// panel floats there when the window is too narrow to dock it, and Esc closes it then.
    pub fn mount(self: &Rc<Self>, window: &gtk4::ApplicationWindow, over: &gtk4::Overlay) {
        *self.float_in.borrow_mut() = Some(over.clone());
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            if let Some(p) = w.upgrade()
                && key == gtk4::gdk::Key::Escape
                && p.floating.get()
                && p.notif_open.get()
            {
                p.notif_open.set(false);
                p.relayout(true);
                return gtk4::glib::Propagation::Stop;
            }
            gtk4::glib::Propagation::Proceed
        });
        window.add_controller(keys);
    }

    /// Config hot-reload: the terminal font or the theme changed.
    pub fn apply_config(self: &Rc<Self>, config: &CopadConfig) {
        let style = Style::from_config(config);
        if *self.style.borrow() == style {
            return;
        }
        self.css.load_from_string(&style.css());
        *self.style.borrow_mut() = style;
        self.last_gen.set(0); // rebuild every row in the new colours
        self.laid_out.set((0, 0, 0));
        self.usage_fit.set((0, 0));
    }

    /// Called from the window's main-loop timer: re-report the visible panels now and then,
    /// rebuild the lists when the host's state moved, and re-fit the layout to the window.
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
        let minute = now_unix() / 60;
        if generation != self.last_gen.get() || minute != self.minute.get() {
            self.last_gen.set(generation);
            self.minute.set(minute);
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
        let key = (window.width(), self.sidebar.height(), self.last_gen.get());
        if key != self.laid_out.get() {
            self.laid_out.set(key);
            self.relayout(false);
        }
        self.fit_usage();
    }

    fn state(&self) -> Value {
        self.cur().state_json()
    }

    fn cur(&self) -> Rc<MuxHost> {
        self.current.borrow().clone()
    }

    /// The width of one cell of the chrome's font.
    fn advance(&self) -> i32 {
        self.sidebar
            .create_pango_layout(Some("MMMMMMMMMM"))
            .pixel_size()
            .0
            .max(10)
            / 10
    }

    /// Panel sizes follow the font within a budget of the window (sidebar ≤ 30%,
    /// notifications ≤ 35%); the notification panel floats over the terminal when docking it
    /// would leave fewer than 40 columns. Then the sidebar's halves are split.
    fn relayout(&self, force_visibility: bool) {
        let on = self.hosting(&self.state());
        let adv = self.advance();
        let width = self
            .float_in
            .borrow()
            .as_ref()
            .and_then(|o| o.root())
            .map_or(0, |r| r.width());
        let pct = |p: i32| width * p / 100;
        let side = (30 * adv).max(200).min(pct(30).max(160));
        let notif = (38 * adv).max(260).min(pct(35).max(220));
        let open = on && self.notif_open.get();
        let floating = open && width > 0 && width - side - notif < 40 * adv;
        self.sidebar.set_width_request(side);
        self.notifications.set_width_request(notif);
        self.sidebar.set_visible(on);
        self.status.set_visible(on);
        self.place_notifications(floating);
        self.notifications.set_visible(open);
        self.notif_dock.set_visible(open && !floating);
        let _ = force_visibility;
        self.split();
    }

    fn place_notifications(&self, floating: bool) {
        let Some(over) = self.float_in.borrow().clone() else {
            return;
        };
        if floating == self.floating.get() {
            return;
        }
        self.floating.set(floating);
        if floating {
            self.notif_dock.remove(&self.notifications);
            self.notifications.set_halign(gtk4::Align::End);
            self.notifications.add_css_class("floating");
            over.add_overlay(&self.notifications);
        } else {
            over.remove_overlay(&self.notifications);
            self.notifications.set_halign(gtk4::Align::Fill);
            self.notifications.remove_css_class("floating");
            self.notif_dock.append(&self.notifications);
        }
    }

    /// Share the sidebar's height between spaces and agents by what each list needs.
    fn split(&self) {
        let total = self.sidebar.height();
        if total <= 0 {
            return;
        }
        let w = self.sidebar.width().max(1);
        let (_, head_h, _, _) = self.spaces_head.measure(gtk4::Orientation::Vertical, w);
        let toolbar = self
            .sidebar
            .first_child()
            .map_or(0, |c| c.measure(gtk4::Orientation::Vertical, w).1);
        let (_, spaces_h, _, _) = self.spaces.measure(gtk4::Orientation::Vertical, w);
        let (_, agents_h, _, _) = self.agents.measure(gtk4::Orientation::Vertical, w);
        let entry = self
            .spaces
            .row_at_index(0)
            .map_or(head_h * 2, |r| r.measure(gtk4::Orientation::Vertical, w).1);
        let body = total - toolbar - 2 * head_h;
        let top = split_sidebar(body, spaces_h, agents_h, entry, entry, 0);
        self.spaces_scroll.set_min_content_height(top);
        self.spaces_scroll.set_max_content_height(top);
    }

    fn rebuild(self: &Rc<Self>) {
        let s = self.state();
        let st = self.style.borrow().clone();
        self.rebuild_status(&s, &st);

        // ---- spaces ----
        clear(&self.spaces);
        let mut sa = Vec::new();
        for sess in s["tree"]["sessions"].as_array().into_iter().flatten() {
            let name = sess["name"].as_str().unwrap_or_default();
            let branch = sess["branch"].as_str().unwrap_or_default();
            let active = sess["active"] == true;
            let tabs = sess["tabs"].as_array().cloned().unwrap_or_default();
            let agents = tabs.iter().any(|t| {
                t["panes"]
                    .as_array()
                    .is_some_and(|p| p.iter().any(|x| x["kind"] == "agent"))
            });
            let bell = tabs.iter().any(|t| t["bell"] == true);
            let dot = if active {
                &st.mauve
            } else if agents {
                &st.peach
            } else {
                &st.dim
            };
            let name_m = if active {
                bold(span(&st.text, name))
            } else {
                span(&st.subtext, name)
            };
            let left = format!(
                "{} {}{}",
                span(dot, "●"),
                name_m,
                if bell {
                    format!(" {}", bold(span(&st.yellow, "!")))
                } else {
                    String::new()
                }
            );
            let sub = span(&st.dim, &space_subtitle(branch, &tabs));
            let row = list_row(&entry(&left, "", Some(&sub), 2 * self.advance()), true);
            let sid = sess["id"].as_str().map(str::to_string);
            self.context_menu(
                &row,
                vec![
                    ("Rename session…", Act::Open("rename-session", sid.clone())),
                    ("New session…", Act::Open("new-session", None)),
                    ("Kill session…", Act::Open("kill-session", sid)),
                ],
            );
            row.set_widget_name(&format!("comux-space-{name}"));
            self.spaces.append(&row);
            sa.push(vec![json!({"cmd": "select-session", "target": sess["id"]})]);
        }
        *self.space_actions.borrow_mut() = sa;

        // ---- agents ----
        clear(&self.agents);
        let agents: Vec<Value> = s["agents"]["agents"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let (rows, grouped) = agent_rows(&agents);
        let blocked = agents.iter().filter(|a| a["status"] == "blocked").count();
        self.set_agents_badge(&st, blocked);
        let mut aa = Vec::new();
        for r in &rows {
            let (row, act): (gtk4::ListBoxRow, Vec<Value>) = match *r {
                AgentRow::Band(i) => {
                    let a = &agents[i];
                    let left = format!(
                        "{} {}",
                        span(&st.yellow, "▲"),
                        span(
                            &st.text,
                            &format!(
                                "{}/{}",
                                a["space"].as_str().unwrap_or_default(),
                                a["title"].as_str().unwrap_or_default()
                            )
                        )
                    );
                    let right = span(&st.yellow, &elapsed(a["since_unix"].as_i64().unwrap_or(0)));
                    (
                        list_row(&entry(&left, &right, None, 0), true),
                        vec![json!({"cmd": "jump", "target": a["token"]})],
                    )
                }
                AgentRow::Rule => {
                    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
                    b.set_margin_start(8);
                    b.set_margin_end(8);
                    b.append(&rule());
                    (list_row(&b, false), Vec::new())
                }
                AgentRow::Header(i) => {
                    let a = &agents[i];
                    let h = header_row(
                        &span(&st.dim, a["space"].as_str().unwrap_or_default()),
                        None,
                    );
                    (
                        list_row(&h, true),
                        vec![json!({"cmd": "select-session", "target": a["space_id"]})],
                    )
                }
                AgentRow::Agent(i) => {
                    let a = &agents[i];
                    let status = a["status"].as_str().unwrap_or_default();
                    let (glyph, color) = st.status(status);
                    let title = a["title"].as_str().unwrap_or_default();
                    let name = if grouped {
                        title.to_string()
                    } else {
                        format!("{}/{title}", a["space"].as_str().unwrap_or_default())
                    };
                    let name_m = if a["focused"] == true {
                        bold(span(&st.text, &name))
                    } else {
                        span(&st.subtext, &name)
                    };
                    let left = format!("{} {name_m}", span(color, glyph));
                    let right = span(
                        color,
                        &format!(
                            "{status} {}",
                            elapsed(a["since_unix"].as_i64().unwrap_or(0))
                        ),
                    );
                    let detail = a["detail"]
                        .as_str()
                        .or(a["tool"].as_str())
                        .unwrap_or_default();
                    let row = list_row(
                        &entry(
                            &left,
                            &right,
                            Some(&span(&st.dim, detail)),
                            2 * self.advance(),
                        ),
                        true,
                    );
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
                    (row, vec![json!({"cmd": "jump", "target": a["token"]})])
                }
            };
            self.agents.append(&row);
            aa.push(act);
        }
        *self.agent_actions.borrow_mut() = aa;

        // ---- notifications ----
        clear(&self.notif_list);
        let mut na = Vec::new();
        for n in s["notifications"]["items"].as_array().into_iter().flatten() {
            let blocked = n["kind"] == "blocked";
            let title = format!(
                "{} · {}/{}",
                n["tool"].as_str().unwrap_or_default(),
                n["space"].as_str().unwrap_or_default(),
                n["tab"].as_str().unwrap_or_default()
            );
            let title_m = if n["read"] == true {
                span(&st.subtext, &title)
            } else {
                bold(span(&st.text, &title))
            };
            let left = format!(
                "{} {title_m}",
                if blocked {
                    span(&st.yellow, "▲")
                } else {
                    span(&st.green, "✓")
                }
            );
            let body = entry(
                &left,
                &span(&st.dim, n["when"].as_str().unwrap_or_default()),
                Some(&span(&st.subtext, n["body"].as_str().unwrap_or_default())),
                2 * self.advance(),
            );
            body.set_hexpand(true);
            let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            line.append(&body);
            let dismiss = json!({"cmd": "notif-dismiss", "id": n["id"]});
            // A text glyph, not an icon: broadway (the headless backend the e2e uses) asserts
            // when one icon render node is drawn several times in a frame.
            let x = gtk4::Button::new();
            x.set_child(Some(&fixed(&span(&st.dim, "✕"))));
            x.add_css_class("comux-flat");
            x.set_tooltip_text(Some("Dismiss"));
            x.set_widget_name("comux-notif-dismiss");
            x.set_valign(gtk4::Align::Start);
            let (w, d) = (Rc::downgrade(self), dismiss.clone());
            x.connect_clicked(move |_| {
                if let Some(p) = w.upgrade() {
                    p.cur().command(d.clone());
                }
            });
            line.append(&x);
            let row = list_row(&line, true);
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
        self.bell_count.set_markup(&if unread > 0 {
            bold(span(&st.yellow, &unread.to_string()))
        } else {
            String::new()
        });
        self.relayout(false);
    }

    fn set_agents_badge(&self, st: &Style, blocked: usize) {
        while let Some(c) = self.agents_head.first_child() {
            self.agents_head.remove(&c);
        }
        let l = markup(&bold(span(&st.dim, "agents")));
        l.set_hexpand(false);
        self.agents_head.append(&l);
        if blocked > 0 {
            self.agents_head
                .append(&markup(&bold(span(&st.yellow, &format!("⚑{blocked}")))));
        }
        self.agents_head.append(&rule());
    }

    fn badge(text: &str, color: &str) -> gtk4::Label {
        let l = gtk4::Label::new(Some(text));
        l.add_css_class("comux-badge");
        l.add_css_class(color);
        l
    }

    /// comux's status bar (decisions #170, #179): session pill, tab chips (they scroll when
    /// short of room), the armed prefix, the outcome line, counters, usage, update, clock, host.
    fn rebuild_status(self: &Rc<Self>, s: &Value, st: &Style) {
        clear_box(&self.status);
        let status = &s["status"];
        let sess = s["tree"]["sessions"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["active"] == true));
        let adv = self.advance();
        if let Some(sess) = sess {
            let pill = gtk4::Label::new(sess["name"].as_str());
            pill.add_css_class("comux-pill");
            pill.set_max_width_chars(16);
            pill.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            let sid = sess["id"].as_str().map(str::to_string);
            self.context_menu(
                &pill,
                vec![
                    ("Rename session…", Act::Open("rename-session", sid.clone())),
                    ("New session…", Act::Open("new-session", None)),
                    ("Kill session…", Act::Open("kill-session", sid)),
                ],
            );
            self.status.append(&pill);
        }
        let chips = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        let mut active_chip: Option<gtk4::Button> = None;
        for t in sess
            .and_then(|x| x["tabs"].as_array())
            .into_iter()
            .flatten()
        {
            let agent = t["panes"]
                .as_array()
                .is_some_and(|p| p.iter().any(|x| x["kind"] == "agent"));
            let active = t["active"] == true;
            let title = format!(
                "{} {}",
                t["index"].as_u64().unwrap_or(0) + 1,
                t["title"].as_str().unwrap_or_default()
            );
            let fg = if active {
                &st.base
            } else if agent {
                &st.yellow
            } else {
                &st.subtext
            };
            let mut m = String::new();
            if agent {
                m.push_str(&span(if active { &st.base } else { &st.yellow }, "● "));
            }
            m.push_str(&span(fg, &title));
            if t["bell"] == true {
                m.push_str(&bold(span(if active { &st.base } else { &st.red }, " !")));
            }
            let l = markup(&if active { bold(m) } else { m });
            l.set_max_width_chars(24);
            let b = gtk4::Button::new();
            b.set_child(Some(&l));
            b.add_css_class("comux-chip");
            if active {
                b.add_css_class("active");
                active_chip = Some(b.clone());
            }
            b.set_tooltip_text(Some(&title));
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
            chips.append(&b);
        }
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::External, gtk4::PolicyType::Never);
        scroller.set_hexpand(true);
        scroller.set_min_content_width(8 * adv);
        scroller.set_propagate_natural_width(true);
        scroller.set_child(Some(&chips));
        self.status.append(&scroller);
        // The active chip into view once the chips have their positions.
        if let Some(chip) = active_chip {
            let sc = scroller.clone();
            gtk4::glib::idle_add_local_once(move || {
                if let Some(p) = chip.compute_point(&sc, &gtk4::graphene::Point::new(0.0, 0.0)) {
                    let adj = sc.hadjustment();
                    let x = adj.value() + p.x() as f64;
                    if x < adj.value() || x + chip.width() as f64 > adj.value() + adj.page_size() {
                        adj.set_value(x - 8.0);
                    }
                }
            });
        }
        *self.chips.borrow_mut() = Some(scroller);
        if status["prefix_armed"] == true {
            self.status.append(&Self::badge(
                status["prefix_label"].as_str().unwrap_or("^b"),
                "yellow",
            ));
        }
        if let Some(f) = status["flash"]["text"].as_str() {
            let l = markup(&span(
                if status["flash"]["error"] == true {
                    &st.red
                } else {
                    &st.subtext
                },
                f,
            ));
            l.set_max_width_chars(40);
            self.status.append(&l);
        }
        let n = |k: &str| status[k].as_u64().unwrap_or(0);
        if n("bells") > 0 {
            self.status
                .append(&Self::badge(&format!("! {}", n("bells")), "red"));
        }
        if n("attention") > 0 {
            self.status
                .append(&Self::badge(&format!("⚑{}", n("attention")), "yellow"));
        }
        if n("agents") > 0 {
            self.status
                .append(&fixed(&span(&st.peach, &format!("● {}", n("agents")))));
        }
        if n("unread") > 0 {
            self.status
                .append(&fixed(&span(&st.blue, &format!("✉ {}", n("unread")))));
        }
        let usage = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        self.fill_usage(&usage, status, st, self.usage_level.get());
        self.status.append(&usage);
        *self.usage_box.borrow_mut() = Some(usage);
        if let Some(u) = status["update"].as_str() {
            self.status
                .append(&fixed(&span(&st.green, &format!("⬆ {u}"))));
        }
        let clock = gtk4::glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format("%H:%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.status.append(&fixed(&span(&st.subtext, &clock)));
        if let Some(h) = status["host"].as_str().filter(|h| !h.is_empty()) {
            self.status.append(&fixed(&bold(span(&st.text, h))));
        }
    }

    fn windows(status: &Value) -> Vec<Value> {
        status["usage"]["windows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The usage readout at `level`: 0 gauge + % + reset per window, 1 without resets, 2
    /// percentages only, 3 one window at a time (click for the next), 4 nothing.
    fn fill_usage(self: &Rc<Self>, into: &gtk4::Box, status: &Value, st: &Style, level: u8) {
        clear_box(into);
        let windows = Self::windows(status);
        if windows.is_empty() || level >= 4 {
            return;
        }
        let item = |w: &Value, label: &str, gauge: bool, reset: bool| {
            let pct = w["pct"].as_f64().unwrap_or(0.0);
            let color = st.usage(pct).to_string();
            let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
            b.append(&fixed(&span(&st.subtext, label)));
            if gauge {
                b.append(&gauge_widget(pct, &st.rule, &color, self.advance()));
            }
            let stale = if w["stale"] == true { "~" } else { "" };
            b.append(&fixed(&span(
                &color,
                &format!("{stale}{}%", pct.round() as i64),
            )));
            let at = w["resets_at"].as_i64();
            if reset && let Some(at) = at {
                b.append(&fixed(&span(&st.dim, &format!("⟳ {}", until(at)))));
            }
            let tip = match at {
                Some(at) => format!(
                    "{} {} resets {}",
                    w["provider"].as_str().unwrap_or_default(),
                    w["window"].as_str().unwrap_or_default(),
                    gtk4::glib::DateTime::from_unix_local(at)
                        .ok()
                        .and_then(|d| d.format("%a %H:%M").ok())
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                ),
                None => String::new(),
            };
            if !tip.is_empty() {
                b.set_tooltip_text(Some(&tip));
            }
            b
        };
        if level == 3 {
            let page = self.usage_page.get() % windows.len();
            let w = &windows[page];
            let label = format!(
                "{} {}",
                w["provider"].as_str().unwrap_or_default(),
                w["window"].as_str().unwrap_or_default()
            );
            let b = item(w, &label, false, false);
            if windows.len() > 1 {
                b.append(&fixed(&span(
                    &st.dim,
                    &format!("{}/{}", page + 1, windows.len()),
                )));
            }
            let btn = gtk4::Button::new();
            btn.add_css_class("comux-flat");
            btn.set_child(Some(&b));
            let (wk, count) = (Rc::downgrade(self), windows.len());
            btn.connect_clicked(move |_| {
                if let Some(p) = wk.upgrade() {
                    p.usage_page.set((p.usage_page.get() + 1) % count);
                    p.last_gen.set(0);
                }
            });
            into.append(&btn);
            return;
        }
        let mut providers: Vec<(String, Vec<Value>)> = Vec::new();
        for w in windows {
            let p = w["provider"].as_str().unwrap_or_default().to_string();
            match providers.iter_mut().find(|(n, _)| *n == p) {
                Some((_, v)) => v.push(w),
                None => providers.push((p, vec![w])),
            }
        }
        for (name, ws) in providers {
            let g = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            g.append(&fixed(&span(&st.dim, &name)));
            for w in &ws {
                g.append(&item(
                    w,
                    w["window"].as_str().unwrap_or_default(),
                    level == 0 || level == 1,
                    level == 0,
                ));
            }
            into.append(&g);
        }
    }

    /// Pick the richest usage level that fits the room the rest of the status bar leaves.
    /// Each level's width is computed with the status bar's own Pango context (same CSS font)
    /// rather than by building and measuring candidates, which would churn widgets every pass.
    fn fit_usage(self: &Rc<Self>) {
        let width = self.status.width();
        let key = (width, self.last_gen.get());
        if width <= 0 || !self.status.is_visible() || key == self.usage_fit.get() {
            return;
        }
        self.usage_fit.set(key);
        let Some(usage) = self.usage_box.borrow().clone() else {
            return;
        };
        let chips = self.chips.borrow().clone();
        let spacing = self.status.spacing();
        let mut others = 0;
        let mut count = 0;
        let mut c = self.status.first_child();
        while let Some(w) = c {
            if w != *usage.upcast_ref::<gtk4::Widget>() {
                let is_chips = chips
                    .as_ref()
                    .is_some_and(|s| w == *s.upcast_ref::<gtk4::Widget>());
                let (min, nat, _, _) = w.measure(gtk4::Orientation::Horizontal, -1);
                others += if is_chips { min } else { nat };
            }
            count += 1;
            c = w.next_sibling();
        }
        let margins = 16; // #comux-status padding
        let available = width - others - spacing * (count - 1).max(0) - margins;
        let windows = Self::windows(&self.state()["status"]);
        let chosen = (0..4u8)
            .find(|&l| self.usage_width(&windows, l) <= available)
            .unwrap_or(4);
        if chosen != self.usage_level.get() {
            self.usage_level.set(chosen);
            let s = self.state();
            let st = self.style.borrow().clone();
            self.fill_usage(&usage, &s["status"], &st, chosen);
        }
    }

    /// The width [`fill_usage`](Self::fill_usage) will take at `level`, from the same texts
    /// and spacings.
    fn usage_width(&self, windows: &[Value], level: u8) -> i32 {
        if windows.is_empty() {
            return 0;
        }
        let text = |t: &str| self.status.create_pango_layout(Some(t)).pixel_size().0;
        let pct = |w: &Value| {
            format!(
                "{}{}%",
                if w["stale"] == true { "~" } else { "" },
                w["pct"].as_f64().unwrap_or(0.0).round() as i64
            )
        };
        let gauge = GAUGE_W + 4;
        let item = |w: &Value, label: &str, g: bool, reset: bool| {
            let mut x = text(label) + 4 + text(&pct(w));
            if g {
                x += gauge;
            }
            if reset && let Some(at) = w["resets_at"].as_i64() {
                x += 4 + text(&format!("⟳ {}", until(at)));
            }
            x
        };
        if level == 3 {
            let w = &windows[self.usage_page.get() % windows.len()];
            let label = format!(
                "{} {}",
                w["provider"].as_str().unwrap_or_default(),
                w["window"].as_str().unwrap_or_default()
            );
            let pages = if windows.len() > 1 {
                4 + text(&format!("{}/{}", windows.len(), windows.len()))
            } else {
                0
            };
            return 8 + item(w, &label, false, false) + pages;
        }
        let mut groups: Vec<(&str, Vec<&Value>)> = Vec::new();
        for w in windows {
            let p = w["provider"].as_str().unwrap_or_default();
            match groups.iter_mut().find(|(n, _)| *n == p) {
                Some((_, v)) => v.push(w),
                None => groups.push((p, vec![w])),
            }
        }
        let g = level <= 1;
        let r = level == 0;
        let total: i32 = groups
            .iter()
            .map(|(name, ws)| {
                text(name)
                    + ws.iter()
                        .map(|w| 6 + item(w, w["window"].as_str().unwrap_or_default(), g, r))
                        .sum::<i32>()
            })
            .sum();
        total + 10 * (groups.len() as i32 - 1)
    }
}

/// A usage gauge: a capsule track in `track`, filled to `pct` in `fill`.
fn gauge_widget(pct: f64, track: &str, fill: &str, adv: i32) -> gtk4::DrawingArea {
    let d = gtk4::DrawingArea::new();
    let h = (adv / 2).clamp(4, 8);
    d.set_content_width(GAUGE_W);
    d.set_content_height(h);
    d.set_valign(gtk4::Align::Center);
    let (t, f) = (rgb(track), rgb(fill));
    let frac = (pct / 100.0).clamp(0.0, 1.0);
    d.set_draw_func(move |_, cr, w, h| {
        let capsule = |cr: &gtk4::cairo::Context, width: f64| {
            let r = h as f64 / 2.0;
            if width < r * 2.0 {
                cr.rectangle(0.0, 0.0, width, h as f64);
                return;
            }
            cr.new_sub_path();
            cr.arc(
                width - r,
                r,
                r,
                -std::f64::consts::FRAC_PI_2,
                std::f64::consts::FRAC_PI_2,
            );
            cr.arc(
                r,
                r,
                r,
                std::f64::consts::FRAC_PI_2,
                3.0 * std::f64::consts::FRAC_PI_2,
            );
            cr.close_path();
        };
        let c = |v: u8| v as f64 / 255.0;
        cr.set_source_rgb(c(t.0), c(t.1), c(t.2));
        capsule(cr, w as f64);
        let _ = cr.fill();
        if frac > 0.0 {
            cr.set_source_rgb(c(f.0), c(f.1), c(f.2));
            capsule(cr, (w as f64 * frac).max(1.0));
            let _ = cr.fill();
        }
    });
    d
}

impl ComuxPanel {
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

/// Config hot-reload: restyle the window's comux chrome (font, theme).
pub fn apply_config_global(config: &CopadConfig) {
    if let Some(p) = PANEL.with(|g| g.borrow().clone()) {
        p.apply_config(config);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(space: &str, status: &str) -> Value {
        json!({"space": space, "space_id": space, "status": status, "token": format!("{space}-{status}")})
    }

    /// Every row the agents list shows has its action at the same index: headers and the
    /// band's rule shift the rows (codex C2).
    #[test]
    fn agent_rows_put_the_band_first_and_group_by_space() {
        let agents = vec![
            agent("a", "idle"),
            agent("b", "blocked"),
            agent("a", "working"),
        ];
        let (rows, grouped) = agent_rows(&agents);
        assert!(grouped);
        assert_eq!(
            rows,
            vec![
                AgentRow::Band(1),
                AgentRow::Rule,
                AgentRow::Header(0),
                AgentRow::Agent(0),
                AgentRow::Agent(2),
                AgentRow::Header(1),
                AgentRow::Agent(1),
            ]
        );
        let (rows, grouped) = agent_rows(&[agent("a", "idle"), agent("a", "ready")]);
        assert!(!grouped, "one space needs no header");
        assert_eq!(rows, vec![AgentRow::Agent(0), AgentRow::Agent(1)]);
    }

    #[test]
    fn a_space_without_a_branch_names_what_its_focused_pane_runs() {
        let tabs = vec![
            json!({"active": false, "panes": [{"focused": true, "label": "vim"}]}),
            json!({"active": true, "panes": [{"focused": false, "label": "zsh"}, {"focused": true, "label": "claude"}]}),
        ];
        assert_eq!(space_subtitle("", &tabs), "claude");
        assert_eq!(space_subtitle("main", &tabs), "main");
        assert_eq!(space_subtitle("", &[]), " ");
    }

    #[test]
    fn the_sidebar_split_gives_slack_to_the_half_that_needs_it() {
        // Both fit: spaces gets what it needs.
        assert_eq!(split_sidebar(600, 100, 200, 40, 40, 0), 100);
        // Spaces small, agents overflow: spaces keeps its need.
        assert_eq!(split_sidebar(600, 120, 2000, 40, 40, 0), 120);
        // Spaces overflow, agents small: agents keeps its need.
        assert_eq!(split_sidebar(600, 2000, 100, 40, 40, 0), 500);
        // Both overflow: even.
        assert_eq!(split_sidebar(600, 2000, 2000, 40, 40, 0), 300);
        // Floors that do not both fit: spaces yields first, down to its header.
        assert_eq!(split_sidebar(60, 2000, 2000, 40, 40, 20), 20);
    }

    #[test]
    fn colours_mix_and_markup_is_escaped() {
        assert_eq!(mix("#000000", "#ffffff"), "#7f7f7f");
        assert_eq!(
            span("#ffffff", "a<b"),
            "<span foreground=\"#ffffff\">a&lt;b</span>"
        );
        let st = Style::from_config(&CopadConfig::default());
        assert_eq!(st.usage(95.0), st.red);
        assert_eq!(st.usage(75.0), st.yellow);
        assert_eq!(st.usage(10.0), st.green);
        assert!(st.css().contains("#comux-sidebar"));
    }
}
