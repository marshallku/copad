use std::cell::Cell;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use vte4::Terminal;
use vte4::prelude::*;

use copad_core::config::CopadConfig;
use copad_core::theme::Theme;

use crate::panel::Panel;
use crate::search::SearchBar;

/// VTE reports cwd as `file://<hostname>/abs/path`. Naive
/// `strip_prefix("file://")` would leave the hostname mixed in. Shared
/// by `terminal.cwd_changed` emission and `terminal.state` for shape parity.
pub(crate) fn normalize_osc7_uri(uri: &str) -> String {
    let path = if let Some(rest) = uri.strip_prefix("file://") {
        if let Some(idx) = rest.find('/') {
            &rest[idx..]
        } else {
            // Bare host with no path — fall back to whatever's left so the
            // value is at least non-empty.
            rest
        }
    } else {
        uri
    };
    // OSC 7 paths are URI-encoded ("My%20Project"). Decode so the
    // restored cwd is a real filesystem path. Falls back to the raw
    // string on decode failure rather than dropping the value.
    glib::Uri::unescape_string(path, None)
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.to_string())
}

const DEFAULT_FONT_SCALE: f64 = 1.0;
const FONT_SCALE_STEP: f64 = 0.1;
const MIN_FONT_SCALE: f64 = 0.3;
const MAX_FONT_SCALE: f64 = 3.0;

pub struct TerminalPanel {
    pub id: String,
    pub overlay: gtk4::Overlay,
    pub terminal: Terminal,
    pub child_pid: Rc<Cell<i32>>,
    pub search_bar: SearchBar,
    /// Last known cwd of the spawned shell. Seeded by the constructor's
    /// `cwd` arg, then refreshed by tabs.rs's OSC 7 handler. Read at
    /// window-close time by session persistence.
    pub last_cwd: Rc<std::cell::RefCell<Option<String>>>,
}

impl TerminalPanel {
    /// `cwd = None` inherits the copad process cwd. `initial_input` is
    /// fed to the PTY only after `spawn_async`'s success callback fires
    /// (writing pre-attach would race against child wiring); on spawn
    /// failure it's dropped (no child = nowhere to deliver).
    pub fn new_with_cwd_and_initial_input(
        config: &CopadConfig,
        cwd: Option<&std::path::Path>,
        initial_input: Option<String>,
        on_exit: impl Fn() + 'static,
    ) -> Self {
        let terminal = Terminal::new();

        // Font
        let font_desc = gtk4::pango::FontDescription::from_string(&format!(
            "{} {}",
            config.terminal.font_family, config.terminal.font_size
        ));
        terminal.set_font(Some(&font_desc));
        terminal.set_font_scale(DEFAULT_FONT_SCALE);

        // Colors from theme. Background is forced transparent (and
        // `set_clear_background(false)` skips the GL clear) so the
        // window-level `BackgroundLayer` shows through every terminal,
        // image or no image. The window's own CSS supplies the solid
        // theme color when no background image is set.
        let theme = Theme::by_name(&config.theme.name).unwrap_or_default();
        let fg = parse_color(&theme.foreground);
        let bg = gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
        let palette: Vec<gdk::RGBA> = theme.palette.iter().map(|c| parse_color(c)).collect();
        let palette_refs: Vec<&gdk::RGBA> = palette.iter().collect();
        terminal.set_colors(Some(&fg), Some(&bg), &palette_refs);
        terminal.set_clear_background(false);

        terminal.set_cursor_blink_mode(vte4::CursorBlinkMode::On);
        terminal.set_cursor_shape(vte4::CursorShape::Block);
        terminal.set_scrollback_lines(10000);
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);

