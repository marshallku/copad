//! The native sidebar's `Ctrl-b e` keyboard mode and its row previews (decisions #181).
//!
//! `Ctrl-b e` in a tab whose comux sidebar copad draws asks copad (through the host) to take the
//! keys like the TUI sidebar does: ↑/↓ j/k move, ←/→ h/l switch spaces↔agents, Enter acts like a
//! click and leaves, Esc/q leaves. Resting the pointer on a space or agent row — or selecting it
//! with the keys — shows its pane (`pane-preview`) in a popover beside the sidebar.
//!
//! The lists are rebuilt on every state change, so selection is by id (session id / agent
//! token), never by row widget, and the popover hangs off the sidebar, which is never rebuilt.

use crate::comux_overlay::line_markup;
use crate::comux_panel::{Style, bold, span};
use copad_core::muxhost::PreviewReq;
use copad_mux_proto as l0;
use gtk4::prelude::*;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// The preview consumer the sidebar uses (an open overlay has its own).
pub const HOVER_CONSUMER: &str = "hover";

/// How long the pointer rests on a row before its preview shows.
const HOVER_DELAY: Duration = Duration::from_millis(500);

/// The keyboard selection: which half, the selected row's id, and its last index (for when that
/// row goes away).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Kbd {
    pub agents: bool,
    pub id: Option<String>,
    pub index: usize,
}

/// What a key does in keyboard mode (pure, unit-tested).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The selection moved / switched half.
    To(Kbd),
    /// Enter on this id (a session id in spaces, an agent token in agents).
    Pick {
        agents: bool,
        id: String,
    },
    Exit,
    /// Swallowed (keyboard mode takes every key, as the TUI's does).
    Swallow,
}

pub(crate) fn step(k: &Kbd, key: &str, spaces: &[String], agents: &[String]) -> Step {
    let list = |a: bool| if a { agents } else { spaces };
    let ids = list(k.agents);
    let at =
        k.id.as_ref()
            .and_then(|id| ids.iter().position(|x| x == id))
            .unwrap_or_else(|| k.index.min(ids.len().saturating_sub(1)));
    match key {
        "Up" | "k" | "Down" | "j" => {
            if ids.is_empty() {
                return Step::Swallow;
            }
            let n = ids.len() as i64;
            let d = if matches!(key, "Up" | "k") { -1 } else { 1 };
            let i = (at as i64 + d).rem_euclid(n) as usize;
            Step::To(Kbd {
                agents: k.agents,
                id: Some(ids[i].clone()),
                index: i,
            })
        }
        "Left" | "h" | "Right" | "l" => {
            let to = matches!(key, "Right" | "l");
            match list(to).first() {
                Some(first) => Step::To(Kbd {
                    agents: to,
                    id: Some(first.clone()),
                    index: 0,
                }),
                None => Step::Swallow,
            }
        }
        "Enter" => match ids.get(at) {
            Some(id) => Step::Pick {
                agents: k.agents,
                id: id.clone(),
            },
            None => Step::Swallow,
        },
        "Esc" | "q" => Step::Exit,
        _ => Step::Swallow,
    }
}

/// One sidebar row as navigation sees it.
struct Row {
    row: gtk4::ListBoxRow,
    /// The id keyboard mode selects it by; `None` = not navigable (a band duplicate, a header).
    id: Option<String>,
    /// The pane its preview shows.
    pane: Option<String>,
}

pub struct SidebarNav {
    kbd: RefCell<Option<Kbd>>,
    /// Who had the keys before keyboard mode; they get them back.
    restore: RefCell<Option<gtk4::Widget>>,
    spaces: RefCell<Vec<Row>>,
    agents: RefCell<Vec<Row>>,
    pop: gtk4::Popover,
    head: gtk4::Label,
    body: gtk4::Label,
    /// The pane the pointer rests on (after the delay), and the pane the popover shows.
    hover: RefCell<Option<String>>,
    shown: RefCell<Option<(String, PreviewReq)>>,
    timer: RefCell<Option<gtk4::glib::SourceId>>,
    /// Bumped by every pointer enter/leave, so a stale timer does nothing.
    hover_gen: Cell<u64>,
    /// The selection or the resting pointer changed since the last `apply` (this window's).
    dirty: Cell<bool>,
}

