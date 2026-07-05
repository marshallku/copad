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
    // Async decode pipeline (mirrors macOS's off-main NSImage decode +
    // backgroundLoadToken). `gdk::Texture::from_file` decoded on the GTK main
    // thread, stalling VTE's PTY IO-watch on large wallpapers; now decode runs
    // on gio's blocking pool and only the ready texture is mounted on main.
    // Bumped by every transition so a slow decode landing after a newer
    // request/clear is dropped instead of resurrecting a stale image.
    load_generation: Cell<u64>,
    // At most one decode in flight; a request arriving mid-decode is coalesced
    // into `pending` (latest wins), so `background.next` spam bounds decode
    // work to 1-in-flight + 1-queued instead of a full 4K decode per keypress.
    decoding: Cell<bool>,
    pending: RefCell<Option<(PathBuf, bool)>>,
    // The image identity actually on screen (set only on a successful mount,
    // cleared by `clear_image`). `current` tracks the latest *request* for
    // command semantics; `mounted` is the rollback target when a decode fails
    // so logical state never claims an image that never painted.
    mounted: RefCell<Option<(PathBuf, bool)>>,
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
            load_generation: Cell::new(0),
            decoding: Cell::new(false),
            pending: RefCell::new(None),
            mounted: RefCell::new(None),
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

    pub fn set_image(self: &Rc<Self>, path: &Path) {
        self.apply_image(path, false);
    }

    /// Like [`set_image`], but marks the image as picked from the wallpaper
    /// list — the only kind `background.delete_current` will delete.
    pub fn set_image_from_list(self: &Rc<Self>, path: &Path) {
        self.apply_image(path, true);
    }

    fn apply_image(self: &Rc<Self>, path: &Path, from_list: bool) {
        eprintln!("[copad] background.set_image: {}", path.display());

        if !path.exists() {
            eprintln!(
                "[copad] background image does not exist: {}",
                path.display()
            );
            return;
        }

        // Logical state is synchronous — like macOS setting `currentBackgroundPath`
        // before the async decode — so `background.next` immediately followed by
        // `delete_current` operates on the just-picked image, not the previous one.
        *self.current.borrow_mut() = Some((path.to_path_buf(), from_list));
        self.has_image.set(true);

        // Every request supersedes any in-flight decode (stale-drop guard).
        self.load_generation.set(self.load_generation.get() + 1);

        // Coalesce: with a decode already running, keep only the latest request;
        // the running decode's completion drains it.
        if self.decoding.get() {
            *self.pending.borrow_mut() = Some((path.to_path_buf(), from_list));
            return;
        }

        self.spawn_decode(path.to_path_buf(), from_list);
    }

    /// Bump the load generation and drop any queued request so an in-flight
    /// decode landing later is discarded rather than mounted. Used by paths that
    /// decide "keep/clear what's shown" without starting a new decode
    /// (`clear_image`, config reloads that keep the current image).
    fn invalidate_pending(&self) {
        self.load_generation.set(self.load_generation.get() + 1);
        *self.pending.borrow_mut() = None;
    }

    /// Decode `path` on gio's blocking pool, then mount the texture on the main
    /// thread if it is still the latest request. Only `glib::Bytes` + dimensions
    /// cross the thread boundary — GDK/pixbuf objects are not `Send`.
    fn spawn_decode(self: &Rc<Self>, path: PathBuf, from_list: bool) {
        let generation = self.load_generation.get();
        self.decoding.set(true);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let decode_path = path.clone();
            let result = gtk4::gio::spawn_blocking(move || decode_image(&decode_path)).await;
            let Some(layer) = weak.upgrade() else {
                return;
            };
            // `spawn_blocking` reports a panic in the decode as `Err`.
            let decoded = result.unwrap_or_else(|_| Err("decode task panicked".to_string()));
            layer.on_decode_complete(generation, path, from_list, decoded);
        });
    }

    fn on_decode_complete(
        self: &Rc<Self>,
        generation: u64,
        path: PathBuf,
        from_list: bool,
        decoded: Result<DecodedImage, String>,
    ) {
        self.decoding.set(false);

        // Mount only if no newer request/clear superseded this decode.
        if generation == self.load_generation.get() {
            match decoded {
                Ok(image) => {
                    self.mount_texture(image);
                    *self.mounted.borrow_mut() = Some((path, from_list));
                }
                Err(e) => {
                    eprintln!("[copad] FAILED to load background image: {e}");
                    // `current`/`has_image` were committed synchronously at
                    // request time; the requested image never mounted, so roll
                    // them back to what is actually on screen. Otherwise
                    // `delete_current` could delete a list image that was never
                    // displayed (its "currently displayed" contract).
                    let mounted = self.mounted.borrow().clone();
                    self.has_image.set(mounted.is_some());
                    *self.current.borrow_mut() = mounted;
                }
            }
        }

        // Drain the coalesced request (already bumped to the latest generation).
        let next = self.pending.borrow_mut().take();
        if let Some((path, from_list)) = next {
            self.spawn_decode(path, from_list);
        }
    }

    fn mount_texture(&self, image: DecodedImage) {
        eprintln!(
            "[copad] background texture loaded: {}x{}",
            image.width, image.height
        );
        let format = if image.has_alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        let texture = gdk::MemoryTexture::new(
            image.width,
            image.height,
            format,
            &image.bytes,
            image.rowstride as usize,
        );
        self.bg_picture.set_paintable(Some(&texture));
        self.bg_picture.set_visible(true);
        self.bg_picture.set_opacity(self.image_opacity.get());
        self.tint_overlay.set_visible(true);
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
    pub fn rotate_once(self: &Rc<Self>) {
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
        self.invalidate_pending();
        self.bg_picture.set_visible(false);
        self.tint_overlay.set_visible(false);
        self.has_image.set(false);
        *self.current.borrow_mut() = None;
        *self.mounted.borrow_mut() = None;
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

    pub fn apply_config(self: &Rc<Self>, config: &CopadConfig, theme_bg: &str) {
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
                    // Don't silently ignore a config typo; surface it and keep
                    // the previously rendered image so the user can fix the path
                    // without flicker. Drop a pending *static* decode that's now
                    // unwanted — but never a list/rotation pick: config changes
                    // must not disturb rotation (`current` is synchronous, so
                    // `showing_list_image()` reflects the pending decode's kind).
                    if !self.showing_list_image() {
                        self.invalidate_pending();
                    }
                    eprintln!(
                        "[copad] background.image points at {} which does not exist; \
                         keeping previously rendered image",
                        path.display()
                    );
                }
            }
            None => {
                // A rotated wallpaper isn't config-driven — a reload that merely
                // touched tint/opacity/interval must not clear it or drop its
                // in-flight decode. Only a static image is cleared, and
                // `clear_image` invalidates that static decode itself.
                if self.has_image.get() && !self.showing_list_image() {
                    self.clear_image();
                }
            }
        }
    }
}

/// Raw decoded pixels handed from the blocking decode thread to the main
/// thread. Only `Send` types: GDK/pixbuf objects are not `Send`, so the pixbuf
/// is consumed inside [`decode_image`] and never escapes it.
struct DecodedImage {
    bytes: glib::Bytes,
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
}

/// Decode an image file to raw pixels off the main thread (runs on gio's
/// blocking pool via `spawn_blocking`; must not touch GTK widgets).
fn decode_image(path: &Path) -> Result<DecodedImage, String> {
    let pixbuf = gtk4::gdk_pixbuf::Pixbuf::from_file(path).map_err(|e| e.to_string())?;
    // `gdk::Texture::from_file` honors EXIF orientation; pixbuf does not unless
    // asked, so match it to avoid sideways JPEG wallpapers.
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    Ok(DecodedImage {
        width: pixbuf.width(),
        height: pixbuf.height(),
        rowstride: pixbuf.rowstride(),
        has_alpha: pixbuf.has_alpha(),
        bytes: pixbuf.read_pixel_bytes(),
    })
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
