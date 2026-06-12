use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;

use copad_core::config::CopadConfig;

use crate::terminal::{norm_opacity, parse_color, rgba_css};

const WALLPAPER_CACHE: &str = ".cache/terminal-wallpapers.txt";
const BG_MODE_FILE: &str = ".cache/copad-bg-mode";

/// Linux locations of the wallpaper list + rotation mode flag — shared by
/// the socket handlers and the native rotation timer.
pub fn bg_paths() -> copad_core::background::BackgroundPaths {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    copad_core::background::BackgroundPaths {
        primary_list: home.join(WALLPAPER_CACHE),
        fallback_list: None,
        mode_file: home.join(BG_MODE_FILE),
    }
}

/// Image + tint mounted as the `gtk4::Overlay` base child in
/// `CopadWindow`. Statusbar / notebook / panels are layered on top as
/// transparent overlays so this layer shows through consistently.
pub struct BackgroundLayer {
    pub bg_picture: gtk4::Picture,
    pub tint_overlay: gtk4::Box,
    tint_css: gtk4::CssProvider,
    tint_opacity: Cell<f64>,
    tint_color: Cell<gdk::RGBA>,
    image_opacity: Cell<f64>,
    // `[window] opacity` — alpha of the solid backdrop color only. The image
    // and tint layers carry their own opacities (`background.opacity` /
    // `background.tint`), independent of this, so the backdrop can stay a
    // strong dark base under a faint image.
    window_opacity: Cell<f64>,
    has_image: Cell<bool>,
    // The window's own `background-color` — the bottom-most layer, an always
    // present `rgba(theme_bg, window_opacity)` base painted behind the image.
    // This layer owns it so a theme/opacity change refreshes it in one place.
    window_css: gtk4::CssProvider,
    theme_bg: RefCell<String>,
    // Native rotation (replaces the external copad-random-bg.sh daemon).
    // `current` remembers what's displayed; the bool marks whether it was
    // picked from the wallpaper list — `background.delete_current` only
    // ever deletes list-picked images, never a manually `set` file.
    current: RefCell<Option<(PathBuf, bool)>>,
    rotate_interval: Cell<u64>,
    rotation_source: RefCell<Option<glib::SourceId>>,
    // Cross-instance toggle propagation: every instance watches the shared
    // mode file and applies clear/pick on a flip, so `background.toggle`
    // against ONE instance reaches all of them (the retired script did
    // this by broadcasting to every gui-*.sock instead).
    last_mode_active: Cell<bool>,
    mode_monitor: RefCell<Option<gtk4::gio::FileMonitor>>,
    // One-shot guard so a missing/empty wallpaper list (rotation enabled but
    // nothing to pick) warns once instead of silently no-op'ing every tick.
    empty_list_warned: Cell<bool>,
}

impl BackgroundLayer {
    pub fn new(config: &CopadConfig, window_css: gtk4::CssProvider, theme_bg: &str) -> Rc<Self> {
        let window_opacity = norm_opacity(config.window.opacity);

        let bg_picture = gtk4::Picture::new();
        bg_picture.set_content_fit(gtk4::ContentFit::Cover);
        bg_picture.set_hexpand(true);
        bg_picture.set_vexpand(true);
        bg_picture.set_visible(false);
        bg_picture.set_opacity(config.background.opacity);
        // Don't intercept input — clicks must reach the panels above.
        bg_picture.set_can_target(false);

        let tint_overlay = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        tint_overlay.set_hexpand(true);
        tint_overlay.set_vexpand(true);
        tint_overlay.set_visible(false);
        tint_overlay.set_can_target(false);
        tint_overlay.add_css_class("copad-bg-tint");

        let tint_css = gtk4::CssProvider::new();
        update_tint_css(
            &tint_css,
            &config.background.tint_color,
            config.background.tint,
        );
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &tint_css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
        );

        let layer = Rc::new(Self {
            bg_picture,
            tint_overlay,
            tint_css,
            tint_opacity: Cell::new(config.background.tint),
            tint_color: Cell::new(parse_color(&config.background.tint_color)),
            image_opacity: Cell::new(config.background.opacity),
            window_opacity: Cell::new(window_opacity),
            has_image: Cell::new(false),
            window_css,
            theme_bg: RefCell::new(theme_bg.to_string()),
            current: RefCell::new(None),
            rotate_interval: Cell::new(config.background.rotate_interval),
            rotation_source: RefCell::new(None),
            last_mode_active: Cell::new(copad_core::background::is_active(&bg_paths().mode_file)),
            mode_monitor: RefCell::new(None),
            empty_list_warned: Cell::new(false),
        });

        layer.refresh_window_backdrop();

        if let Some(ref path) = config.background.image {
            let p = Path::new(path);
            if p.exists() {
                layer.set_image(p);
            }
        }

