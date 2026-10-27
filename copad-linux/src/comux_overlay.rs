//! comux's delegated overlays drawn over the window like the TUI draws them (decisions #170,
//! redrawn in #180): the `Ctrl-f` switcher, the notification center, the `Ctrl-b C` / `Ctrl-b R`
//! pickers, prompts, confirms and menus. A layer over the whole window swallows clicks (one
//! outside the card cancels) and keys stay in the card; the card is the TUI's box — terminal
//! font, theme colours, glyph cards, `←/→` tabs and the selected pane's live preview.
//!
//! Filtering and key handling happen here (the pure parts are unit-tested); comux keeps the
//! meaning and runs the answer.

use crate::comux_panel::{Style, bold, elapsed, span};
use copad_core::muxhost::PreviewReq;
use copad_mux_proto as l0;
use gtk4::prelude::*;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The overlay layer's CSS (appended to the chrome's provider).
pub fn css(s: &Style) -> String {
    let Style {
        family,
        size,
        base,
        text,
        dim,
        rule,
        hover,
        accent,
        ..
    } = s;
    let family = family.replace('"', "");
    format!(
        r#"
#comux-overlay {{ background: alpha(black, 0.18); }}
#comux-overlay * {{ font-family: "{family}"; font-size: {size}pt; color: {text}; }}
#comux-overlay .comux-card {{ background: alpha({base}, 0.97); border: 1px solid {accent};
  border-radius: 8px; box-shadow: 0 6px 18px alpha(black, 0.35); }}
#comux-overlay .comux-dim, #comux-overlay .comux-dim * {{ color: {dim}; }}
#comux-overlay .comux-accent {{ color: {accent}; font-weight: bold; }}
#comux-overlay .comux-rule {{ background: {rule}; min-height: 1px; min-width: 1px; }}
#comux-overlay list, #comux-overlay row, #comux-overlay scrolledwindow, #comux-overlay viewport
  {{ background: transparent; box-shadow: none; }}
#comux-overlay row {{ padding: 0; border-radius: 5px; }}
#comux-overlay row.selected {{ background: {hover}; box-shadow: inset 3px 0 {accent}; }}
#comux-overlay entry {{ background: none; border: none; box-shadow: none; outline: none; min-height: 0; padding: 0; }}
#comux-overlay .comux-chip {{ background: alpha({rule}, 0.6); border-radius: 3px; padding: 0 6px; border: none; box-shadow: none; min-height: 0; }}
#comux-overlay .comux-chip.active {{ background: {accent}; }}
#comux-overlay .comux-chip.active label {{ color: {base}; font-weight: bold; }}
#comux-overlay .comux-button {{ background: alpha({rule}, 0.6); border-radius: 4px; border: none; box-shadow: none; padding: 2px 10px; min-height: 0; }}
#comux-overlay .comux-button.filled {{ background: {accent}; }}
#comux-overlay .comux-button.filled label {{ color: {base}; font-weight: bold; }}
#comux-overlay .comux-button.danger {{ background: {red}; }}
#comux-overlay .comux-button.danger label {{ color: {base}; font-weight: bold; }}
"#,
        red = s.red,
    )
}

// ---------------------------------------------------------------------------------------------
// Pure logic (mirrors the iOS/macOS overlay; both follow the TUI)
// ---------------------------------------------------------------------------------------------

/// What a key does in a picker.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Cancel,
    Answer(String),
    Act(String),
    Tab(i32),
    Move(i32),
    Pick,
    None,
}

/// The TUI's order: a declared key wins; then ←/→ (and Tab) switch tabs, ↑/↓ ^n/^p move,
/// Enter picks, Esc cancels. A printable declared key only counts without a filter field.
pub(crate) fn outcome(key: &str, spec: &Value, row: Option<&Value>) -> Outcome {
    if key == "Esc" {
        return Outcome::Cancel;
    }
    let no_filter = spec["no_filter"] == true;
    let typed = key.chars().count() == 1;
    if !typed || no_filter {
        if let Some(k) = spec["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|k| k["key"] == key)
        {
            let c = k["choice"].as_str().unwrap_or_default().to_string();
            return match (c.is_empty(), k["stays"] == true) {
                (true, _) => Outcome::Cancel,
                (false, true) => Outcome::Act(c),
                (false, false) => Outcome::Answer(c),
            };
        }
        if let Some(row) = row.filter(|r| r["no_actions"] != true)
            && let Some(a) = spec["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["key"] == key)
        {
            let c = format!(
                "{}:{}",
                a["id"].as_str().unwrap_or_default(),
                row["id"].as_str().unwrap_or_default()
            );
            return if a["stays"] == true {
                Outcome::Act(c)
            } else {
                Outcome::Answer(c)
            };
        }
    }
    let tabs = spec["tabs"].as_array().map_or(0, Vec::len);
    match key {
        "Left" if tabs > 1 => Outcome::Tab(-1),
        "Right" | "Tab" if tabs > 1 => Outcome::Tab(1),
        "Up" | "C-p" => Outcome::Move(-1),
        "Down" | "C-n" => Outcome::Move(1),
        "Enter" => Outcome::Pick,
        "k" if no_filter => Outcome::Move(-1),
        "j" if no_filter => Outcome::Move(1),
        "q" if no_filter => Outcome::Cancel,
        _ => Outcome::None,
    }
}

/// `!` working, `@` blocked, `#` ready/idle — the TUI switcher's status filters.
fn status_op(filter: &str) -> (Option<&'static [&'static str]>, &str) {
    let op: Option<&'static [&'static str]> = match filter.chars().next() {
        Some('!') => Some(&["working"]),
        Some('@') => Some(&["blocked"]),
        Some('#') => Some(&["ready", "idle"]),
        _ => None,
    };
    match op {
        Some(op) => (Some(op), filter[1..].trim_start()),
        None => (None, filter),
    }
}

