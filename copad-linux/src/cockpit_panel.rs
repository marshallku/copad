//! Agent cockpit panel (GTK) — terminal panes listed with their AI-agent
//! status, attention-sorted, double-click to focus. The Linux half of the
//! cockpit; `CockpitViewController` is the AppKit counterpart. See
//! `docs/agent-cockpit.md`.
//!
//! This is a *view* over the app-lifetime `AgentCockpit` model that `window.rs`
//! owns and pumps — the panel holds no subscription of its own, so opening and
//! closing it never loses state. Redraws arrive via
//! `TabManager::notify_cockpit_views()`.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gtk4::prelude::*;

use copad_core::agent_cockpit::{AgentCockpit, AgentState};
use copad_core::theme::Theme;

use crate::panel::Panel;
use crate::tabs::TabManager;

/// One rendered row: a live pane joined with its model state.
struct Row {
    panel_id: String,
    title: String,
    cwd: String,
    state: AgentState,
}

pub struct CockpitPanel {
    id: String,
    container: gtk4::Box,
    pub list: gtk4::ListBox,
    cockpit: Rc<RefCell<AgentCockpit>>,
    mgr: Weak<TabManager>,
}

impl CockpitPanel {
    pub fn new(cockpit: Rc<RefCell<AgentCockpit>>, mgr: Weak<TabManager>) -> Self {
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.add_css_class("copad-cockpit");

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        header.add_css_class("copad-cockpit-header");
        let title = gtk4::Label::new(Some("Agent cockpit"));
        title.add_css_class("copad-cockpit-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        let refresh = gtk4::Button::with_label("Refresh");
        let reset = gtk4::Button::with_label("Reset");
        for b in [&refresh, &reset] {
            b.add_css_class("copad-cockpit-btn");
        }
        header.append(&title);
        header.append(&refresh);
        header.append(&reset);

        let scroll = gtk4::ScrolledWindow::new();
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_vexpand(true);
        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Browse);
        // Double-click to activate, matching the macOS table's doubleAction —
        // single-click would steal focus away on mere arrow-key browsing.
        list.set_activate_on_single_click(false);
        list.add_css_class("copad-cockpit-list");
        scroll.set_child(Some(&list));

        container.append(&header);
        container.append(&scroll);

        let panel = Self {
            id: uuid::Uuid::new_v4().to_string(),
            container,
            list,
            cockpit,
            mgr,
        };

        panel.connect_row_activated();
        {
            let mgr = panel.mgr.clone();
            refresh.connect_clicked(move |_| {
                if let Some(mgr) = mgr.upgrade() {
                    mgr.notify_cockpit_views();
                }
            });
        }
        {
            let mgr = panel.mgr.clone();
            let cockpit = panel.cockpit.clone();
            reset.connect_clicked(move |_| {
                cockpit.borrow_mut().reset();
                if let Some(mgr) = mgr.upgrade() {
                    mgr.notify_cockpit_views();
                }
            });
        }

        panel
    }

    fn connect_row_activated(&self) {
        let mgr = self.mgr.clone();
        let cockpit = self.cockpit.clone();
        self.list.connect_row_activated(move |_, row| {
            let Some(ptr) = (unsafe { row.data::<String>("panel-id") }) else {
                return;
            };
            let panel_id = unsafe { ptr.as_ref() }.clone();
            let Some(mgr) = mgr.upgrade() else { return };

            // Acting on a pane clears its attention. Borrow as a statement
            // temporary — activate_panel() below re-enters through the focus
            // handler, and a live borrow here would panic.
            cockpit.borrow_mut().acknowledge(&panel_id);
            mgr.activate_panel(&panel_id);
            mgr.notify_cockpit_views();
        });
    }