        layer
    }

    pub fn set_image(&self, path: &Path) {
        self.apply_image(path, false);
    }

    /// Like [`set_image`], but marks the image as picked from the wallpaper
    /// list — the only kind `background.delete_current` will delete.
    pub fn set_image_from_list(&self, path: &Path) {
        self.apply_image(path, true);
    }

    fn apply_image(&self, path: &Path, from_list: bool) {
        eprintln!("[copad] background.set_image: {}", path.display());

        if !path.exists() {
            eprintln!(
                "[copad] background image does not exist: {}",
                path.display()
            );
            return;
        }

        let file = gtk4::gio::File::for_path(path);
        match gdk::Texture::from_file(&file) {
            Ok(texture) => {
                eprintln!(
                    "[copad] background texture loaded: {}x{}",
                    texture.width(),
                    texture.height()
                );
                self.bg_picture.set_paintable(Some(&texture));
            }
            Err(e) => {
                eprintln!(
                    "[copad] FAILED to load background image {}: {}",
                    path.display(),
                    e
                );
                return;
            }
        }

        self.bg_picture.set_visible(true);
        self.bg_picture.set_opacity(self.image_opacity.get());
        self.tint_overlay.set_visible(true);
        self.has_image.set(true);
        *self.current.borrow_mut() = Some((path.to_path_buf(), from_list));
    }

    /// The displayed image's path, only when it came from the wallpaper list.
    pub fn current_list_image(&self) -> Option<PathBuf> {
        self.current
            .borrow()
            .as_ref()
            .and_then(|(p, from_list)| from_list.then(|| p.clone()))
    }

    /// True while the displayed image is a rotation/list pick (used by the
    /// config hot-reload to keep rotated wallpapers when `[background] image`
    /// is unset).
    fn showing_list_image(&self) -> bool {
        matches!(self.current.borrow().as_ref(), Some((_, true)))
    }

    /// (Re)start the rotation timer from the configured interval; 0 stops it.
    /// Also the manual-change hook: `background.set`/`next` call this so the
    /// countdown restarts after a manual pick (the retired script did the
    /// same via file mtimes).
    pub fn arm_rotation(self: &Rc<Self>) {
        if let Some(id) = self.rotation_source.borrow_mut().take() {
            id.remove();
        }
        let interval = self.rotate_interval.get();
        if interval == 0 {
            return;
        }
        // Surface an empty/missing list now rather than `interval` seconds later
        // on the first tick — probe only (the actual pick happens on the tick).
        let paths = bg_paths();
        if copad_core::background::is_active(&paths.mode_file) {
            let _ = self.pick_or_warn(&paths);
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local(interval.min(u32::MAX as u64) as u32, move || {
            let Some(layer) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            layer.rotate_once();
            glib::ControlFlow::Continue
        });
        *self.rotation_source.borrow_mut() = Some(id);
    }

    /// One rotation tick: respect the shared mode flag, pick a random list
    /// image, apply it. No-op when the list is missing/empty (warned once).
    pub fn rotate_once(&self) {
        let paths = bg_paths();
        if !copad_core::background::is_active(&paths.mode_file) {
            return;
        }
        if let Some(img) = self.pick_or_warn(&paths) {
            self.set_image_from_list(Path::new(&img));
        }
    }

    /// `pick_random`, but the first time rotation is active yet the list yields
    /// no image, log the cause — otherwise a user who set `rotate_interval`
    /// without ever populating `terminal-wallpapers.txt` sees nothing happen
    /// and no reason why. Warns once per process; a successful pick re-arms it
    /// so a later emptied list warns again.
    fn pick_or_warn(&self, paths: &copad_core::background::BackgroundPaths) -> Option<String> {
        match copad_core::background::pick_random(paths) {
            Some(img) => {
                self.empty_list_warned.set(false);
                Some(img)
            }
            None => {
                if note_empty_list(&self.empty_list_warned) {
                    eprintln!(
                        "[copad] background rotation is enabled but no wallpaper is available — \
                         add image paths (one per line) to {}",
                        paths.primary_list.display(),
                    );
                }
                None
            }
        }
    }

    /// Record the mode this instance just applied itself, so the file
    /// monitor's echo of our own `background.toggle` write is a no-op
    /// instead of a second random pick.
    pub fn note_mode_applied(&self, active: bool) {
        self.last_mode_active.set(active);
    }

    /// Watch the shared mode file so a `background.toggle` against ANY
    /// instance propagates here: flip→deactive clears the image,
    /// flip→active picks a fresh one. Armed regardless of
    /// `rotate_interval` — the retired script broadcast its toggle to
    /// every instance, and interval-less instances still participated.
    pub fn arm_mode_watch(self: &Rc<Self>) {
        let mode_file = bg_paths().mode_file;
        let gfile = gtk4::gio::File::for_path(&mode_file);
        let monitor = match gfile.monitor_file(
            gtk4::gio::FileMonitorFlags::NONE,
            gtk4::gio::Cancellable::NONE,
        ) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[copad] background mode watch unavailable: {e}");
                return;
            }
        };
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, _| {
            let Some(layer) = weak.upgrade() else { return };
            let active = copad_core::background::is_active(&bg_paths().mode_file);
            if active == layer.last_mode_active.get() {
                return;
            }
            layer.last_mode_active.set(active);
            if active {
                if let Some(img) = layer.pick_or_warn(&bg_paths()) {
                    layer.set_image_from_list(Path::new(&img));
                }
            } else {
                layer.clear_image();
            }
            layer.arm_rotation();
        });
        *self.mode_monitor.borrow_mut() = Some(monitor);
    }

    /// The window's `background-color`: `rgba(theme_bg, window_opacity)`, the
    /// always-present dark base. Independent of image state — the image is a
    /// separate layer painted on top with its own `background.opacity`, so the
    /// base stays put underneath it. Re-run when `window_opacity` or the theme
    /// color changes.
    fn refresh_window_backdrop(&self) {
        self.window_css.load_from_string(&format!(
            "window {{ background-color: {}; }}",
            rgba_css(&self.theme_bg.borrow(), self.window_opacity.get())
        ));
    }

    pub fn clear_image(&self) {
        eprintln!("[copad] background.clear_image");
        self.bg_picture.set_visible(false);
        self.tint_overlay.set_visible(false);
        self.has_image.set(false);
        *self.current.borrow_mut() = None;
    }

    pub fn set_tint(&self, opacity: f64) {
        self.tint_opacity.set(opacity);
        let c = self.tint_color.get();
        update_tint_css(
            &self.tint_css,
            &format!(
                "#{:02x}{:02x}{:02x}",
                (c.red() * 255.0) as u8,
                (c.green() * 255.0) as u8,
                (c.blue() * 255.0) as u8,
            ),
            opacity,
        );
    }

    pub fn apply_config(&self, config: &CopadConfig, theme_bg: &str) {
        self.window_opacity.set(norm_opacity(config.window.opacity));
        *self.theme_bg.borrow_mut() = theme_bg.to_string();
        self.refresh_window_backdrop();

        self.tint_opacity.set(config.background.tint);
        self.tint_color
            .set(parse_color(&config.background.tint_color));
        update_tint_css(
            &self.tint_css,
            &config.background.tint_color,
            config.background.tint,
        );

        self.image_opacity.set(config.background.opacity);
        if self.has_image.get() {
            self.bg_picture.set_opacity(config.background.opacity);
        }

        self.rotate_interval.set(config.background.rotate_interval);

        match &config.background.image {
            Some(image) => {
                let path = Path::new(image);
                if path.exists() {
                    self.set_image(path);
                } else {
                    // Don't silently ignore a config typo; surface it
                    // and keep the previously rendered image so the
                    // user can fix the path without flicker.
                    eprintln!(
                        "[copad] background.image points at {} which does not exist; \
                         keeping previously rendered image",
                        path.display()
                    );
                }
            }
            None => {
                // A rotated wallpaper isn't config-driven — a reload that
                // merely touched tint/opacity/interval must not clear it.
                if self.has_image.get() && !self.showing_list_image() {
                    self.clear_image();
                }
            }
        }
    }
}

fn update_tint_css(provider: &gtk4::CssProvider, hex_color: &str, opacity: f64) {
    let css = format!(
        ".copad-bg-tint {{ background-color: {}; }}",
        rgba_css(hex_color, opacity)
    );
    provider.load_from_string(&css);
}

/// One-shot guard for the empty-wallpaper-list warning: returns `true` the first time the list is
/// observed empty and flips `warned`, so subsequent ticks stay quiet. A successful pick resets
/// `warned` to `false` (in `pick_or_warn`), so a list that is later emptied warns again.
fn note_empty_list(warned: &Cell<bool>) -> bool {
    !warned.replace(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_warns_once_then_rearms_after_a_pick() {
        let warned = Cell::new(false);
        // First empty observation warns; the next ones stay quiet.
        assert!(note_empty_list(&warned), "first empty list must warn");
        assert!(
            !note_empty_list(&warned),
            "second consecutive empty must be quiet"
        );
        assert!(!note_empty_list(&warned));
        // A successful pick resets the guard (as pick_or_warn does) → warns again if re-emptied.
        warned.set(false);
        assert!(
            note_empty_list(&warned),
            "an emptied-again list warns after a successful pick"
        );
    }
}