fn subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Rows matching `filter`: contiguous matches before scattered ones, over label and detail; a
/// status op keeps rows whose agents (`statuses`) have one of its statuses.
pub(crate) fn filter_rows<'a>(
    rows: &'a [Value],
    filter: &str,
    statuses: impl Fn(&Value) -> Option<Vec<String>>,
) -> Vec<&'a Value> {
    let (op, rest) = status_op(filter);
    let all: Vec<&Value> = rows
        .iter()
        .filter(|r| match op {
            Some(op) => statuses(r).is_some_and(|s| s.iter().any(|x| op.contains(&x.as_str()))),
            None => true,
        })
        .collect();
    if rest.is_empty() {
        return all;
    }
    let f = rest.to_lowercase();
    let hay = |r: &Value| {
        format!(
            "{} {}",
            r["label"].as_str().unwrap_or_default(),
            r["detail"].as_str().unwrap_or_default()
        )
        .to_lowercase()
    };
    let (contig, rest): (Vec<&Value>, Vec<&Value>) =
        all.into_iter().partition(|r| hay(r).contains(&f));
    contig
        .into_iter()
        .chain(rest.into_iter().filter(|r| subsequence(&f, &hay(r))))
        .collect()
}

/// A preview colour: the theme for 0–15, xterm's cube and greys above, or rgb.
pub(crate) fn run_color(s: &Style, c: l0::RunColor) -> String {
    match c {
        l0::RunColor::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        l0::RunColor::Indexed(i) if i < 16 => {
            s.ansi.get(i as usize).cloned().unwrap_or(s.fg.clone())
        }
        l0::RunColor::Indexed(i) if i < 232 => {
            let n = i - 16;
            let step = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            format!(
                "#{:02x}{:02x}{:02x}",
                step(n / 36),
                step(n / 6 % 6),
                step(n % 6)
            )
        }
        l0::RunColor::Indexed(i) => {
            let g = 8 + (i - 232) * 10;
            format!("#{g:02x}{g:02x}{g:02x}")
        }
    }
}

/// One preview line as Pango markup.
pub(crate) fn line_markup(s: &Style, line: &[l0::Run]) -> String {
    let mut out = String::new();
    for r in line {
        let mut fg = r.fg.map_or(s.fg.clone(), |c| run_color(s, c));
        let mut bg = r.bg.map(|c| run_color(s, c));
        if r.reverse {
            let f = fg.clone();
            fg = bg.unwrap_or(s.base.clone());
            bg = Some(f);
        }
        let mut attrs = format!("foreground=\"{fg}\"");
        if let Some(bg) = bg {
            attrs.push_str(&format!(" background=\"{bg}\""));
        }
        if r.bold {
            attrs.push_str(" weight=\"bold\"");
        }
        if r.italic {
            attrs.push_str(" style=\"italic\"");
        }
        if r.underline {
            attrs.push_str(" underline=\"single\"");
        }
        if r.dim {
            attrs.push_str(" alpha=\"60%\"");
        }
        out.push_str(&format!(
            "<span {attrs}>{}</span>",
            gtk4::glib::markup_escape_text(&r.t)
        ));
    }
    out
}

/// A key press in comux's naming (`Left`, `C-r`, `d`); `None` for what the overlay leaves
/// alone (Super/Alt shortcuts).
fn key_name(key: gtk4::gdk::Key, state: gtk4::gdk::ModifierType) -> Option<String> {
    use gtk4::gdk::{Key, ModifierType};
    let named = match key {
        Key::Left => Some("Left"),
        Key::Right => Some("Right"),
        Key::Up => Some("Up"),
        Key::Down => Some("Down"),
        Key::Return | Key::KP_Enter => Some("Enter"),
        Key::Escape => Some("Esc"),
        Key::Tab | Key::ISO_Left_Tab => Some("Tab"),
        _ => None,
    };
    if let Some(n) = named {
        return Some(n.into());
    }
    if state.intersects(ModifierType::SUPER_MASK | ModifierType::ALT_MASK) {
        return None;
    }
    let c = key.to_lower().to_unicode()?;
    if state.contains(ModifierType::CONTROL_MASK) {
        return Some(format!("C-{c}"));
    }
    key.to_unicode()
        .filter(|c| !c.is_control())
        .map(String::from)
}

// ---------------------------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------------------------

/// The host's side of an overlay.
#[derive(Clone)]
pub struct Handlers {
    /// (choice, text): both `None` = cancelled. Closes the overlay.
    pub answer: Rc<dyn Fn(Option<String>, Option<String>)>,
    /// A staying choice: the overlay stays until comux re-sends it.
    pub act: Rc<dyn Fn(String)>,
    /// Keep the preview of a pane fresh; `None` stops it.
    pub preview: Rc<dyn Fn(Option<PreviewReq>)>,
}

/// One shown overlay: the layer over the window and, for a picker, its live state.
pub struct OverlayUi {
    pub id: u64,
    pub layer: gtk4::Box,
    card: gtk4::Box,
    picker: Option<Rc<PickerUi>>,
    /// Who had focus before; it gets it back on close.
    restore: Option<gtk4::Widget>,
}

impl OverlayUi {
    pub fn new(
        id: u64,
        spec: &Value,
        state: &Value,
        style: &Style,
        handlers: Handlers,
        window: &gtk4::ApplicationWindow,
    ) -> OverlayUi {
        let restore = gtk4::prelude::GtkWindowExt::focus(window);
        let layer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        layer.set_widget_name("comux-overlay");
        layer.set_hexpand(true);
        layer.set_vexpand(true);
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        card.add_css_class("comux-card");
        card.set_halign(gtk4::Align::Center);
        card.set_valign(gtk4::Align::Center);
        card.set_vexpand(true);
        card.set_focusable(true);
        card.set_overflow(gtk4::Overflow::Hidden);
        layer.append(&card);
        // A click outside the card cancels; inside, it is the card's.
        let click = gtk4::GestureClick::new();
        let (c2, a2) = (card.clone(), handlers.answer.clone());
        let l2 = layer.clone();
        click.connect_pressed(move |_, _, x, y| {
            let inside = c2
                .compute_bounds(&l2)
                .is_some_and(|b| b.contains_point(&gtk4::graphene::Point::new(x as f32, y as f32)));
            if !inside {
                a2(None, None);
            }
        });
        layer.add_controller(click);
        let (ww, wh) = (window.width().max(400), window.height().max(300));
        let picker = match spec["kind"].as_str() {
            Some("picker") => Some(PickerUi::build(
                &card,
                spec,
                state,
                style,
                handlers,
                (ww, wh),
            )),
            Some("prompt") => {
                build_prompt(&card, spec, &handlers, (ww, wh));
                None
            }
            Some("confirm") => {
                build_confirm(&card, spec, &handlers, (ww, wh));
                None
            }
            Some("menu") => {
                build_menu(&card, spec, &handlers, (ww, wh));
                None
            }
            // A kind this build doesn't know: answered as cancelled, never guessed at.
            _ => {
                let a = handlers.answer.clone();
                gtk4::glib::idle_add_local_once(move || a(None, None));
                None
            }
        };
        OverlayUi {
            id,
            layer,
            card,
            picker,
            restore,
        }
    }