        // Zoom shortcuts
        let zoom_controller = gtk4::EventControllerKey::new();
        let term_clone = terminal.clone();
        zoom_controller.connect_key_pressed(move |_, keyval, _, modifier| {
            if !modifier.contains(gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            match keyval {
                gdk::Key::equal | gdk::Key::plus => {
                    let scale = (term_clone.font_scale() + FONT_SCALE_STEP).min(MAX_FONT_SCALE);
                    term_clone.set_font_scale(scale);
                    glib::Propagation::Stop
                }
                gdk::Key::minus => {
                    let scale = (term_clone.font_scale() - FONT_SCALE_STEP).max(MIN_FONT_SCALE);
                    term_clone.set_font_scale(scale);
                    glib::Propagation::Stop
                }
                gdk::Key::_0 => {
                    term_clone.set_font_scale(DEFAULT_FONT_SCALE);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        terminal.add_controller(zoom_controller);

        // Spawn shell. Panel id is allocated here (not in the
        // `Self { id, ... }` initializer below) so it can be
        // injected into the child env BEFORE spawn — the shell's
        // precmd hook reads `$COPAD_PANEL_ID` to attribute
        // `pane.context_changed` events back to this pane. macOS
        // already follows this pattern in TerminalViewController.swift.
        let id = uuid::Uuid::new_v4().to_string();
        let shell = config.terminal.shell.clone();
        let socket_env = format!(
            "COPAD_SOCKET={}",
            copad_core::paths::gui_socket_path(std::process::id()).display()
        );
        let panel_id_env = format!("COPAD_PANEL_ID={id}");
        let child_pid: Rc<Cell<i32>> = Rc::new(Cell::new(-1));
        let pid_cell = child_pid.clone();
        // Resolve cwd once upfront. We pass `Option<&str>` to
        // VTE's spawn_async, which interprets it as the working
        // directory (None = inherit from copad). On Linux paths
        // are arbitrary bytes; `to_string_lossy` substitutes
        // U+FFFD for non-UTF-8 components rather than failing.
        // In practice every cwd we receive flows through
        // `std::fs::canonicalize` upstream, which itself
        // operates on `OsStr` and produces canonical paths the
        // user already typed somewhere, so non-UTF-8 cwds are a
        // theoretical concern only.
        let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());
        let cwd_arg: Option<&str> = cwd_str.as_deref();
        // Clone the VTE Terminal handle (it's a refcounted
        // GObject) into the spawn callback so we can reach the
        // child after spawn completes. Then feed the initial
        // input from THERE — not from the caller — to remove
        // the race where the caller writes before the child
        // is actually attached to the PTY.
        let terminal_for_init = terminal.clone();
        terminal.spawn_async(
            vte4::PtyFlags::DEFAULT,
            cwd_arg,
            &[&shell],
            &[&socket_env, &panel_id_env],
            gtk4::glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            gtk4::gio::Cancellable::NONE,
            move |result| match &result {
                Ok(pid) => {
                    eprintln!("[copad] shell spawned, child_pid={}", pid.0);
                    pid_cell.set(pid.0);
                    if let Some(text) = &initial_input {
                        // feed_child writes directly to the PTY
                        // master — at this point the slave is
                        // attached to the just-spawned shell, so
                        // the bytes land in the shell's stdin
                        // queue without ambiguity.
                        terminal_for_init.feed_child(text.as_bytes());
                    }
                }
                Err(e) => {
                    eprintln!("[copad] shell spawn error: {e}");
                }
            },
        );

        terminal.connect_child_exited(move |_terminal, _status| {
            on_exit();
        });

        // VTE transparent CSS — required so the GTK widget composites
        // its content against the window-level `BackgroundLayer`.
        let css_provider = gtk4::CssProvider::new();
        css_provider.load_from_string("vte-terminal { background-color: transparent; }");
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &css_provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );

        // Search bar
        let search_bar = SearchBar::new(&terminal, &theme);

        // Overlay only exists to host the search bar above the terminal.
        // The background image moved to `BackgroundLayer` at the window
        // level so every panel (terminals, plugins, webviews) sits over
        // the same image instead of each terminal owning its own copy.
        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&terminal));
        overlay.add_overlay(&search_bar.container);
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);

        let last_cwd = Rc::new(std::cell::RefCell::new(cwd_str.clone()));

        Self {
            id,
            overlay,
            terminal,
            child_pid,
            search_bar,
            last_cwd,
        }
    }

    /// Read visible terminal screen text
    pub fn read_screen(&self) -> String {
        self.terminal
            .text_format(vte4::Format::Text)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    /// Read a specific range of terminal text (row/col are 0-based)
    pub fn read_range(&self, start_row: i64, start_col: i64, end_row: i64, end_col: i64) -> String {
        let (text, _len) = self.terminal.text_range_format(
            vte4::Format::Text,
            start_row as std::ffi::c_long,
            start_col as std::ffi::c_long,
            end_row as std::ffi::c_long,
            end_col as std::ffi::c_long,
        );
        text.map(|s: gtk4::glib::GString| s.to_string())
            .unwrap_or_default()
    }

    /// Best-effort current cwd: OSC 7 (`current_directory_uri`) first,
    /// then `/proc/<pid>/cwd`. Used by both `state()` (socket query)
    /// and session persistence (window-close snapshot). Shells that
    /// don't emit OSC 7 still produce a usable cwd through the proc
    /// fallback as long as the child is alive.
    pub fn current_cwd(&self) -> Option<String> {
        if let Some(u) = self.terminal.current_directory_uri() {
            return Some(normalize_osc7_uri(u.as_str()));
        }
        let pid = self.child_pid.get();
        if pid > 0
            && let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd"))
        {
            return Some(p.to_string_lossy().to_string());
        }
        // Child PID gone (shell exited) — last_cwd preserves the OSC 7
        // value or the spawn-time cwd.
        self.last_cwd.borrow().clone()
    }

    /// Get terminal state: cursor, dimensions, CWD, title
    pub fn state(&self) -> serde_json::Value {
        let (cursor_col, cursor_row) = self.terminal.cursor_position();
        let cwd = self.current_cwd();
        serde_json::json!({
            "cols": self.terminal.column_count(),
            "rows": self.terminal.row_count(),
            "cursor": [cursor_row, cursor_col],
            "cwd": cwd,
            "title": self.terminal.window_title().map(|t| t.to_string()),
        })
    }

    /// Send text to the terminal PTY (execute a command)
    pub fn feed_input(&self, text: &str) {
        self.terminal.feed_child(text.as_bytes());
    }

    pub fn apply_config(&self, config: &CopadConfig) {
        let font_desc = gtk4::pango::FontDescription::from_string(&format!(
            "{} {}",
            config.terminal.font_family, config.terminal.font_size
        ));
        self.terminal.set_font(Some(&font_desc));
    }
}

