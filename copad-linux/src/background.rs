use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;

use copad_core::background::DirSource;
use copad_core::config::CopadConfig;

use crate::terminal::{norm_opacity, parse_color, rgba_css};

const WALLPAPER_CACHE: &str = ".cache/terminal-wallpapers.txt";
const BG_MODE_FILE: &str = ".cache/copad-bg-mode";

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Rotation on/off flag. Deliberately NOT configurable: it is internal
/// cross-instance state, not user content, and every instance must agree on
/// the path for `background.toggle` to propagate.
pub fn mode_file() -> PathBuf {
    home_dir().join(BG_MODE_FILE)
}

/// Where the wallpaper list lives when `[background] list` is unset.
fn default_list_path() -> PathBuf {
    home_dir().join(WALLPAPER_CACHE)
}

/// Linux locations of the wallpaper list + rotation mode flag. The list is
/// `[background] list` when set, else the legacy cache path (so an existing
/// install keeps working with no config).
pub fn bg_paths(config: &CopadConfig) -> copad_core::background::BackgroundPaths {
    copad_core::background::BackgroundPaths {
        primary_list: config
            .background
            .list_path()
            .unwrap_or_else(default_list_path),
        fallback_list: None,
        mode_file: mode_file(),
    }
}

/// Where random picks come from. `[background] image` pointing at a
/// directory makes it the source and bypasses the list file entirely;
/// anything else (a plain file, or unset) falls back to the list.
enum WallpaperSource {
    Dir(DirSource),
    List,
}

impl WallpaperSource {
    fn resolve(config: &CopadConfig) -> Self {
        match config.background.source_path() {
            // A path that doesn't exist yet is NOT a directory, so it falls
            // through to `static_image` and gets the existing "does not
            // exist" warning rather than being silently treated as a source.
            Some(p) if p.is_dir() => Self::Dir(DirSource::new(
                p,
                config.background.recursive,
                &config.background.extensions,
            )),
            _ => Self::List,
        }
    }
}

/// `[background] image` when it names a static file — i.e. everything a
/// directory source is not. Returns None for a directory so the rotation
/// source is never also mounted as a literal image.
fn static_image(config: &CopadConfig) -> Option<PathBuf> {
    config.background.source_path().filter(|p| !p.is_dir())
}

/// Memoized directory listing. Scanning ~30k entries costs ~30ms, which is a
/// dropped frame on the GTK main loop, so the result is cached and only
/// rebuilt when the source config or the root's mtime changes.
///
/// Caveat for `recursive = true`: only the ROOT's mtime is watched, so a file
/// added deep in a subtree is picked up on the next config reload or restart
/// rather than immediately. Adding/removing files directly under the root
/// (the common case, and what `delete_current` does) invalidates correctly.
struct DirCache {
    source: DirSource,
    mtime: Option<SystemTime>,
    entries: Vec<PathBuf>,
}