    /// Give the card the keyboard (the filter field, when there is one).
    pub fn focus(&self) {
        match self.picker.as_ref().and_then(|p| p.entry.clone()) {
            Some(e) => {
                e.grab_focus();
            }
            None => {
                if !focus_first_entry(&self.card) {
                    self.card.grab_focus();
                }
            }
        }
    }

    /// The same overlay again (rows landed, a live re-send) or the topics moved: update in
    /// place, keeping the filter, tab and selection.
    pub fn update(&self, spec: &Value, state: &Value) {
        if let Some(p) = &self.picker {
            p.update(spec, state);
        }
    }

    pub fn close(&self) {
        if let Some(p) = &self.picker {
            (p.handlers.preview)(None);
        }
        if let Some(w) = &self.restore {
            w.grab_focus();
        }
    }
}

fn focus_first_entry(w: &gtk4::Box) -> bool {
    let mut c = w.first_child();
    while let Some(ch) = c {
        if let Some(e) = ch.downcast_ref::<gtk4::Entry>() {
            return e.grab_focus();
        }
        if let Some(b) = ch.downcast_ref::<gtk4::Box>()
            && focus_first_entry(b)
        {
            return true;
        }
        c = ch.next_sibling();
    }
    false
}

fn label(markup: &str) -> gtk4::Label {
    let l = gtk4::Label::new(None);
    l.set_markup(markup);
    l.set_xalign(0.0);
    l.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    l
}

fn rule() -> gtk4::Separator {
    let r = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    r.add_css_class("comux-rule");
    r
}

fn button(text: &str, class: Option<&str>, on: impl Fn() + 'static) -> gtk4::Button {
    let b = gtk4::Button::with_label(text);
    b.add_css_class("comux-button");
    if let Some(c) = class {
        b.add_css_class(c);
    }
    b.connect_clicked(move |_| on());
    b
}

/// `C-r` → `^R`, `Right` → `→`.
fn key_label(k: &str) -> String {
    match k {
        "Left" => "←".into(),
        "Right" => "→".into(),
        k if k.starts_with("C-") => format!("^{}", k[2..].to_uppercase()),
        k => k.into(),
    }
}

fn keys_controller(card: &gtk4::Box, on: impl Fn(&str) -> bool + 'static) {
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, state| match key_name(key, state) {
        Some(name) if on(&name) => gtk4::glib::Propagation::Stop,
        _ => gtk4::glib::Propagation::Proceed,
    });
    card.add_controller(keys);
}

fn build_prompt(card: &gtk4::Box, spec: &Value, h: &Handlers, (ww, _): (i32, i32)) {
    card.set_size_request((ww - 32).min(460), -1);
    let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    vb.set_margin_top(16);
    vb.set_margin_bottom(16);
    vb.set_margin_start(16);
    vb.set_margin_end(16);
    let title = label(&gtk4::glib::markup_escape_text(
        spec["title"].as_str().unwrap_or_default(),
    ));
    title.add_css_class("comux-accent");
    vb.append(&title);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let gt = gtk4::Label::new(Some(">"));
    gt.add_css_class("comux-accent");
    row.append(&gt);
    let entry = gtk4::Entry::new();
    entry.set_hexpand(true);
    entry.set_text(spec["initial"].as_str().unwrap_or_default());
    let a = h.answer.clone();
    entry.connect_activate(move |e| a(None, Some(e.text().to_string())));
    row.append(&entry);
    vb.append(&row);
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons.set_halign(gtk4::Align::End);
    let a = h.answer.clone();
    buttons.append(&button("Cancel", None, move || a(None, None)));
    let (a, e2) = (h.answer.clone(), entry.clone());
    buttons.append(&button("OK", Some("filled"), move || {
        a(None, Some(e2.text().to_string()))
    }));
    vb.append(&buttons);
    card.append(&vb);
    let a = h.answer.clone();
    keys_controller(card, move |k| {
        if k == "Esc" {
            a(None, None);
            return true;
        }
        false
    });
}

fn build_confirm(card: &gtk4::Box, spec: &Value, h: &Handlers, (ww, _): (i32, i32)) {
    card.set_size_request((ww - 32).min(460), -1);
    let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    vb.set_margin_top(18);
    vb.set_margin_bottom(18);
    vb.set_margin_start(18);
    vb.set_margin_end(18);
    let msg = gtk4::Label::new(None);
    msg.set_markup(&bold(
        gtk4::glib::markup_escape_text(spec["message"].as_str().unwrap_or_default()).to_string(),
    ));
    msg.set_wrap(true);
    vb.append(&msg);
    let hint = gtk4::Label::new(Some("y = yes · n / esc = no"));
    hint.add_css_class("comux-dim");
    vb.append(&hint);
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons.set_halign(gtk4::Align::Center);
    let a = h.answer.clone();
    buttons.append(&button("Cancel", None, move || a(None, None)));
    let a = h.answer.clone();
    buttons.append(&button("Confirm", Some("danger"), move || {
        a(Some("yes".into()), None)
    }));
    vb.append(&buttons);
    card.append(&vb);
    let a = h.answer.clone();
    keys_controller(card, move |k| match k {
        "y" | "Y" => {
            a(Some("yes".into()), None);
            true
        }
        "n" | "N" | "q" | "Esc" => {
            a(None, None);
            true
        }
        _ => false,
    });
}

