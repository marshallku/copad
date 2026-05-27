use gtk4::prelude::*;
use gtk4::{gdk, glib};
use vte4::prelude::*;

const PCRE2_CASELESS: u32 = 0x00000008;
const PCRE2_MULTILINE: u32 = 0x00000400;

pub struct SearchBar {
    pub container: gtk4::Box,
    pub entry: gtk4::Entry,
    match_label: gtk4::Label,
}

impl SearchBar {
    pub fn new(terminal: &vte4::Terminal, theme: &copad_core::theme::Theme) -> Self {
        let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        container.set_halign(gtk4::Align::Fill);
        container.set_valign(gtk4::Align::End);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_bottom(8);
        container.add_css_class("copad-search-bar");
        container.set_visible(false);

        let entry = gtk4::Entry::new();
        entry.set_hexpand(true);
        entry.set_placeholder_text(Some("Search..."));
        entry.add_css_class("copad-search-entry");

        let match_label = gtk4::Label::new(None);
        match_label.add_css_class("copad-search-count");

        let prev_btn = gtk4::Button::from_icon_name("go-up-symbolic");
        prev_btn.add_css_class("flat");
        prev_btn.add_css_class("copad-search-btn");
        prev_btn.set_tooltip_text(Some("Previous (Shift+Enter)"));

        let next_btn = gtk4::Button::from_icon_name("go-down-symbolic");
        next_btn.add_css_class("flat");
        next_btn.add_css_class("copad-search-btn");
        next_btn.set_tooltip_text(Some("Next (Enter)"));

        let case_btn = gtk4::ToggleButton::new();
        case_btn.set_icon_name("format-text-italic-symbolic");
        case_btn.add_css_class("flat");
        case_btn.add_css_class("copad-search-btn");
        case_btn.set_tooltip_text(Some("Case sensitive"));

        let close_btn = gtk4::Button::from_icon_name("window-close-symbolic");
        close_btn.add_css_class("flat");
        close_btn.add_css_class("copad-search-btn");
        close_btn.set_tooltip_text(Some("Close (Escape)"));

        container.append(&entry);
        container.append(&match_label);
        container.append(&prev_btn);
        container.append(&next_btn);
        container.append(&case_btn);
        container.append(&close_btn);

        // Apply search on text change
        let term = terminal.clone();
        let case = case_btn.clone();
        let label = match_label.clone();
        entry.connect_changed(move |e| {
            apply_search(&term, &e.text(), case.is_active(), &label);
        });

        // Toggle case sensitivity
        let term = terminal.clone();
        let entry_ref = entry.clone();
        let label = match_label.clone();
        case_btn.connect_toggled(move |btn| {
            apply_search(&term, &entry_ref.text(), btn.is_active(), &label);
        });

        // Enter = next
        let term = terminal.clone();
        entry.connect_activate(move |_| {
            term.search_find_next();
        });

        let key_controller = gtk4::EventControllerKey::new();
        let term_for_key = terminal.clone();
        let container_for_key = container.clone();
        let terminal_for_focus = terminal.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, modifier| match keyval {
            gdk::Key::Escape => {
                close_search(&container_for_key, &term_for_key, &terminal_for_focus);
                glib::Propagation::Stop
            }
            gdk::Key::Return if modifier.contains(gdk::ModifierType::SHIFT_MASK) => {
                term_for_key.search_find_previous();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        entry.add_controller(key_controller);

        // Button clicks
        let term = terminal.clone();
        next_btn.connect_clicked(move |_| {
            term.search_find_next();
        });

        let term = terminal.clone();
        prev_btn.connect_clicked(move |_| {
            term.search_find_previous();
        });

        let term = terminal.clone();
        let container_ref = container.clone();
        let terminal_for_focus = terminal.clone();
        close_btn.connect_clicked(move |_| {
            close_search(&container_ref, &term, &terminal_for_focus);
        });

        // Load CSS
        let css = gtk4::CssProvider::new();
        css.load_from_string(&build_search_css(theme));
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 4,
        );

        Self {
            container,
            entry,
            match_label,
        }
    }

    pub fn toggle(&self, terminal: &vte4::Terminal) {
        if self.container.is_visible() {
            self.container.set_visible(false);
            self.match_label.set_text("");
            terminal.search_set_regex(None::<&vte4::Regex>, 0);
            terminal.grab_focus();
        } else {
            self.container.set_visible(true);
            self.entry.grab_focus();
            // Select all after focus settles
            let entry = self.entry.clone();
            glib::idle_add_local_once(move || {
                entry.select_region(0, -1);
            });
            // Re-apply current search text if any
            if !self.entry.text().is_empty() {
                apply_search(terminal, &self.entry.text(), false, &self.match_label);
            }
        }
    }
}

fn apply_search(terminal: &vte4::Terminal, text: &str, case_sensitive: bool, label: &gtk4::Label) {
    if text.is_empty() {
        terminal.search_set_regex(None::<&vte4::Regex>, 0);
        label.set_text("");
        return;
    }

    let escaped = glib::Regex::escape_string(text);
    let mut flags = PCRE2_MULTILINE;
    if !case_sensitive {
        flags |= PCRE2_CASELESS;
    }

    match vte4::Regex::for_search(&escaped, flags) {
        Ok(regex) => {
            terminal.search_set_regex(Some(&regex), 0);
            terminal.search_set_wrap_around(true);
            let found = terminal.search_find_next();
            label.set_text(if found { "" } else { "No matches" });
        }
        Err(e) => {
            eprintln!("[copad] search regex error: {e}");
            label.set_text("Invalid");
        }
    }
}

fn close_search(container: &gtk4::Box, terminal: &vte4::Terminal, focus_target: &vte4::Terminal) {
    container.set_visible(false);
    terminal.search_set_regex(None::<&vte4::Regex>, 0);
    focus_target.grab_focus();
}

pub fn build_search_css(theme: &copad_core::theme::Theme) -> String {
    format!(
        r#"
.copad-search-bar {{
    background-color: {surface2};
    border-radius: 8px;
    padding: 4px 8px;
    margin: 8px;
}}

.copad-search-entry {{
    background-color: {bg};
    color: {text};
    border: 1px solid {overlay0};
    border-radius: 4px;
    padding: 4px 8px;
    min-height: 24px;
}}

.copad-search-entry:focus {{
    border-color: {accent};
}}

.copad-search-count {{
    color: {subtext0};
    font-size: 12px;
    margin: 0 4px;
}}

.copad-search-btn {{
    min-width: 24px;
    min-height: 24px;
    padding: 2px;
    border-radius: 4px;
    color: {text};
}}

.copad-search-btn:hover {{
    background-color: {overlay0};
}}
"#,
        surface2 = theme.surface2,
        bg = theme.background,
        text = theme.text,
        overlay0 = theme.overlay0,
        accent = theme.accent,
        subtext0 = theme.subtext0,
    )
}