fn dir_mtime(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(root).ok().and_then(|m| m.modified().ok())
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
    // Where `next`/rotation picks come from, and the list file backing the
    // `WallpaperSource::List` case. Both re-resolved on config reload.
    source: RefCell<WallpaperSource>,
    list_path: RefCell<PathBuf>,
    dir_cache: RefCell<Option<DirCache>>,
    // Entries found missing on disk. The legacy list file accumulates deleted
    // paths and is never rewritten by a failed pick (it may be hand-curated),
    // so without remembering the misses every retry re-rolls against the same
    // dead lines. Reset whenever the source is re-resolved.
    missing: RefCell<HashSet<PathBuf>>,
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
            source: RefCell::new(WallpaperSource::resolve(config)),
            list_path: RefCell::new(bg_paths(config).primary_list),
            dir_cache: RefCell::new(None),
            missing: RefCell::new(HashSet::new()),
            last_mode_active: Cell::new(copad_core::background::is_active(&mode_file())),
            mode_monitor: RefCell::new(None),
            empty_list_warned: Cell::new(false),
            load_generation: Cell::new(0),
            decoding: Cell::new(false),
            pending: RefCell::new(None),
            mounted: RefCell::new(None),
        });

        layer.refresh_window_backdrop();

        match static_image(config) {
            Some(path) if path.exists() => layer.set_image(&path),
            _ => layer.apply_initial_dir_pick(),
        }

        layer
    }

    /// Mount a wallpaper immediately when `[background] image` names a
    /// directory. A static image applies at startup, so a directory source
    /// must too — otherwise pointing the key at a wallpaper folder shows
    /// nothing at all until the first rotation tick, and at the default
    /// `rotate_interval = 0` there is no tick, so it would look broken.
    ///
    /// Deliberately NOT done for a list source: that would change what
    /// existing installs (populated list, `rotate_interval = 0`) render at
    /// startup. A directory source is new, so nothing regresses.
    fn apply_initial_dir_pick(self: &Rc<Self>) {
        if !matches!(&*self.source.borrow(), WallpaperSource::Dir(_)) || !self.is_active() {
            return;
        }
        if let Some(img) = self.pick_or_warn() {
            self.set_image_from_list(Path::new(&img));
        }
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
        // Surface an empty/missing source now rather than `interval` seconds
        // later on the first tick — probe only (the actual pick happens on the
        // tick). Also warms the directory cache so the first tick is instant.
        if self.is_active() {
            let _ = self.pick_or_warn();
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
        if !self.is_active() {
            return;
        }
        if let Some(img) = self.pick_or_warn() {
            self.set_image_from_list(Path::new(&img));
        }
    }

    /// Rotation on/off, shared across instances via the mode file.
    pub fn is_active(&self) -> bool {
        copad_core::background::is_active(&mode_file())
    }

    /// Flip the shared rotation flag; returns the new state.
    pub fn toggle_mode(&self) -> bool {
        copad_core::background::toggle(&mode_file())
    }

    /// Pick a wallpaper from whichever source is configured, skipping entries
    /// that have disappeared. A stale entry is expected — the 24k-line legacy
    /// list accumulates deleted files, and a cached directory listing can lag
    /// a deletion — so retrying turns "File not found" into a working
    /// rotation. Every miss is remembered (see `missing`) so the candidate
    /// pool shrinks monotonically and retries can't re-roll the same dead
    /// entry. Bounded anyway: a mostly-dead source must fail in bounded time
    /// on the GTK main loop rather than stat its way through 24k lines.
    pub fn pick(&self) -> Option<String> {
        const ATTEMPTS: usize = 16;
        for _ in 0..ATTEMPTS {
            let candidate = match &*self.source.borrow() {
                WallpaperSource::Dir(source) => self.pick_from_dir(source),
                WallpaperSource::List => self.pick_from_list(),
            }?;
            if Path::new(&candidate).exists() {
                return Some(candidate);
            }
            // Don't let a vanished file be re-picked on the next attempt.
            self.forget_entry(Path::new(&candidate));
        }
        None
    }

    /// List-file pick, minus entries already found missing this process. The
    /// list is deliberately NOT rewritten here — it may be hand-curated, and a
    /// temporarily unreachable path (unmounted drive) must not be destroyed by
    /// a rotation tick. `background.delete_current` is the one path that edits
    /// it, because there the deletion is what the user asked for.
    fn pick_from_list(&self) -> Option<String> {
        let entries = copad_core::background::list_entries(&self.paths());
        let missing = self.missing.borrow();
        let live: Vec<&String> = entries
            .iter()
            .filter(|e| !missing.contains(Path::new(e.as_str())))
            .collect();
        copad_core::background::pick_one(&live).map(|e| (*e).clone())
    }

    fn paths(&self) -> copad_core::background::BackgroundPaths {
        copad_core::background::BackgroundPaths {
            primary_list: self.list_path.borrow().clone(),
            fallback_list: None,
            mode_file: mode_file(),
        }
    }

    fn pick_from_dir(&self, source: &DirSource) -> Option<String> {
        self.ensure_dir_cache(source);
        let cache = self.dir_cache.borrow();
        let entries = &cache.as_ref()?.entries;
        copad_core::background::pick_one(entries).map(|p| p.to_string_lossy().into_owned())
    }

    /// Rescan only when the source config or the root's mtime moved. On a
    /// scan failure the cache is stored EMPTY (rather than left stale or
    /// cleared) so the failure is reported once by `pick_or_warn` instead of
    /// re-scanning on every tick.
    fn ensure_dir_cache(&self, source: &DirSource) {
        let mtime = dir_mtime(&source.root);
        let fresh = self
            .dir_cache
            .borrow()
            .as_ref()
            .is_some_and(|c| c.source == *source && c.mtime == mtime);
        if fresh {
            return;
        }
        let entries = match copad_core::background::scan_dir(source) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!(
                    "[copad] cannot scan background directory {}: {e}",
                    source.root.display()
                );
                Vec::new()
            }
        };
        *self.dir_cache.borrow_mut() = Some(DirCache {
            source: source.clone(),
            mtime,
            entries,
        });
    }

    /// Stop picking `path`: remember it as missing (the only thing that helps
    /// a list source, whose file we won't rewrite) and drop it from the
    /// in-memory directory listing — deleting a file nested under a
    /// `recursive` root does not move the root's mtime, so the cache would
    /// otherwise keep offering it.
    fn forget_entry(&self, path: &Path) {
        self.missing.borrow_mut().insert(path.to_path_buf());
        if let Some(cache) = self.dir_cache.borrow_mut().as_mut() {
            cache.entries.retain(|p| p != path);
        }
    }

    /// Remove `path` from whatever source produced it: the list file for a
    /// list source, the in-memory listing for a directory source (the file
    /// itself is deleted by the caller). Returns whether anything changed.
    pub fn drop_from_source(&self, path: &Path) -> std::io::Result<bool> {
        self.forget_entry(path);
        match &*self.source.borrow() {
            // A directory source has no list to rewrite — the file is gone
            // from disk, which is the whole record.
            WallpaperSource::Dir(_) => Ok(true),
            WallpaperSource::List => copad_core::background::remove_from_list(
                &self.list_path.borrow(),
                &path.to_string_lossy(),
            ),
        }
    }

    /// Human-readable description of the active source, for error messages.
    fn source_hint(&self) -> String {
        match &*self.source.borrow() {
            WallpaperSource::Dir(source) => format!(
                "no images matching {:?} found in {}",
                source.extensions,
                source.root.display()
            ),
            WallpaperSource::List => format!(
                "add image paths (one per line) to {}, or point `[background] image` at a \
                 directory",
                self.list_path.borrow().display()
            ),
        }
    }

    /// [`pick`](Self::pick), but the first time rotation is active yet the
    /// source yields no image, log the cause — otherwise a user who set
    /// `rotate_interval` without a usable source sees nothing happen and no
    /// reason why. Warns once per process; a successful pick re-arms it so a
    /// source that later empties warns again.
    fn pick_or_warn(&self) -> Option<String> {
        match self.pick() {
            Some(img) => {
                self.empty_list_warned.set(false);
                Some(img)
            }
            None => {
                if note_empty_list(&self.empty_list_warned) {
                    eprintln!(
                        "[copad] background rotation is enabled but no wallpaper is available — {}",
                        self.source_hint(),
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
        let gfile = gtk4::gio::File::for_path(mode_file());
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
            let active = layer.is_active();
            if active == layer.last_mode_active.get() {
                return;
            }
            layer.last_mode_active.set(active);
            if active {
                if let Some(img) = layer.pick_or_warn() {
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

        // Re-resolve the pick source. Dropping the directory cache is what
        // makes a hot reload pick up files added under a `recursive` root,
        // whose subtree mtimes the cache doesn't watch.
        *self.source.borrow_mut() = WallpaperSource::resolve(config);
        *self.list_path.borrow_mut() = bg_paths(config).primary_list;
        *self.dir_cache.borrow_mut() = None;
        // A reload is also the retry point for paths that were missing
        // earlier: the drive may be mounted now, or the list replaced.
        self.missing.borrow_mut().clear();

        match static_image(config) {
            Some(path) => {
                let path = path.as_path();
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
                // Switching `image` from a file to a DIRECTORY lands here (the
                // static image was just cleared above). Mount a pick right away
                // for the same reason startup does — otherwise the reload leaves
                // a blank background until a tick that never comes at
                // `rotate_interval = 0`.
                if !self.has_image.get() {
                    self.apply_initial_dir_pick();
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