    /// Rebuild the rows from the live pane snapshot joined with the model,
    /// attention first. Cheap enough to run on any change: the pane count is
    /// small and the model is a hash lookup.
    pub fn reload_rows(&self) {
        let Some(mgr) = self.mgr.upgrade() else {
            return;
        };

        let mut rows: Vec<Row> = {
            let cockpit = self.cockpit.borrow();
            mgr.terminal_pane_snapshot()
                .into_iter()
                .map(|pane| Row {
                    state: cockpit.state(&pane.panel_id),
                    panel_id: pane.panel_id,
                    title: pane.title,
                    cwd: pane.cwd,
                })
                .collect()
        };
        rows.sort_by(|a, b| {
            a.state
                .rank()
                .cmp(&b.state.rank())
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for row in rows {
            self.list.append(&build_row(&row));
        }
    }
}

fn build_row(row: &Row) -> gtk4::ListBoxRow {
    let dot = gtk4::Label::new(Some("●"));
    dot.add_css_class("copad-cockpit-dot");
    dot.add_css_class(&format!("copad-cockpit-dot-{}", row.state.as_str()));

    let title = gtk4::Label::new(Some(&row.title));
    title.add_css_class("copad-cockpit-row-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);

    let subtitle_text = if row.cwd.is_empty() {
        row.state.label().to_string()
    } else {
        format!("{} · {}", row.state.label(), row.cwd)
    };
    let subtitle = gtk4::Label::new(Some(&subtitle_text));
    subtitle.add_css_class("copad-cockpit-row-sub");
    subtitle.set_xalign(0.0);
    // Middle-ellipsize so a deep cwd keeps both its project root and leaf.
    subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&subtitle);

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    hbox.add_css_class("copad-cockpit-row");
    hbox.append(&dot);
    hbox.append(&text);

    let lb_row = gtk4::ListBoxRow::new();
    lb_row.set_child(Some(&hbox));
    unsafe {
        lb_row.set_data("panel-id", row.panel_id.clone());
    }
    lb_row
}

/// Theme-derived CSS. Backgrounds stay transparent so the window wallpaper
/// shows through, as every other panel does. State colors come from the ANSI
/// palette (the theme has no semantic green/blue): 2=green, 3=yellow, 4=blue.
pub fn build_cockpit_css(theme: &Theme) -> String {
    format!(
        r#"
.copad-cockpit,
.copad-cockpit scrolledwindow,
.copad-cockpit list,
.copad-cockpit row {{
    background-color: transparent;
}}
.copad-cockpit-header {{
    padding: 6px 10px;
    border-bottom: 1px solid {overlay0};
}}
.copad-cockpit-title {{
    color: {text};
    font-weight: bold;
}}
.copad-cockpit-btn {{
    padding: 2px 8px;
    min-height: 0;
    color: {text};
    background-color: {surface1};
    border: 1px solid {overlay0};
}}
.copad-cockpit-btn:hover {{
    background-color: {surface2};
}}
.copad-cockpit-row {{
    padding: 4px 8px;
}}
.copad-cockpit-row-title {{
    color: {text};
}}
.copad-cockpit-row-sub {{
    color: {subtext0};
    font-size: 0.8em;
}}
.copad-cockpit list > row:hover {{
    background-color: {surface0};
}}
.copad-cockpit list > row:selected {{
    background-color: {surface2};
}}
.copad-cockpit-dot-awaiting {{ color: {yellow}; }}
.copad-cockpit-dot-done {{ color: {green}; }}
.copad-cockpit-dot-working {{ color: {blue}; }}
.copad-cockpit-dot-idle {{ color: {overlay0}; }}
"#,
        overlay0 = theme.overlay0,
        text = theme.text,
        subtext0 = theme.subtext0,
        surface0 = theme.surface0,
        surface1 = theme.surface1,
        surface2 = theme.surface2,
        green = theme.palette[2],
        yellow = theme.palette[3],
        blue = theme.palette[4],
    )
}

impl Panel for CockpitPanel {
    fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    fn title(&self) -> String {
        "Agents".to_string()
    }

    fn panel_type(&self) -> &str {
        "cockpit"
    }

    fn grab_focus(&self) {
        self.list.grab_focus();
    }

    fn id(&self) -> &str {
        &self.id
    }
}