fn build_menu(card: &gtk4::Box, spec: &Value, h: &Handlers, (ww, _): (i32, i32)) {
    card.set_size_request((ww - 32).min(320), -1);
    let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    vb.set_margin_top(10);
    vb.set_margin_bottom(10);
    vb.set_margin_start(10);
    vb.set_margin_end(10);
    if let Some(t) = spec["title"].as_str().filter(|t| !t.is_empty()) {
        let l = label(&gtk4::glib::markup_escape_text(t));
        l.add_css_class("comux-accent");
        vb.append(&l);
    }
    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::None);
    list.set_activate_on_single_click(true);
    let mut ids = Vec::new();
    for item in spec["items"].as_array().into_iter().flatten() {
        let text = gtk4::glib::markup_escape_text(item["label"].as_str().unwrap_or_default());
        let l = label(&if item["destructive"] == true {
            format!("<span foreground=\"red\">{text}</span>")
        } else {
            text.to_string()
        });
        l.set_margin_start(8);
        l.set_margin_top(4);
        l.set_margin_bottom(4);
        list.append(&l);
        ids.push(item["id"].as_str().unwrap_or_default().to_string());
    }
    let ids = Rc::new(ids);
    let sel = Rc::new(Cell::new(0usize));
    let mark = {
        let list = list.clone();
        let sel = sel.clone();
        move || {
            let mut i = 0;
            while let Some(r) = list.row_at_index(i) {
                if i as usize == sel.get() {
                    r.add_css_class("selected");
                } else {
                    r.remove_css_class("selected");
                }
                i += 1;
            }
        }
    };
    mark();
    let (a, ids2) = (h.answer.clone(), ids.clone());
    list.connect_row_activated(move |_, r| {
        if let Some(id) = ids2.get(r.index() as usize) {
            a(Some(id.clone()), None);
        }
    });
    vb.append(&list);
    card.append(&vb);
    let a = h.answer.clone();
    keys_controller(card, move |k| {
        let n = ids.len().max(1);
        match k {
            "Up" | "k" => sel.set((sel.get() + n - 1) % n),
            "Down" | "j" => sel.set((sel.get() + 1) % n),
            "Enter" => {
                if let Some(id) = ids.get(sel.get()) {
                    a(Some(id.clone()), None);
                }
                return true;
            }
            "Esc" | "q" => {
                a(None, None);
                return true;
            }
            _ => return false,
        }
        mark();
        true
    });
}

/// A picker card: header, tab chips, filter line, card rows and the live preview.
struct PickerUi {
    spec: RefCell<Value>,
    state: RefCell<Value>,
    style: Style,
    handlers: Handlers,
    tab: Cell<usize>,
    sel: RefCell<Option<String>>,
    last_index: Cell<usize>,
    entry: Option<gtk4::Entry>,
    title: gtk4::Label,
    hints: gtk4::Label,
    note: gtk4::Label,
    chips: gtk4::Box,
    list: gtk4::ListBox,
    scroll: gtk4::ScrolledWindow,
    /// The rows the list shows, in order (a row's index is its position here).
    shown: RefCell<Vec<Value>>,
    preview: Option<(gtk4::Box, gtk4::Label, gtk4::Label)>,
    card: gtk4::Box,
    window: (i32, i32),
    list_width: i32,
    /// The cell size of the preview font, for sizing the request.
    cell: (i32, i32),
    /// What the preview last asked for.
    asked: RefCell<Option<PreviewReq>>,
    /// Itself, for the chips rebuilt on every refill.
    me: RefCell<std::rc::Weak<PickerUi>>,
}