impl Panel for TerminalPanel {
    fn widget(&self) -> &gtk4::Widget {
        self.overlay.upcast_ref()
    }

    fn title(&self) -> String {
        self.terminal
            .window_title()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "Terminal".to_string())
    }

    fn panel_type(&self) -> &str {
        "terminal"
    }

    fn grab_focus(&self) {
        self.terminal.grab_focus();
    }

    fn id(&self) -> &str {
        &self.id
    }
}

pub fn parse_color(hex: &str) -> gdk::RGBA {
    let hex = hex.trim_start_matches('#');
    if hex.len() < 6 {
        return gdk::RGBA::new(0.0, 0.0, 0.0, 1.0);
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0) as f32 / 255.0;
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0) as f32 / 255.0;
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0) as f32 / 255.0;
    gdk::RGBA::new(r, g, b, 1.0)
}

/// Clamp an opacity into [0, 1]. Non-finite input (NaN / ±inf, reachable
/// from a malformed TOML float) collapses to fully opaque rather than
/// poisoning a CSS string or widget alpha — `f64::clamp` would otherwise
/// propagate NaN through.
pub fn norm_opacity(o: f64) -> f64 {
    if o.is_finite() {
        o.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// CSS `rgba(...)` literal for a `#rrggbb` color at the given alpha.
pub fn rgba_css(hex: &str, alpha: f64) -> String {
    let c = parse_color(hex);
    format!(
        "rgba({},{},{},{})",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
        norm_opacity(alpha),
    )
}

#[cfg(test)]
mod osc7_tests {
    use super::normalize_osc7_uri;

    #[test]
    fn strips_hostname_correctly() {
        assert_eq!(normalize_osc7_uri("file://arch/tmp"), "/tmp");
        assert_eq!(
            normalize_osc7_uri("file://example.com/home/user"),
            "/home/user"
        );
    }

    #[test]
    fn preserves_when_already_no_host() {
        assert_eq!(normalize_osc7_uri("file:///abs/path"), "/abs/path");
    }

    #[test]
    fn passes_through_non_file_uris() {
        assert_eq!(normalize_osc7_uri("/already/clean"), "/already/clean");
        assert_eq!(normalize_osc7_uri(""), "");
    }

    #[test]
    fn malformed_no_slash_after_host_is_preserved() {
        // Edge: bare host, no path. Don't try to invent a value, just don't
        // crash — return whatever's left so the caller sees a non-empty hint.
        assert_eq!(normalize_osc7_uri("file://lonely-host"), "lonely-host");
    }

    #[test]
    fn percent_decodes_path_segments() {
        // VTE emits the path as URI-encoded. Without decoding, the
        // restored cwd would not exist on disk for any path containing
        // a space, accented character, etc.
        assert_eq!(
            normalize_osc7_uri("file://arch/home/me/My%20Project"),
            "/home/me/My Project"
        );
        assert_eq!(normalize_osc7_uri("file:///tmp/a%2Bb"), "/tmp/a+b");
    }
}

#[cfg(test)]
mod color_tests {
    use super::{norm_opacity, rgba_css};

    #[test]
    fn norm_opacity_clamps_and_guards_non_finite() {
        assert_eq!(norm_opacity(0.85), 0.85);
        assert_eq!(norm_opacity(-0.5), 0.0);
        assert_eq!(norm_opacity(1.5), 1.0);
        assert_eq!(norm_opacity(f64::NAN), 1.0);
        assert_eq!(norm_opacity(f64::INFINITY), 1.0);
        assert_eq!(norm_opacity(f64::NEG_INFINITY), 1.0);
    }

    #[test]
    fn rgba_css_renders_channels_and_alpha() {
        assert_eq!(rgba_css("#1e1e2e", 0.85), "rgba(30,30,46,0.85)");
        assert_eq!(rgba_css("1e1e2e", 1.0), "rgba(30,30,46,1)");
        assert_eq!(rgba_css("#000000", 0.0), "rgba(0,0,0,0)");
        // Out-of-range alpha is normalized before formatting.
        assert_eq!(rgba_css("#ffffff", 2.0), "rgba(255,255,255,1)");
    }
}
