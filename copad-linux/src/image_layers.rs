//! Draws the relay shadow's images on a VTE terminal (decisions #190): one layer under VTE for
//! z < 0 (VTE's background is transparent in copad, so cells with their own background cover
//! it, as kitty specifies) and one over it. A layer shows the newest publication VTE has parsed
//! up to — learned from the `OSC 6` marker the relay writes after it (see `image_shadow`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use gtk4::cairo;
use gtk4::prelude::*;
use vte4::Terminal;
use vte4::prelude::*;

use crate::image_shadow::{ImageShared, Published, marker_seq};
use crate::kitty_relay::Relay;

pub struct ImageLayers {
    pub below: gtk4::DrawingArea,
    pub above: gtk4::DrawingArea,
}

#[derive(Default)]
struct State {
    shown: Option<Published>,
    /// Cairo copies of the shown images, by image key.
    surfaces: HashMap<u64, cairo::ImageSurface>,
}

/// Layers for `terminal`, fed by `shared`; `relay` is woken when a publication is acknowledged
/// (the relay holds new ones back while too many wait).
pub fn attach(
    terminal: &Terminal,
    shared: Arc<ImageShared>,
    relay: Weak<RefCell<Option<Relay>>>,
) -> ImageLayers {
    let state = Rc::new(RefCell::new(State::default()));
    let layer = |below: bool| {
        let area = gtk4::DrawingArea::new();
        area.set_can_target(false);
        area.set_focusable(false);
        area.set_hexpand(true);
        area.set_vexpand(true);
        let (state, term, shared) = (state.clone(), terminal.downgrade(), shared.clone());
        area.set_draw_func(move |_, cr, _, _| {
            if let Some(term) = term.upgrade()
                && shared.graphics.enabled.load(Ordering::Relaxed)
            {
                draw(&term, &mut state.borrow_mut(), cr, below);
            }
        });
        area
    };
    let layers = ImageLayers {
        below: layer(true),
        above: layer(false),
    };

    let redraw = {
        let (below, above) = (layers.below.downgrade(), layers.above.downgrade());
        move || {
            for area in [below.upgrade(), above.upgrade()].into_iter().flatten() {
                area.queue_draw();
            }
        }
    };
    // VTE parsed a marker: show the publication it stands for.
    {
        let (state, shared, redraw) = (state.clone(), shared.clone(), redraw.clone());
        terminal.connect_notify_local(Some("current-file-uri"), move |t, _| {
            let Some(seq) = t.current_file_uri().as_deref().and_then(marker_seq) else {
                return;
            };
            if let Some(published) = shared.acknowledge(seq) {
                let mut st = state.borrow_mut();
                let keep: std::collections::HashSet<u64> =
                    published.images.iter().map(|i| i.key).collect();
                st.surfaces.retain(|key, _| keep.contains(key));
                st.shown = Some(published);
                drop(st);
                redraw();
            }
            if let Some(relay) = relay.upgrade()
                && let Some(relay) = relay.borrow().as_ref()
            {
                relay.wake_output();
            }
        });
    }
    // Scrolling back moves the images with the text.
    if let Some(adj) = terminal.vadjustment() {
        let (state, redraw) = (state.clone(), redraw.clone());
        adj.connect_value_changed(move |_| {
            if state
                .borrow()
                .shown
                .as_ref()
                .is_some_and(|p| !p.runs.is_empty())
            {
                redraw();
            }
        });
    }
    layers
}

/// How many rows VTE is scrolled back from the live screen.
fn scrolled_back(terminal: &Terminal) -> f64 {
    terminal.vadjustment().map_or(0.0, |a| {
        (a.upper() - a.page_size() - a.value()).max(0.0).round()
    })
}

fn draw(terminal: &Terminal, st: &mut State, cr: &cairo::Context, below: bool) {
    let State { shown, surfaces } = st;
    let Some(shown) = shown.as_ref() else { return };
    if shown.runs.is_empty() {
        return;
    }
    let (cw, ch) = (terminal.char_width() as f64, terminal.char_height() as f64);
    if cw <= 0.0 || ch <= 0.0 {
        return;
    }
    #[allow(deprecated)]
    let pad = terminal.style_context().padding();
    let (px, py) = (pad.left() as f64, pad.top() as f64);
    // Published rows count from the top of the shadow's history; the view starts that many
    // rows down, less what VTE is scrolled back.
    let top = shown.history as f64 - scrolled_back(terminal);
    let rows = terminal.row_count() as f64;
    for run in shown.runs.iter().filter(|r| (r.z < 0) == below) {
        let (y0, y1) = (run.rect[1] as f64 - top, run.rect[3] as f64 - top);
        if y1 <= 0.0 || y0 >= rows {
            continue;
        }
        let Some(image) = shown.images.get(run.image as usize) else {
            continue;
        };
        let surface = match surfaces.get(&image.key) {
            Some(s) => s.clone(),
            None => match to_surface(image) {
                Some(s) => {
                    surfaces.insert(image.key, s.clone());
                    s
                }
                None => continue,
            },
        };
        let dest = (
            px + run.rect[0] as f64 * cw,
            py + y0 * ch,
            (run.rect[2] - run.rect[0]) as f64 * cw,
            (y1 - y0) * ch,
        );
        let (uw, uh) = (
            (run.uv[2] - run.uv[0]) as f64,
            (run.uv[3] - run.uv[1]) as f64,
        );
        if uw <= 0.0 || uh <= 0.0 {
            continue;
        }
        // Where the whole image would land for this slice to fall on `dest`.
        let (fw, fh) = (dest.2 / uw, dest.3 / uh);
        let (fx, fy) = (
            dest.0 - run.uv[0] as f64 * fw,
            dest.1 - run.uv[1] as f64 * fh,
        );
        cr.save().ok();
        cr.rectangle(dest.0, dest.1, dest.2, dest.3);
        cr.clip();
        cr.translate(fx, fy);
        cr.scale(fw / image.width as f64, fh / image.height as f64);
        if cr.set_source_surface(&surface, 0.0, 0.0).is_ok() {
            cr.source().set_filter(cairo::Filter::Good);
            cr.paint().ok();
        }
        cr.restore().ok();
    }
}

/// Premultiplied RGBA → cairo's ARGB32 (native-endian, so BGRA in memory on little-endian).
fn to_surface(image: &alacritty_terminal::term::cell::GraphicImage) -> Option<cairo::ImageSurface> {
    let (w, h) = (image.width as i32, image.height as i32);
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().ok()?;
        for (y, row) in image.rgba.chunks_exact(w as usize * 4).enumerate() {
            let out = &mut data[y * stride..y * stride + w as usize * 4];
            for (px, o) in row.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                let argb = u32::from_be_bytes([px[3], px[0], px[1], px[2]]);
                o.copy_from_slice(&argb.to_ne_bytes());
            }
        }
    }
    surface.mark_dirty();
    Some(surface)
}