impl PickerUi {
    fn build(
        card: &gtk4::Box,
        spec: &Value,
        state: &Value,
        style: &Style,
        handlers: Handlers,
        (ww, wh): (i32, i32),
    ) -> Rc<PickerUi> {
        // The preview pane is built whenever the window has room for one and shown while the
        // rows have panes to preview — rows that land later (a resume scan) get it too.
        let wide = ww >= 820 && wh >= 420;
        let cw = ww * 9 / 10;
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        head.set_margin_start(12);
        head.set_margin_end(12);
        head.set_margin_top(7);
        head.set_margin_bottom(7);
        let title = label("");
        title.add_css_class("comux-accent");
        head.append(&title);
        let hints = label("");
        hints.add_css_class("comux-dim");
        hints.set_hexpand(true);
        hints.set_xalign(1.0);
        head.append(&hints);
        let a = handlers.answer.clone();
        let x = button("✕", None, move || a(None, None));
        x.add_css_class("comux-dim");
        head.append(&x);
        card.append(&head);
        let chips = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        chips.set_margin_start(12);
        chips.set_margin_bottom(6);
        card.append(&chips);
        let entry = (spec["no_filter"] != true).then(|| {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            row.set_margin_start(12);
            row.set_margin_end(12);
            row.set_margin_bottom(8);
            let gt = gtk4::Label::new(Some(">"));
            gt.add_css_class("comux-accent");
            row.append(&gt);
            let e = gtk4::Entry::new();
            e.set_hexpand(true);
            e.set_placeholder_text(Some("type to filter"));
            row.append(&e);
            card.append(&row);
            e
        });
        let note = label("");
        note.set_margin_start(12);
        note.set_margin_bottom(4);
        card.append(&note);
        card.append(&rule());
        let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        body.set_vexpand(true);
        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.set_activate_on_single_click(true);
        list.set_margin_start(6);
        list.set_margin_end(6);
        list.set_margin_top(6);
        let scroll = gtk4::ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_hscrollbar_policy(gtk4::PolicyType::Never);
        scroll.set_vexpand(true);
        let measure = card.create_pango_layout(Some("MMMMMMMMMM"));
        let (mw, mh) = measure.pixel_size();
        let cell = ((mw / 10).max(4), mh.max(8));
        let preview = wide.then(|| {
            body.append(&scroll);
            let sep = gtk4::Separator::new(gtk4::Orientation::Vertical);
            sep.add_css_class("comux-rule");
            body.append(&sep);
            let pv = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            pv.set_hexpand(true);
            pv.set_vexpand(true);
            pv.set_margin_start(10);
            pv.set_margin_end(10);
            pv.set_margin_top(8);
            pv.set_margin_bottom(8);
            pv.set_overflow(gtk4::Overflow::Hidden);
            let ph = label("");
            ph.add_css_class("comux-dim");
            let pb = gtk4::Label::new(None);
            pb.set_xalign(0.0);
            pb.set_yalign(1.0);
            pb.set_vexpand(true);
            pb.set_valign(gtk4::Align::End);
            pb.set_wrap(false);
            pb.set_single_line_mode(false);
            pv.append(&ph);
            pv.append(&pb);
            body.append(&pv);
            (pv, ph, pb)
        });
        if preview.is_none() {
            body.append(&scroll);
        }
        card.append(&body);
        let ui = Rc::new(PickerUi {
            spec: RefCell::new(spec.clone()),
            state: RefCell::new(state.clone()),
            style: style.clone(),
            handlers,
            tab: Cell::new(
                spec["tab"].as_u64().unwrap_or(0).min(
                    spec["tabs"]
                        .as_array()
                        .map_or(1, Vec::len)
                        .saturating_sub(1) as u64,
                ) as usize,
            ),
            sel: RefCell::new(spec["selected"].as_str().map(str::to_string)),
            last_index: Cell::new(0),
            entry,
            title,
            hints,
            note,
            chips,
            list,
            scroll,
            shown: RefCell::new(Vec::new()),
            preview,
            card: card.clone(),
            window: (ww, wh),
            list_width: (cw * 2 / 5).max(30 * cell.0).min(cw),
            cell,
            asked: RefCell::new(None),
            me: RefCell::new(std::rc::Weak::new()),
        });
        *ui.me.borrow_mut() = Rc::downgrade(&ui);
        let w = Rc::downgrade(&ui);
        ui.list.connect_row_activated(move |_, r| {
            if let Some(ui) = w.upgrade()
                && let Some(id) = ui
                    .shown
                    .borrow()
                    .get(r.index() as usize)
                    .and_then(|x| x["id"].as_str())
            {
                (ui.handlers.answer)(Some(id.to_string()), None);
            }
        });
        if let Some(e) = &ui.entry {
            let w = Rc::downgrade(&ui);
            e.connect_changed(move |_| {
                if let Some(ui) = w.upgrade() {
                    ui.refill();
                }
            });
            let w = Rc::downgrade(&ui);
            e.connect_activate(move |_| {
                if let Some(ui) = w.upgrade() {
                    ui.run(Outcome::Pick);
                }
            });
        }
        let w = Rc::downgrade(&ui);
        keys_controller(card, move |k| {
            let Some(ui) = w.upgrade() else { return false };
            let row = ui.current();
            let o = outcome(k, &ui.spec.borrow(), row.as_ref());
            if o == Outcome::None {
                return false;
            }
            ui.run(o);
            true
        });
        // Right-click a row: its actions.
        let rc = gtk4::GestureClick::new();
        rc.set_button(3);
        let w = Rc::downgrade(&ui);
        rc.connect_pressed(move |g, _, x, y| {
            let Some(ui) = w.upgrade() else { return };
            let Some(r) = ui.list.row_at_y(y as i32) else {
                return;
            };
            let Some(row) = ui.shown.borrow().get(r.index() as usize).cloned() else {
                return;
            };
            if row["no_actions"] == true {
                return;
            }
            g.set_state(gtk4::EventSequenceState::Claimed);
            ui.row_menu(&row, x, y);
        });
        ui.list.add_controller(rc);
        ui.refill();
        ui
    }

