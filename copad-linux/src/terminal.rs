use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use vte4::Terminal;
use vte4::prelude::*;

use copad_core::config::CopadConfig;
use copad_core::theme::Theme;

use crate::image_shadow::ImageShared;
use crate::kitty_relay::{self, Relay, RelayOptions};
use crate::panel::Panel;
use crate::search::SearchBar;

/// POSIX single-quote escape: every `'` becomes `'\''` and the string is wrapped in `'...'`.
/// Mirrors the Swift port's `shellQuote`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Turn a dropped `GdkFileList` into one shell-quoted, space-joined payload.
///
/// - Non-local URIs: a dragged web URL can arrive as a `FileList` with `File::path()` `None`;
///   falling back to the URI keeps the drop useful.
/// - Non-UTF-8 paths: `paste_text` takes `&str` and `to_string_lossy` would paste a path that
///   does not exist, so such entries are skipped loudly.
///
/// Returns `None` when nothing usable survived (the caller rejects the drop).
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
            // Non-local (a web URL, a remote gvfs mount): quoted like the paths beside it, since
            // an unquoted `?`/`&` would break the shell command line.
            None => out.push(shell_quote(&file.uri())),
        }
    }
    (!out.is_empty()).then(|| out.join(" "))
}

/// Save dropped image bytes as PNG under `<cache>/drops/` and return the path. Uses `create_new`
/// plus a counter: two copad processes start their counters at the same value, so a bare
/// timestamp+counter name can collide and truncate the other process's file.
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

/// VTE reports cwd as `file://<hostname>/abs/path`; a naive `strip_prefix("file://")` would keep
/// the hostname. Shared by `terminal.cwd_changed` and `terminal.state`.
pub(crate) fn normalize_osc7_uri(uri: &str) -> String {
    let path = if let Some(rest) = uri.strip_prefix("file://") {
        if let Some(idx) = rest.find('/') {
            &rest[idx..]
        } else {
            rest
        }
    } else {
        uri
    };
    // OSC 7 paths are URI-encoded ("My%20Project"); decode so the restored cwd is a real path,
    // falling back to the raw string on failure.
    glib::Uri::unescape_string(path, None)
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.to_string())
}

/// `[terminal] images`: VTE ships sixel support but leaves it off, and only advertises sixel in
/// DA1 while it is on, which is how yazi decides to send images at all. A libvte built without
/// sixel ignores the switch, so say once why images stay off.
fn apply_images(terminal: &Terminal, enabled: bool) {
    terminal.set_enable_sixel(enabled);
    if enabled && !terminal.enables_sixel() {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            log::warn!("[terminal] images: this VTE was built without sixel; images stay off");
        });
    }
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
    /// Last known cwd of the shell: seeded by the constructor, refreshed by tabs.rs's OSC 7
    /// handler, read by session persistence at window close.
    pub last_cwd: Rc<std::cell::RefCell<Option<String>>>,
    relay: Rc<RefCell<Option<Relay>>>,
    exit: Rc<ExitLatch>,
    images: Option<(Arc<ImageShared>, crate::image_layers::ImageLayers)>,
}

/// A relayed panel's exit: VTE has consumed every byte (its `eof`) AND the shell's status is in.
/// They arrive in either order, and `child-exited` is emitted once both are.
#[derive(Default)]
struct ExitLatch {
    eof: Cell<bool>,
    status: Cell<Option<i32>>,
    done: Cell<bool>,
}

impl ExitLatch {
    fn try_finish(&self, terminal: &Terminal) {
        if self.done.get() || !self.eof.get() {
            return;
        }
        let Some(status) = self.status.get() else {
            return;
        };
        self.done.set(true);
        terminal.emit_by_name::<()>("child-exited", &[&status]);
    }
}

impl Drop for TerminalPanel {
    fn drop(&mut self) {
        // No exit to report; stopping the relay closes the shell's PTY, which hangs it up.
        self.exit.done.set(true);
        self.relay.borrow_mut().take();
    }
}

