use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::prelude::*;

use copad_core::config::CopadConfig;

use crate::terminal::{norm_opacity, parse_color, rgba_css};

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
                if self.has_image.get() {
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