impl SidebarNav {
    pub fn new(sidebar: &gtk4::Box) -> Rc<SidebarNav> {
        let pop = gtk4::Popover::new();
        pop.set_parent(sidebar);
        pop.set_position(gtk4::PositionType::Right);
        // Never takes the keys or closes on a click elsewhere: it follows the pointer/selection.
        pop.set_autohide(false);
        pop.set_can_focus(false);
        pop.set_widget_name("comux-preview-pop");
        let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        vb.set_margin_start(8);
        vb.set_margin_end(8);
        vb.set_margin_top(6);
        vb.set_margin_bottom(6);
        let head = gtk4::Label::new(None);
        head.set_xalign(0.0);
        let body = gtk4::Label::new(None);
        body.set_xalign(0.0);
        body.set_yalign(1.0);
        body.set_vexpand(true);
        body.set_valign(gtk4::Align::End);
        vb.append(&head);
        vb.append(&body);
        vb.set_overflow(gtk4::Overflow::Hidden);
        pop.set_child(Some(&vb));
        Rc::new(SidebarNav {
            kbd: RefCell::new(None),
            restore: RefCell::new(None),
            spaces: RefCell::new(Vec::new()),
            agents: RefCell::new(Vec::new()),
            pop,
            head,
            body,
            hover: RefCell::new(None),
            shown: RefCell::new(None),
            timer: RefCell::new(None),
            hover_gen: Cell::new(0),
            dirty: Cell::new(false),
        })
    }

    pub fn active(&self) -> bool {
        self.kbd.borrow().is_some()
    }

    /// The lists are about to be rebuilt.
    pub fn clear_rows(&self) {
        self.spaces.borrow_mut().clear();
        self.agents.borrow_mut().clear();
    }

    /// Register a rebuilt row (and give it the pointer-rest preview).
    pub fn add(
        self: &Rc<Self>,
        agents_half: bool,
        row: &gtk4::ListBoxRow,
        id: Option<String>,
        pane: Option<String>,
    ) {
        if let Some(p) = pane.clone() {
            let motion = gtk4::EventControllerMotion::new();
            let w = Rc::downgrade(self);
            let p2 = p.clone();
            motion.connect_enter(move |_, _, _| {
                if let Some(n) = w.upgrade() {
                    n.pointer(Some(p2.clone()));
                }
            });
            let w = Rc::downgrade(self);
            motion.connect_leave(move |_| {
                if let Some(n) = w.upgrade() {
                    n.pointer(None);
                }
            });
            row.add_controller(motion);
        }
        let r = Row {
            row: row.clone(),
            id,
            pane,
        };
        if agents_half {
            self.agents.borrow_mut().push(r);
        } else {
            self.spaces.borrow_mut().push(r);
        }
    }

    /// The pointer entered a row with `pane` (after [`HOVER_DELAY`] its preview shows) or left
    /// one. Rows are rebuilt under a resting pointer every state change, so an enter for the
    /// pane already shown keeps it, and a leave closes only if no enter follows shortly.
    fn pointer(self: &Rc<Self>, pane: Option<String>) {
        let g = self.hover_gen.get() + 1;
        self.hover_gen.set(g);
        if let Some(t) = self.timer.borrow_mut().take() {
            t.remove();
        }
        if pane.is_some() && *self.hover.borrow() == pane {
            return;
        }
        let w = Rc::downgrade(self);
        let delay = if pane.is_some() {
            HOVER_DELAY
        } else {
            Duration::from_millis(150)
        };
        let id = gtk4::glib::timeout_add_local_once(delay, move || {
            if let Some(n) = w.upgrade()
                && n.hover_gen.get() == g
            {
                n.timer.borrow_mut().take();
                *n.hover.borrow_mut() = pane;
                n.signal_redraw();
            }
        });
        *self.timer.borrow_mut() = Some(id);
    }

    /// Ask the panel to re-apply (its next tick calls `apply`, see [`Self::needs_apply`]).
    fn signal_redraw(&self) {
        self.dirty.set(true);
    }

    /// Whether this window's selection or resting pointer changed since the last `apply`.
    pub fn needs_apply(&self) -> bool {
        self.dirty.get()
    }