/// Put a [`Relay`] in front of `terminal`: VTE reads a raw PTY the relay feeds, and the
/// returned PTY is the shell's, to spawn on. Nothing is spawned yet, so a failure leaves the
/// terminal free for the plain path.
fn prepare_relay(
    terminal: &Terminal,
    slot: &Rc<RefCell<Option<Relay>>>,
    options: RelayOptions,
) -> std::io::Result<vte4::Pty> {
    let other = |e: glib::Error| std::io::Error::other(e.to_string());
    let (r_master, r_slave) = kitty_relay::open_raw_pty()?;
    let r_pty = vte4::Pty::foreign_sync(r_master, gtk4::gio::Cancellable::NONE).map_err(other)?;
    let p_pty = vte4::Pty::new_sync(vte4::PtyFlags::DEFAULT, gtk4::gio::Cancellable::NONE)
        .map_err(other)?;
    let images = options.images.clone();
    let relay = Relay::start(p_pty.fd(), r_slave, options)?;
    sync_relay_size(terminal, &relay, images.as_deref());
    terminal.set_pty(Some(&r_pty));
    *slot.borrow_mut() = Some(relay);
    Ok(p_pty)
}

fn sync_relay_size(terminal: &Terminal, relay: &Relay, images: Option<&ImageShared>) {
    let scale = terminal.scale_factor().max(1) as i64;
    let px = |v: i64| (v * scale).clamp(0, u16::MAX as i64) as u16;
    let (cw, ch) = (px(terminal.char_width()), px(terminal.char_height()));
    if let Some(images) = images {
        images
            .graphics
            .cell_width
            .store(cw as u32, std::sync::atomic::Ordering::Relaxed);
        images
            .graphics
            .cell_height
            .store(ch as u32, std::sync::atomic::Ordering::Relaxed);
    }
    relay.set_size(
        terminal.row_count().clamp(1, u16::MAX as i64) as u16,
        terminal.column_count().clamp(1, u16::MAX as i64) as u16,
        cw,
        ch,
    );
}