    fn rows_of_tab(&self) -> Vec<Value> {
        self.spec.borrow()["tabs"]
            .get(self.tab.get())
            .and_then(|t| t["rows"].as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn statuses(&self, r: &Value) -> Vec<String> {
        let st = self.state.borrow();
        let agents = st["agents"]["agents"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let session = r["session"].as_str().unwrap_or_default();
        agents
            .iter()
            .filter(|a| {
                if !session.is_empty() {
                    a["space_id"] == session
                } else {
                    r["pane"].as_str().is_some_and(|p| a["token"] == p)
                }
            })
            .filter_map(|a| a["status"].as_str().map(str::to_string))
            .collect()
    }

    /// The row Enter picks: the highlighted one if still listed, else the one now at its old
    /// place, else the first.
    fn current(&self) -> Option<Value> {
        let shown = self.shown.borrow();
        let sel = self.sel.borrow();
        shown
            .iter()
            .find(|r| sel.as_deref().is_some_and(|s| r["id"] == s))
            .or_else(|| shown.get(self.last_index.get().min(shown.len().saturating_sub(1))))
            .cloned()
    }

    fn update(&self, spec: &Value, state: &Value) {
        let loading_done = self.spec.borrow()["loading"] == true && spec["loading"] != true;
        *self.spec.borrow_mut() = spec.clone();
        *self.state.borrow_mut() = state.clone();
        if loading_done {
            // The rows landed: take the picker's default row now.
            *self.sel.borrow_mut() = spec["selected"].as_str().map(str::to_string);
        }
        self.refill();
    }

    /// Rebuild the rows (filter, tab, live topics), the header and the preview.
    fn refill(&self) {
        let spec = self.spec.borrow().clone();
        let s = &self.style;
        let mut title = bold(span(&s.accent, spec["title"].as_str().unwrap_or_default()));
        if spec["loading"] == true {
            title.push_str(&span(&s.dim, "  loading…"));
        }
        self.title.set_markup(&title);
        let mut hints = vec!["↵ go".to_string()];
        let tabs = spec["tabs"].as_array().cloned().unwrap_or_default();
        if tabs.len() > 1 {
            hints.push("←/→ tab".into());
        }
        for a in spec["actions"].as_array().into_iter().flatten() {
            if let Some(k) = a["key"].as_str().filter(|k| !k.is_empty()) {
                hints.push(format!(
                    "{} {}",
                    key_label(k),
                    a["label"].as_str().unwrap_or_default().to_lowercase()
                ));
            }
        }
        for k in spec["keys"].as_array().into_iter().flatten() {
            if let Some(c) = k["choice"].as_str().filter(|c| !c.is_empty()) {
                hints.push(format!(
                    "{} {}",
                    key_label(k["key"].as_str().unwrap_or_default()),
                    c.replace('-', " ")
                ));
            }
        }
        hints.push("esc".into());
        self.hints.set_text(&hints.join(" · "));
        let note = spec["note"].as_str().unwrap_or_default();
        self.note.set_visible(!note.is_empty());
        self.note.set_markup(&span(&s.peach, note));

        // Tab chips.
        while let Some(c) = self.chips.first_child() {
            self.chips.remove(&c);
        }
        self.chips.set_visible(tabs.len() > 1);
        if tabs.len() > 1 {
            for (i, t) in tabs.iter().enumerate() {
                let b = gtk4::Button::with_label(t["name"].as_str().unwrap_or_default());
                b.add_css_class("comux-chip");
                if i == self.tab.get() {
                    b.add_css_class("active");
                }
                let me = self.me.borrow().clone();
                b.connect_clicked(move |_| {
                    if let Some(ui) = me.upgrade() {
                        let d = i as i32 - ui.tab.get() as i32;
                        ui.run(Outcome::Tab(d));
                    }
                });
                self.chips.append(&b);
            }
        }

        // Rows.
        let all = self.rows_of_tab();
        let joins = all.iter().any(|r| {
            r["pane"].as_str().is_some_and(|p| !p.is_empty())
                || r["session"].as_str().is_some_and(|x| !x.is_empty())
        });
        let filter = self
            .entry
            .as_ref()
            .map(|e| e.text().to_string())
            .unwrap_or_default();
        let shown: Vec<Value> = filter_rows(&all, &filter, |r| joins.then(|| self.statuses(r)))
            .into_iter()
            .cloned()
            .collect();
        *self.shown.borrow_mut() = shown.clone();
        self.size_card(spec["tabs"].as_array().into_iter().flatten().any(|t| {
            t["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|r| r["pane"].as_str().is_some_and(|p| !p.is_empty()))
        }));
        let cur = self
            .current()
            .and_then(|r| r["id"].as_str().map(str::to_string));
        if let Some(c) = &cur {
            *self.sel.borrow_mut() = Some(c.clone());
        }
        // By row, not `first_child`: a widget parented to the list that is not a row (a popover)
        // can never be removed, and that loop would spin forever.
        while let Some(r) = self.list.row_at_index(0) {
            self.list.remove(&r);
        }
        if shown.is_empty() && spec["loading"] != true {
            let l = label(&span(&s.dim, "(no match)"));
            l.set_margin_start(8);
            l.set_margin_top(6);
            self.list.append(&l);
            if let Some(r) = self.list.row_at_index(0) {
                r.set_activatable(false);
            }
        }
        for (i, r) in shown.iter().enumerate() {
            let w = self.card(r);
            self.list.append(&w);
            if cur.as_deref().is_some_and(|c| r["id"] == c)
                && let Some(row) = self.list.row_at_index(i as i32)
            {
                row.add_css_class("selected");
                self.last_index.set(i);
            }
        }
        self.refresh_preview();
    }

    /// One row as a card, from the live topics where it joins.
    fn card(&self, r: &Value) -> gtk4::Box {
        let s = &self.style;
        let st = self.state.borrow();
        let b = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
        b.set_margin_start(8);
        b.set_margin_end(8);
        b.set_margin_top(4);
        b.set_margin_bottom(4);
        let session = r["session"]
            .as_str()
            .filter(|x| !x.is_empty())
            .and_then(|id| {
                st["tree"]["sessions"]
                    .as_array()
                    .and_then(|a| a.iter().find(|x| x["id"] == id))
                    .cloned()
            });
        let agents = st["agents"]["agents"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let agent = r["id"]
            .as_str()
            .filter(|id| id.starts_with("a:"))
            .and_then(|_| {
                agents
                    .iter()
                    .find(|a| r["pane"].as_str().is_some_and(|p| a["token"] == p))
            });
        let (line1, line2) = if let Some(x) = &session {
            let mine: Vec<&Value> = agents.iter().filter(|a| a["space_id"] == x["id"]).collect();
            let active = x["active"] == true;
            let color = if active {
                &s.mauve
            } else if mine.is_empty() {
                &s.dim
            } else {
                &s.peach
            };
            let name = x["name"].as_str().unwrap_or_default();
            let glyphs: String = mine
                .iter()
                .map(|a| {
                    let (g, c) = s.status(a["status"].as_str().unwrap_or_default());
                    span(c, g)
                })
                .collect();
            let tabs = x["tabs"].as_array().map_or(0, Vec::len);
            let sub = [
                crate::comux_panel::space_subtitle(
                    x["branch"].as_str().unwrap_or_default(),
                    x["tabs"].as_array().map_or(&[][..], Vec::as_slice),
                )
                .trim()
                .to_string(),
                format!("{tabs} tab{}", if tabs == 1 { "" } else { "s" }),
                short_dir(x["dir"].as_str().unwrap_or_default()),
            ]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
            (
                format!(
                    "{} {}  {glyphs}",
                    span(color, "▪"),
                    bold(span(if active { &s.mauve } else { &s.text }, name))
                ),
                span(&s.dim, &format!("  {sub}")),
            )
        } else if let Some(a) = agent {
            let status = a["status"].as_str().unwrap_or_default();
            let (g, c) = s.status(status);
            let since = a["since_unix"].as_i64().map(elapsed).unwrap_or_default();
            let space = a["space"].as_str().unwrap_or_default();
            let title = a["title"].as_str().unwrap_or_default();
            let place = if title == space {
                space.to_string()
            } else {
                format!("{space} · {title}")
            };
            let detail = a["detail"]
                .as_str()
                .map(|d| format!("{place} — {d}"))
                .unwrap_or(place);
            (
                format!(
                    "{} {} {}",
                    span(c, g),
                    bold(span(&s.text, a["tool"].as_str().unwrap_or_default())),
                    span(c, &format!("{status} {since}"))
                ),
                span(&s.dim, &format!("  {detail}")),
            )
        } else {
            let badge = r["badge"].as_str().unwrap_or_default();
            let (g, c) = s.status(badge);
            let glyph = if badge.is_empty() || badge == "live" {
                String::new()
            } else {
                format!("{} ", span(c, g))
            };
            let label_text = r["label"].as_str().unwrap_or_default();
            let name = if r["current"] == true {
                bold(span(&s.text, label_text))
            } else {
                span(&s.text, label_text)
            };
            let tail = match badge {
                "" => String::new(),
                "live" => format!("  {}", span(&s.green, "live")),
                b => format!("  {}", span(c, b)),
            };
            (
                format!("{glyph}{name}{tail}"),
                span(&s.dim, r["detail"].as_str().unwrap_or_default()),
            )
        };
        b.append(&label(&line1));
        if !line2.is_empty() && line2 != span(&s.dim, "") {
            b.append(&label(&line2));
        }
        b
    }

    /// The TUI's sizes: a picker with panes to preview takes most of the window and splits,
    /// the rest stay small.
    fn size_card(&self, previews: bool) {
        let (ww, wh) = self.window;
        let on = previews && self.preview.is_some();
        if let Some((pv, _, _)) = &self.preview {
            pv.set_visible(on);
            if let Some(sep) = pv.prev_sibling() {
                sep.set_visible(on);
            }
        }
        self.scroll.set_hexpand(!on);
        self.scroll
            .set_size_request(if on { self.list_width } else { -1 }, -1);
        if on {
            self.card.set_size_request(ww * 9 / 10, wh * 4 / 5);
        } else {
            self.card.set_size_request(
                (ww * 6 / 10).max(420).min(ww - 32),
                (wh * 6 / 10).max(320).min(wh - 32),
            );
        }
    }

    fn refresh_preview(&self) {
        let Some((pv, head, body)) = &self.preview else {
            return;
        };
        if !pv.is_visible() {
            // Nothing to preview any more (the center cleared): stop polling.
            if self.asked.borrow_mut().take().is_some() {
                (self.handlers.preview)(None);
            }
            return;
        }
        let pane = self
            .current()
            .and_then(|r| r["pane"].as_str().map(str::to_string))
            .filter(|p| !p.is_empty());
        let (w, h) = (pv.width(), pv.height());
        let rows = if h > 0 {
            (h / self.cell.1 - 1).max(1)
        } else {
            30
        } as u16;
        let cols = if w > 0 { (w / self.cell.0).max(1) } else { 100 } as u16;
        let req = pane.clone().map(|pane| PreviewReq { pane, rows, cols });
        if *self.asked.borrow() != req {
            *self.asked.borrow_mut() = req.clone();
            (self.handlers.preview)(req);
        }
        let s = &self.style;
        let st = &self.state.borrow()["previews"]["overlay"];
        let Some(pane) = pane else {
            head.set_markup("");
            body.set_markup(&span(&s.dim, "nothing to preview"));
            return;
        };
        match st["state"].as_str() {
            Some("unsupported") => {
                head.set_markup("");
                body.set_markup(&span(&s.dim, "preview needs a newer comux"));
            }
            Some("gone") => {
                head.set_markup("");
                body.set_markup(&span(&s.dim, "that pane has closed"));
            }
            _ => match serde_json::from_value::<l0::Preview>(st["preview"].clone()) {
                Ok(p) if p.pane == pane => {
                    head.set_markup(&bold(span(&s.subtext, &p.header)));
                    let lines: Vec<String> = p
                        .lines
                        .iter()
                        .skip(p.lines.len().saturating_sub(rows as usize))
                        .map(|l| line_markup(s, l))
                        .collect();
                    body.set_markup(&lines.join("\n"));
                }
                _ => {
                    head.set_markup("");
                    body.set_markup(&span(&s.dim, "…"));
                }
            },
        }
    }

    fn run(&self, o: Outcome) {
        match o {
            Outcome::Cancel => (self.handlers.answer)(None, None),
            Outcome::Answer(c) => (self.handlers.answer)(Some(c), None),
            Outcome::Act(c) => (self.handlers.act)(c),
            Outcome::Tab(d) => {
                let n = self.spec.borrow()["tabs"]
                    .as_array()
                    .map_or(1, Vec::len)
                    .max(1) as i32;
                self.tab
                    .set(((self.tab.get() as i32 + d).rem_euclid(n)) as usize);
                *self.sel.borrow_mut() = None;
                self.last_index.set(0);
                self.refill();
            }
            Outcome::Move(d) => {
                let shown = self.shown.borrow().clone();
                if shown.is_empty() {
                    return;
                }
                let cur = self.current();
                let i = shown
                    .iter()
                    .position(|r| cur.as_ref().is_some_and(|c| r["id"] == c["id"]))
                    .unwrap_or(0) as i32;
                let n = shown.len() as i32;
                let next = ((i + d).rem_euclid(n)) as usize;
                *self.sel.borrow_mut() = shown[next]["id"].as_str().map(str::to_string);
                self.last_index.set(next);
                self.mark(next);
            }
            Outcome::Pick => {
                if let Some(id) = self
                    .current()
                    .and_then(|r| r["id"].as_str().map(str::to_string))
                {
                    (self.handlers.answer)(Some(id), None);
                }
            }
            Outcome::None => {}
        }
    }

    /// Move the highlight without rebuilding (keeps scroll), and bring it into view.
    fn mark(&self, i: usize) {
        let mut k = 0;
        while let Some(r) = self.list.row_at_index(k) {
            if k as usize == i {
                r.add_css_class("selected");
                if let Some(b) = r.compute_bounds(&self.list) {
                    let adj = self.scroll.vadjustment();
                    let (top, bottom) = (b.y() as f64, (b.y() + b.height()) as f64);
                    if top < adj.value() {
                        adj.set_value(top);
                    } else if bottom > adj.value() + adj.page_size() {
                        adj.set_value(bottom - adj.page_size());
                    }
                }
            } else {
                r.remove_css_class("selected");
            }
            k += 1;
        }
        self.refresh_preview();
    }

    fn row_menu(&self, row: &Value, x: f64, y: f64) {
        // Parented to the card, which is never cleared: the list is rebuilt under it whenever
        // the overlay updates.
        let pop = gtk4::Popover::new();
        pop.set_parent(&self.card);
        let (x, y) = self
            .list
            .compute_point(&self.card, &gtk4::graphene::Point::new(x as f32, y as f32))
            .map_or((x, y), |p| (p.x() as f64, p.y() as f64));
        pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let vb = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        for a in self.spec.borrow()["actions"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let b = gtk4::Button::with_label(a["label"].as_str().unwrap_or_default());
            b.add_css_class("comux-flat");
            let choice = format!(
                "{}:{}",
                a["id"].as_str().unwrap_or_default(),
                row["id"].as_str().unwrap_or_default()
            );
            let stays = a["stays"] == true;
            let h = self.handlers.clone();
            let p2 = pop.clone();
            b.connect_clicked(move |_| {
                p2.popdown();
                if stays {
                    (h.act)(choice.clone());
                } else {
                    (h.answer)(Some(choice.clone()), None);
                }
            });
            vb.append(&b);
        }
        pop.set_child(Some(&vb));
        pop.connect_closed(|p| p.unparent());
        pop.popup();
    }
}

fn short_dir(d: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && d.starts_with(&h) => format!("~{}", &d[h.len()..]),
        _ => d.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn picker(extra: Value) -> Value {
        let mut p = json!({"kind": "picker", "title": "Switch",
            "tabs": [{"name": "sessions", "rows": []}, {"name": "agents", "rows": []}]});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    }

    #[test]
    fn arrows_switch_tabs_and_declared_keys_win() {
        let p = picker(json!({
            "actions": [{"id": "rename", "label": "Rename", "key": "C-r"}],
            "keys": [{"key": "C-f", "choice": ""}]
        }));
        let r = json!({"id": "s:a"});
        assert_eq!(outcome("Left", &p, Some(&r)), Outcome::Tab(-1));
        assert_eq!(outcome("Right", &p, Some(&r)), Outcome::Tab(1));
        assert_eq!(outcome("C-n", &p, Some(&r)), Outcome::Move(1));
        assert_eq!(outcome("Enter", &p, Some(&r)), Outcome::Pick);
        assert_eq!(
            outcome("C-r", &p, Some(&r)),
            Outcome::Answer("rename:s:a".into())
        );
        assert_eq!(outcome("C-f", &p, Some(&r)), Outcome::Cancel);
        let agent = json!({"id": "a:t1", "no_actions": true});
        assert_eq!(outcome("C-r", &p, Some(&agent)), Outcome::None);
        assert_eq!(
            outcome("x", &p, Some(&r)),
            Outcome::None,
            "typing is the filter's"
        );
    }

    #[test]
    fn the_center_letters_stay_open_and_the_browser_arrows_answer() {
        let center = json!({"kind": "picker", "tabs": [{"name": "n", "rows": []}], "no_filter": true,
            "actions": [{"id": "dismiss", "label": "Dismiss", "key": "d", "stays": true}],
            "keys": [{"key": "D", "choice": "clear-all", "stays": true}]});
        let n = json!({"id": "n:4"});
        assert_eq!(
            outcome("d", &center, Some(&n)),
            Outcome::Act("dismiss:n:4".into())
        );
        assert_eq!(
            outcome("D", &center, None),
            Outcome::Act("clear-all".into())
        );
        assert_eq!(outcome("j", &center, None), Outcome::Move(1));
        assert_eq!(outcome("Left", &center, None), Outcome::None, "one tab");
        let browser = picker(json!({
            "actions": [{"id": "open", "label": "Open", "key": "Right"}],
            "keys": [{"key": "Left", "choice": "up"}]
        }));
        let d = json!({"id": "d:2"});
        assert_eq!(
            outcome("Left", &browser, Some(&d)),
            Outcome::Answer("up".into())
        );
        assert_eq!(
            outcome("Right", &browser, Some(&d)),
            Outcome::Answer("open:d:2".into())
        );
    }

    #[test]
    fn status_filters_and_fuzzy_matching() {
        let rows = vec![
            json!({"id": "s:a", "label": "alpha", "session": "a"}),
            json!({"id": "s:b", "label": "beta", "session": "b"}),
            json!({"id": "a:1", "label": "claude", "detail": "webpack", "pane": "p1"}),
        ];
        let statuses = |r: &Value| {
            Some(match r["id"].as_str().unwrap() {
                "s:a" => vec!["working".to_string()],
                "a:1" => vec!["blocked".to_string()],
                _ => vec![],
            })
        };
        let ids = |v: Vec<&Value>| {
            v.iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(filter_rows(&rows, "!", statuses)), ["s:a"]);
        assert_eq!(ids(filter_rows(&rows, "@", statuses)), ["a:1"]);
        assert_eq!(ids(filter_rows(&rows, "webp", statuses)), ["a:1"]);
        assert_eq!(
            ids(filter_rows(&rows, "ah", statuses)),
            ["s:a"],
            "scattered match"
        );
    }

    #[test]
    fn preview_markup_maps_the_palette_and_escapes() {
        let mut s = Style::from_config(&copad_core::config::CopadConfig::default());
        s.ansi[1] = "#ff0000".into();
        let line = vec![
            l0::Run {
                t: "<a&b>".into(),
                fg: Some(l0::RunColor::Indexed(1)),
                bold: true,
                ..Default::default()
            },
            l0::Run {
                t: "x".into(),
                fg: Some(l0::RunColor::Indexed(196)),
                bg: Some(l0::RunColor::Rgb(1, 2, 3)),
                ..Default::default()
            },
        ];
        let m = line_markup(&s, &line);
        assert!(
            m.contains("foreground=\"#ff0000\" weight=\"bold\">&lt;a&amp;b&gt;"),
            "{m}"
        );
        assert!(
            m.contains("foreground=\"#ff0000\" background=\"#010203\">x"),
            "{m}"
        );
        assert_eq!(run_color(&s, l0::RunColor::Indexed(232)), "#080808");
    }
}