    /// `Ctrl-b e`: take the keys, starting on `active` (the active session).
    pub fn enter(&self, window: &gtk4::ApplicationWindow, active: Option<String>) {
        if self.kbd.borrow().is_none() {
            *self.restore.borrow_mut() = gtk4::prelude::GtkWindowExt::focus(window);
        }
        let index = self
            .spaces
            .borrow()
            .iter()
            .filter(|r| r.id.is_some())
            .position(|r| r.id == active)
            .unwrap_or(0);
        *self.kbd.borrow_mut() = Some(Kbd {
            agents: false,
            id: active,
            index,
        });
        // The terminal must not keep the keys: park focus on the sidebar list.
        if let Some(r) = self.spaces.borrow().first() {
            r.row.grab_focus();
        }
        self.signal_redraw();
    }

    pub fn exit(&self) {
        if self.kbd.borrow_mut().take().is_none() {
            return;
        }
        if let Some(w) = self.restore.borrow_mut().take() {
            w.grab_focus();
        }
        self.signal_redraw();
    }

    /// A key in keyboard mode. Returns the command to run, if any.
    pub fn key(&self, name: &str) -> Option<Value> {
        let k = self.kbd.borrow().clone()?;
        let ids = |rows: &RefCell<Vec<Row>>| -> Vec<String> {
            rows.borrow().iter().filter_map(|r| r.id.clone()).collect()
        };
        match step(&k, name, &ids(&self.spaces), &ids(&self.agents)) {
            Step::To(k) => {
                *self.kbd.borrow_mut() = Some(k);
                self.signal_redraw();
                None
            }
            Step::Pick { agents, id } => {
                self.exit();
                Some(if agents {
                    serde_json::json!({"cmd": "jump", "target": id})
                } else {
                    serde_json::json!({"cmd": "select-session", "target": id})
                })
            }
            Step::Exit => {
                self.exit();
                None
            }
            Step::Swallow => None,
        }
    }

    /// After a rebuild (or a selection/pointer change): mark the keyboard row, keep it in view,
    /// and place / fill / close the preview popover. `preview` asks the host for a pane.
    pub fn apply(
        &self,
        state: &Value,
        style: &Style,
        cell: (i32, i32),
        preview: &dyn Fn(Option<PreviewReq>),
    ) {
        self.dirty.set(false);
        // Keep the selection valid: the selected row may have gone (session killed).
        let kbd = {
            let mut kbd = self.kbd.borrow_mut();
            if let Some(k) = kbd.as_mut() {
                let rows = if k.agents { &self.agents } else { &self.spaces };
                let ids: Vec<String> = rows.borrow().iter().filter_map(|r| r.id.clone()).collect();
                if !k.id.as_ref().is_some_and(|id| ids.contains(id)) {
                    k.index = k.index.min(ids.len().saturating_sub(1));
                    k.id = ids.get(k.index).cloned();
                }
            }
            kbd.clone()
        };
        let mut target: Option<(gtk4::ListBoxRow, String)> = None;
        for (half, rows) in [(false, &self.spaces), (true, &self.agents)] {
            for r in rows.borrow().iter() {
                let selected = kbd
                    .as_ref()
                    .is_some_and(|k| k.agents == half && r.id.is_some() && k.id == r.id);
                if selected {
                    r.row.add_css_class("comux-kbd");
                    scroll_into_view(&r.row);
                    if let Some(p) = &r.pane {
                        target = Some((r.row.clone(), p.clone()));
                    }
                } else {
                    r.row.remove_css_class("comux-kbd");
                }
            }
        }
        // Keyboard selection first; otherwise the row the pointer rests on.
        if target.is_none()
            && let Some(p) = self.hover.borrow().clone()
        {
            target = [&self.spaces, &self.agents]
                .iter()
                .flat_map(|rows| {
                    rows.borrow()
                        .iter()
                        .map(|r| (r.row.clone(), r.pane.clone()))
                        .collect::<Vec<_>>()
                })
                .find(|(_, pane)| pane.as_deref() == Some(p.as_str()))
                .map(|(row, _)| (row, p));
        }
        let Some((row, pane)) = target else {
            if self.shown.borrow_mut().take().is_some() {
                self.pop.popdown();
                preview(None);
            }
            return;
        };
        let req = PreviewReq {
            pane: pane.clone(),
            rows: 16,
            cols: 72,
        };
        if self.shown.borrow().as_ref().map(|(p, _)| p) != Some(&pane) {
            *self.shown.borrow_mut() = Some((pane.clone(), req.clone()));
            preview(Some(req));
            self.head.set_markup("");
            self.body.set_markup(&span(&style.dim, "…"));
        }
        // The row was just rebuilt and is not laid out yet: point at it once it is.
        let pop = self.pop.downgrade();
        row.add_tick_callback(move |row, _| {
            let Some(pop) = pop.upgrade() else {
                return gtk4::glib::ControlFlow::Break;
            };
            match pop.parent().and_then(|p| row.compute_bounds(&p)) {
                Some(b) if b.width() > 0.0 => {
                    pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                        b.x() as i32,
                        b.y() as i32,
                        b.width() as i32,
                        b.height() as i32,
                    )));
                    if !pop.is_visible() {
                        pop.popup();
                    }
                    gtk4::glib::ControlFlow::Break
                }
                _ => gtk4::glib::ControlFlow::Continue,
            }
        });
        if let Some(c) = self.pop.child() {
            c.set_size_request(72 * cell.0 + 16, 17 * cell.1 + 12);
        }
        let st = &state["previews"][HOVER_CONSUMER];
        match st["state"].as_str() {
            Some("unsupported") => self
                .body
                .set_markup(&span(&style.dim, "preview needs a newer comux")),
            Some("gone") => self
                .body
                .set_markup(&span(&style.dim, "that pane has closed")),
            _ => {
                if let Ok(p) = serde_json::from_value::<l0::Preview>(st["preview"].clone())
                    && p.pane == pane
                {
                    self.head.set_markup(&bold(span(&style.subtext, &p.header)));
                    let lines: Vec<String> = p
                        .lines
                        .iter()
                        .skip(p.lines.len().saturating_sub(16))
                        .map(|l| line_markup(style, l))
                        .collect();
                    self.body.set_markup(&lines.join("\n"));
                }
            }
        }
    }

    /// The window is going away: stop the preview.
    pub fn shutdown(&self, preview: &dyn Fn(Option<PreviewReq>)) {
        if self.shown.borrow_mut().take().is_some() {
            preview(None);
        }
        self.pop.unparent();
    }
}