impl TerminalPanel {
    /// `cwd = None` inherits the copad process cwd. `initial_input` is fed to the PTY only after
    /// `spawn_async` succeeds (writing earlier races the child wiring) and is dropped on failure.
    pub fn new_with_cwd_and_initial_input(
        config: &CopadConfig,
        cwd: Option<&std::path::Path>,
        initial_input: Option<String>,
        on_exit: impl Fn() + 'static,
    ) -> Self {
        let terminal = Terminal::new();

        let font_desc = gtk4::pango::FontDescription::from_string(&format!(
            "{} {}",
            config.terminal.font_family, config.terminal.font_size
        ));
        terminal.set_font(Some(&font_desc));
        terminal.set_font_scale(DEFAULT_FONT_SCALE);

        // Background is forced transparent (and `set_clear_background(false)` skips the GL clear)
        // so the window-level `BackgroundLayer` shows through; the window CSS supplies the solid
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
        terminal.set_scrollback_lines(config.terminal.scrollback as i64);
        apply_images(&terminal, config.terminal.images);
        terminal.set_hexpand(true);
        terminal.set_vexpand(true);

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

        // Drag-and-drop: files / images / URLs. Routed through `paste_text` (not `feed_child`) so
        // bracketed-paste mode is honored; dropped paths must not be interpreted as keystrokes.
        //
        // GTK negotiates ONE type and hands back one Value, so precedence is the `set_types`
        // order plus the match below, and `quote_dropped_files` handles non-local URIs itself: a
        // dragged web URL deserializes to a FileList, with no falling back to the String branch.
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
                        // Everything unusable (e.g. only non-UTF-8 paths): reject so the drop
                        // bounces back.
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
                    term_for_drop.paste_text(&text);
                    return true;
                }
                false
            });
            terminal.add_controller(drop_target);
        }

        // Panel id is allocated before spawn so it can go into the child env: the shell's precmd
        // hook reads `$COPAD_PANEL_ID` to attribute `pane.context_changed` events to this pane.
        let id = uuid::Uuid::new_v4().to_string();
        let shell = config.terminal.shell.clone();
        let socket_env = format!(
            "COPAD_SOCKET={}",
            copad_core::paths::gui_socket_path(std::process::id()).display()
        );
        let panel_id_env = format!("COPAD_PANEL_ID={id}");
        let child_pid: Rc<Cell<i32>> = Rc::new(Cell::new(-1));
        let pid_cell = child_pid.clone();
        // `to_string_lossy` substitutes U+FFFD for non-UTF-8 components rather than failing; cwds
        // arrive canonicalized from upstream, so that is theoretical.
        let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());
        let cwd_arg: Option<&str> = cwd_str.as_deref();
        // Feed the initial input from the spawn callback, not the caller: a caller write before
        // the child is attached to the PTY would race.
        let terminal_for_init = terminal.clone();
        let on_spawned = move |result: Result<glib::Pid, glib::Error>| match &result {
            Ok(pid) => {
                eprintln!("[copad] shell spawned, child_pid={}", pid.0);
                pid_cell.set(pid.0);
                if let Some(text) = &initial_input {
                    terminal_for_init.feed_child(text.as_bytes());
                }
            }
            Err(e) => {
                eprintln!("[copad] shell spawn error: {e}");
            }
        };
        let relay: Rc<RefCell<Option<Relay>>> = Rc::new(RefCell::new(None));
        let exit = Rc::new(ExitLatch::default());
        let argv = [shell.as_str()];
        let envv = [socket_env.as_str(), panel_id_env.as_str()];
        // Behind a relay the panel speaks the kitty keyboard protocol and draws images; if the
        // relay cannot be set up, the shell starts the way it always did.
        let kitty = config.terminal.kitty_keyboard;
        let image_shared = config.terminal.images.then(|| {
            let shared = Arc::new(ImageShared::default());
            shared
                .graphics
                .enabled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            shared
        });
        let p_pty = if kitty || image_shared.is_some() {
            let options = RelayOptions {
                kitty,
                images: image_shared.clone(),
            };
            prepare_relay(&terminal, &relay, options)
                .inspect_err(|e| log::warn!("[copad] terminal relay unavailable ({e}); plain PTY"))
                .ok()
        } else {
            None
        };
        let image_shared = image_shared.filter(|_| p_pty.is_some());
        match p_pty {
            Some(p_pty) => {
                if kitty {
                    crate::kitty_keys::attach(&terminal, relay.clone());
                }
                let exit_e = exit.clone();
                terminal.connect_eof(move |t| {
                    exit_e.eof.set(true);
                    exit_e.try_finish(t);
                });
                let (relay_w, exit_w, term_w) =
                    (Rc::downgrade(&relay), exit.clone(), terminal.downgrade());
                let keep = p_pty.clone();
                p_pty.spawn_async(
                    cwd_arg,
                    &argv,
                    &envv,
                    gtk4::glib::SpawnFlags::DEFAULT,
                    || {},
                    -1,
                    gtk4::gio::Cancellable::NONE,
                    move |result| {
                        // The shell's PTY object lives until the spawn finished; the relay holds
                        // its own copies of the descriptor.
                        drop(keep);
                        if let Ok(pid) = &result {
                            glib::child_watch_add_local(*pid, move |_, status| {
                                if let Some(r) = relay_w.upgrade()
                                    && let Some(r) = r.borrow().as_ref()
                                {
                                    r.child_exited();
                                }
                                exit_w.status.set(Some(status));
                                if let Some(t) = term_w.upgrade() {
                                    exit_w.try_finish(&t);
                                }
                            });
                        }
                        on_spawned(result);
                    },
                );
            }
            None => terminal.spawn_async(
                vte4::PtyFlags::DEFAULT,
                cwd_arg,
                &argv,
                &envv,
                gtk4::glib::SpawnFlags::DEFAULT,
                || {},
                -1,
                gtk4::gio::Cancellable::NONE,
                on_spawned,
            ),
        }

        terminal.connect_child_exited(move |_terminal, _status| {
            on_exit();
        });

        // VTE transparent CSS: the widget must composite against the window-level
        // `BackgroundLayer`.
        let css_provider = gtk4::CssProvider::new();
        css_provider.load_from_string("vte-terminal { background-color: transparent; }");
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &css_provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );

        let search_bar = SearchBar::new(&terminal, &theme);

        // The overlay only hosts the search bar; the background image is on `BackgroundLayer` at
        // the window level.
        let overlay = gtk4::Overlay::new();
        let images = image_shared.map(|shared| {
            let layers =
                crate::image_layers::attach(&terminal, shared.clone(), Rc::downgrade(&relay));
            let inner = gtk4::Overlay::new();
            inner.set_child(Some(&layers.below));
            inner.add_overlay(&terminal);
            inner.set_measure_overlay(&terminal, true);
            overlay.set_child(Some(&inner));
            overlay.add_overlay(&layers.above);
            (shared, layers)
        });
        if images.is_none() {
            overlay.set_child(Some(&terminal));
        }
        if relay.borrow().is_some() {
            // VTE sizes its own PTY (R) when allocated; the shell's (P) follows. An invisible
            // overlay child is allocated right after the terminal in the same pass, so its
            // `resize` sees the new grid. A font change moves the grid without resizing anything,
            // hence the second hook.
            let sync = {
                let (term, relay) = (terminal.downgrade(), Rc::downgrade(&relay));
                let shared = images.as_ref().map(|(s, _)| s.clone());
                move || {
                    if let (Some(t), Some(r)) = (term.upgrade(), relay.upgrade())
                        && let Some(r) = r.borrow().as_ref()
                    {
                        sync_relay_size(&t, r, shared.as_deref());
                    }
                }
            };
            let probe = gtk4::DrawingArea::new();
            probe.set_can_target(false);
            probe.set_focusable(false);
            let on_resize = sync.clone();
            probe.connect_resize(move |_, _, _| on_resize());
            overlay.add_overlay(&probe);
            terminal.connect_char_size_changed(move |_, _, _| {
                let sync = sync.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(50), sync);
            });
        }
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
            relay,
            exit,
            images,
        }
    }

    pub fn read_screen(&self) -> String {
        self.terminal
            .text_format(vte4::Format::Text)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    /// The absolute row of the screen's first line. VTE numbers rows from the start of its
    /// buffer, not from the screen, so history is the rows just above this one — a negative
    /// row is not "N lines up" but a row that does not exist (reads as blank).
    pub fn screen_top_row(&self) -> i64 {
        let rows = self.terminal.row_count();
        // The adjustment's upper bound is the buffer's end only once the widget is laid out; the
        // cursor row is a floor for it that is always current.
        let upper = self.terminal.vadjustment().map_or(0, |a| a.upper() as i64);
        let (_, cursor_row) = self.terminal.cursor_position();
        (upper.max(cursor_row + 1) - rows).max(0)
    }

    /// Rows are VTE's absolute rows (see [`Self::screen_top_row`]); cols are 0-based.
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

    /// Best-effort current cwd: OSC 7 first, then `/proc/<pid>/cwd` for shells that do not emit it.
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
        // Child gone (shell exited): last_cwd keeps the OSC 7 value or the spawn-time cwd.
        self.last_cwd.borrow().clone()
    }

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

    pub fn feed_input(&self, text: &str) {
        self.terminal.feed_child(text.as_bytes());
    }

    pub fn apply_config(&self, config: &CopadConfig) {
        let font_desc = gtk4::pango::FontDescription::from_string(&format!(
            "{} {}",
            config.terminal.font_family, config.terminal.font_size
        ));
        self.terminal.set_font(Some(&font_desc));
        let lines = config.terminal.scrollback as i64;
        if self.terminal.scrollback_lines() != lines {
            self.terminal.set_scrollback_lines(lines);
        }
        if self.terminal.enables_sixel() != config.terminal.images {
            apply_images(&self.terminal, config.terminal.images);
        }
        // The image shadow exists only for panels created with `[terminal] images`, so turning
        // images on later reaches new panels only (decisions #190).
        if let Some((shared, layers)) = &self.images {
            let on = config.terminal.images;
            if shared
                .graphics
                .enabled
                .swap(on, std::sync::atomic::Ordering::Relaxed)
                != on
            {
                layers.below.queue_draw();
                layers.above.queue_draw();
            }
        }
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

/// Clamp into [0, 1]. Non-finite input (reachable from a malformed TOML float) collapses to
/// fully opaque; `f64::clamp` would propagate NaN into CSS strings and widget alpha.
pub fn norm_opacity(o: f64) -> f64 {
    if o.is_finite() {
        o.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

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
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn shell_metacharacters_are_inert() {
        assert_eq!(shell_quote("a; rm -rf /"), "'a; rm -rf /'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
        assert_eq!(shell_quote("`id`"), "'`id`'");
        assert_eq!(shell_quote("a && b"), "'a && b'");
        assert_eq!(shell_quote("*.rs"), "'*.rs'");
    }

    #[test]
    fn quote_breakout_attempt_stays_quoted() {
        // A filename that tries to close our quote and append a command stays one word.
        let hostile = "'; rm -rf / #";
        let quoted = shell_quote(hostile);
        assert_eq!(quoted, r"''\''; rm -rf / #'");
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
        // Bare host, no path: return whatever is left so the caller gets a non-empty hint.
        assert_eq!(normalize_osc7_uri("file://lonely-host"), "lonely-host");
    }

    #[test]
    fn percent_decodes_path_segments() {
        // The restored cwd would not exist on disk for a path with a space or accent otherwise.
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
        assert_eq!(rgba_css("#ffffff", 2.0), "rgba(255,255,255,1)");
    }
}
