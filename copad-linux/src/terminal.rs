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

/// POSIX single-quote escape: every `'` becomes `'\''` and the whole string is
/// wrapped in `'...'`. Safe for any shell-interpreted character (spaces, `$`,
/// backticks, `;`, glob chars, newlines). Empty string round-trips as `''`.
/// Mirrors the Swift port's `shellQuote`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Turn a dropped `GdkFileList` into one shell-quoted, space-joined payload.
///
/// Two cases the naive version gets wrong:
/// - **Non-local URIs.** GTK deserializes `text/uri-list` into a `FileList`, so a
///   dragged *web* URL can arrive here rather than as a string, and `File::path()`
///   is `None` for it. Falling back to the URI keeps the drop useful instead of
///   silently doing nothing.
/// - **Non-UTF-8 paths.** Linux filenames are arbitrary bytes but `paste_text`
///   takes `&str`. `to_string_lossy` would paste a corrupted path that doesn't
///   exist, so such entries are skipped loudly rather than mangled.
///
/// Returns `None` when nothing usable survived (caller rejects the drop).
fn quote_dropped_files(files: &[gtk4::gio::File]) -> Option<String> {
    let mut out = Vec::new();
    for file in files {
        match file.path() {
            Some(path) => match path.to_str() {
                Some(s) => out.push(shell_quote(s)),
                None => log::warn!(
                    "[copad] drop: skipping non-UTF-8 path {:?} (cannot be pasted as text)",
                    path
                ),
            },
            // Non-local (e.g. a web URL, or a remote gvfs mount). Quoted like the
            // paths beside it — this lands in a shell command line, where an
            // unquoted `?`/`&` in a URL would break the command.
            None => out.push(shell_quote(&file.uri())),
        }
    }
    (!out.is_empty()).then(|| out.join(" "))
}

/// Save dropped image bytes as PNG under `<cache>/drops/` and return the path.
///
/// `create_new` + a counter rather than trusting `<millis>-<n>`: two copad
/// processes start their counters at the same value, so a bare timestamp+counter
/// name can collide and truncate the other process's file.
fn save_dropped_image(texture: &gdk::Texture) -> Option<std::path::PathBuf> {
    let dir = copad_core::paths::cache_dir().join("drops");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("[copad] drop: cannot create {}: {e}", dir.display());
        return None;
    }
    let bytes = texture.save_to_png_bytes();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    for n in 0..100 {
        let path = dir.join(format!("{stamp}-{}-{n}.png", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                return match f.write_all(&bytes) {
                    Ok(()) => Some(path),
                    Err(e) => {
                        log::warn!("[copad] drop: write failed {}: {e}", path.display());
                        None
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                log::warn!("[copad] drop: create failed {}: {e}", path.display());
                return None;
            }
        }
    }
    None
}

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

        // Drag-and-drop: files / images / URLs, mirroring the macOS terminal.
        // Routed through `paste_text` (not `feed_child`) so bracketed-paste mode
        // is honored exactly like a clipboard paste — dropping several paths into
        // an editor must not be interpreted as keystrokes.
        //
        // GTK negotiates ONE type and hands back one Value, so unlike macOS
        // (which probes the pasteboard in priority order) precedence is the
        // `set_types` order plus the match below. That is also why
        // `quote_dropped_files` handles non-local URIs itself: a dragged web URL
        // deserializes to a FileList, and there is no falling back to the String
        // branch once GTK has chosen.
        {
            let drop_target = gtk4::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY);
            drop_target.set_types(&[
                gdk::FileList::static_type(),
                gdk::Texture::static_type(),
                String::static_type(),
            ]);
            let term_for_drop = terminal.clone();
            drop_target.connect_drop(move |_, value, _, _| {
                if let Ok(files) = value.get::<gdk::FileList>() {
                    match quote_dropped_files(&files.files()) {
                        Some(text) => {
                            term_for_drop.paste_text(&text);
                            return true;
                        }
                        // Everything was unusable (e.g. only non-UTF-8 paths) —
                        // reject so the user sees the drop bounce back.
                        None => return false,
                    }
                }
                if let Ok(texture) = value.get::<gdk::Texture>() {
                    return match save_dropped_image(&texture) {
                        Some(path) => match path.to_str() {
                            Some(s) => {
                                term_for_drop.paste_text(&shell_quote(s));
                                true
                            }
                            None => false,
                        },
                        None => false,
                    };
                }
                if let Ok(text) = value.get::<String>() {
                    // Bare, matching macOS priority 3: a dropped URL is text for
                    // the shell/CLI to do something with.
                    term_for_drop.paste_text(&text);
                    return true;
                }
                false
            });
            terminal.add_controller(drop_target);
        }

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
mod shell_quote_tests {
    use super::shell_quote;

    #[test]
    fn plain_path_is_quoted() {
        assert_eq!(shell_quote("/home/u/a.txt"), "'/home/u/a.txt'");
    }

    #[test]
    fn empty_round_trips_as_empty_quotes() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn spaces_survive_as_one_word() {
        assert_eq!(shell_quote("/tmp/my file.txt"), "'/tmp/my file.txt'");
    }

    #[test]
    fn single_quote_is_escaped() {
        // POSIX has no escape inside '...', so it must be closed, escaped, reopened.
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn shell_metacharacters_are_inert() {
        // The whole point: a hostile filename must stay one literal argument.
        assert_eq!(shell_quote("a; rm -rf /"), "'a; rm -rf /'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
        assert_eq!(shell_quote("`id`"), "'`id`'");
        assert_eq!(shell_quote("a && b"), "'a && b'");
        assert_eq!(shell_quote("*.rs"), "'*.rs'");
    }

    #[test]
    fn quote_breakout_attempt_stays_quoted() {
        // The classic escape: a filename that tries to close our quote and append
        // a command. Every `'` is neutralized, so the payload stays one word.
        let hostile = "'; rm -rf / #";
        let quoted = shell_quote(hostile);
        assert_eq!(quoted, r"''\''; rm -rf / #'");
        // No bare (unescaped) closing quote can appear before the final char.
        assert!(quoted.ends_with('\''));
        assert!(quoted.starts_with('\''));
    }

    #[test]
    fn newline_is_preserved_inside_quotes() {
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn unicode_passes_through() {
        assert_eq!(shell_quote("/tmp/사진.png"), "'/tmp/사진.png'");
    }
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