fn scroll_into_view(row: &gtk4::ListBoxRow) {
    let Some(sw) = row
        .ancestor(gtk4::ScrolledWindow::static_type())
        .and_downcast::<gtk4::ScrolledWindow>()
    else {
        return;
    };
    let Some(list) = row.parent() else { return };
    let Some(b) = row.compute_bounds(&list) else {
        return;
    };
    let adj = sw.vadjustment();
    let (top, bottom) = (b.y() as f64, (b.y() + b.height()) as f64);
    if top < adj.value() {
        adj.set_value(top);
    } else if bottom > adj.value() + adj.page_size() {
        adj.set_value(bottom - adj.page_size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keyboard_mode_moves_switches_picks_and_recovers() {
        let spaces = ids(&["s1", "s2", "s3"]);
        let agents = ids(&["a1"]);
        let k = Kbd {
            agents: false,
            id: Some("s2".into()),
            index: 1,
        };
        assert_eq!(
            step(&k, "Down", &spaces, &agents),
            Step::To(Kbd {
                agents: false,
                id: Some("s3".into()),
                index: 2
            })
        );
        assert_eq!(
            step(&k, "k", &spaces, &agents),
            Step::To(Kbd {
                agents: false,
                id: Some("s1".into()),
                index: 0
            })
        );
        assert_eq!(
            step(&k, "Right", &spaces, &agents),
            Step::To(Kbd {
                agents: true,
                id: Some("a1".into()),
                index: 0
            })
        );
        assert_eq!(
            step(&k, "Enter", &spaces, &agents),
            Step::Pick {
                agents: false,
                id: "s2".into()
            }
        );
        assert_eq!(step(&k, "Esc", &spaces, &agents), Step::Exit);
        assert_eq!(step(&k, "x", &spaces, &agents), Step::Swallow);
        // The selected session went away: the row now at its index is the one picked.
        let gone = Kbd {
            agents: false,
            id: Some("s9".into()),
            index: 1,
        };
        assert_eq!(
            step(&gone, "Enter", &spaces, &agents),
            Step::Pick {
                agents: false,
                id: "s2".into()
            }
        );
        // An empty half is not switched to.
        assert_eq!(step(&k, "Right", &spaces, &[]), Step::Swallow);
    }
}
