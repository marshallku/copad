//! Images in comux panes, shown by the terminal a client runs in (decisions #194).
//!
//! Panes run copad's kitty graphics code (`copad-graphics`), so their cells carry image
//! fragments. While composing, comux turns the visible ones into screen-space row runs
//! ([`ScreenRun`]) — clipped to the pane and to what chrome left of it — and each client whose
//! terminal draws kitty graphics gets them as [`ImageRun`]s with every frame, plus the image
//! data it lacks. The client places them over the cells after drawing every frame (re-placing
//! also restores images a terminal like copad erases under rewritten text).
//!
//! Per client, what it holds advances only when a frame actually reached its queue: a dropped
//! frame is rebuilt later against the same state, so no frame can name an image the client
//! has not been given.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::term::cell::GraphicImage;
use base64::Engine;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Position;

use crate::proto::{ImageRun, ImageUpload};

/// Bytes of image data a client keeps in its terminal (kitty's and copad's default quota is
/// 320 MiB; this stays well under it so uploads never fail silently there).
pub const CLIENT_BUDGET: usize = 160 << 20;

/// One visible row-run of an image, in frame cells.
#[derive(Clone)]
pub struct ScreenRun {
    pub image: Arc<GraphicImage>,
    /// `[left, top, right, bottom]` in frame cells (fractional at the image's edges).
    pub rect: [f32; 4],
    /// `[left, top, right, bottom]` in the image, 0..=1.
    pub uv: [f32; 4],
    pub z: i32,
}

/// The visible runs of one pane: `runs` from `PaneTerm::image_runs`, in pane cells, placed at
/// `(x, y)` and clipped to `cols` x `rows`.
pub fn pane_runs(
    (runs, images): (Vec<copad_graphics::CopadGraphic>, Vec<Arc<GraphicImage>>),
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
) -> Vec<ScreenRun> {
    let mut out = Vec::new();
    for r in runs {
        let Some(image) = images.get(r.image as usize) else {
            continue;
        };
        // Rows are whole runs; clip to the pane's rows and columns, cutting the source with it.
        if r.rect[1] >= rows as f32 || r.rect[3] <= 0.0 {
            continue;
        }
        let Some((rect, uv)) = clip_x(r.rect, r.uv, 0.0, cols as f32) else {
            continue;
        };
        out.push(ScreenRun {
            image: image.clone(),
            rect: [
                rect[0] + x as f32,
                rect[1] + y as f32,
                rect[2] + x as f32,
                rect[3] + y as f32,
            ],
            uv,
            z: r.z,
        });
    }
    out
}

/// The part of a run between columns `lo` and `hi`, its source cut in proportion.
fn clip_x(rect: [f32; 4], uv: [f32; 4], lo: f32, hi: f32) -> Option<([f32; 4], [f32; 4])> {
    let (l, r) = (rect[0].max(lo), rect[2].min(hi));
    if l >= r {
        return None;
    }
    let w = rect[2] - rect[0];
    let at = |x: f32| uv[0] + (x - rect[0]) / w * (uv[2] - uv[0]);
    Some(([l, rect[1], r, rect[3]], [at(l), uv[1], at(r), uv[3]]))
}

/// What the panes wrote where their images are, before any chrome is drawn.
pub fn remember(runs: &[ScreenRun], buf: &Buffer) -> Vec<(Position, Cell)> {
    let mut cells = Vec::new();
    for r in runs {
        let y = r.rect[1].floor() as u16;
        for x in r.rect[0].floor() as u16..r.rect[2].ceil() as u16 {
            let at = Position::new(x, y);
            if let Some(c) = buf.cell(at) {
                cells.push((at, c.clone()));
            }
        }
    }
    cells
}

/// Cut out of each run the cells chrome drew over after the panes (a menu, the switcher, a
/// prompt): a run splits around them rather than vanishing.
pub fn uncovered(
    runs: Vec<ScreenRun>,
    before: &[(Position, Cell)],
    buf: &Buffer,
) -> Vec<ScreenRun> {
    let covered: HashSet<(u16, u16)> = before
        .iter()
        .filter(|(at, c)| buf.cell(*at) != Some(c))
        .map(|(at, _)| (at.x, at.y))
        .collect();
    if covered.is_empty() {
        return runs;
    }
    let mut out = Vec::new();
    for r in runs {
        let y = r.rect[1].floor() as u16;
        let (first, last) = (r.rect[0].floor() as u16, r.rect[2].ceil() as u16);
        let mut start = first;
        for x in first..=last {
            if x == last || covered.contains(&(x, y)) {
                if x > start
                    && let Some((rect, uv)) = clip_x(r.rect, r.uv, start as f32, x as f32)
                {
                    out.push(ScreenRun {
                        rect,
                        uv,
                        ..r.clone()
                    });
                }
                start = x + 1;
            }
        }
    }
    out
}

/// Client ids for images, server-wide and stable while an image lives.
#[derive(Default)]
pub struct ImageIds {
    by_key: HashMap<u64, u32>,
    next: u32,
    /// Encoded uploads, shared by every client that needs them this frame.
    encoded: HashMap<u64, Arc<String>>,
}

impl ImageIds {
    pub fn id(&mut self, key: u64) -> u32 {
        *self.by_key.entry(key).or_insert_with(|| {
            // Ids start high so they never meet a client's own small ones; 0 is not an id.
            self.next = self.next.wrapping_add(1).max(1);
            0x0100_0000 | (self.next & 0x00FF_FFFF)
        })
    }

    /// Forget images no run shows any more (their ids are not reused while referenced).
    pub fn retain(&mut self, shown: &HashSet<u64>) {
        self.by_key.retain(|k, _| shown.contains(k));
        self.encoded.retain(|k, _| shown.contains(k));
    }

    fn upload(&mut self, id: u32, image: &GraphicImage) -> ImageUpload {
        let data = self
            .encoded
            .entry(image.key)
            .or_insert_with(|| Arc::new(encode(image)))
            .clone();
        ImageUpload {
            id,
            width: image.width,
            height: image.height,
            data: data.as_ref().clone(),
        }
    }
}

/// Straight RGBA, zlib, base64 (kitty `f=32,o=z`); the cells hold premultiplied pixels.
fn encode(image: &GraphicImage) -> String {
    let mut straight = image.rgba.clone();
    for px in straight.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = z.write_all(&straight);
    base64::engine::general_purpose::STANDARD.encode(z.finish().unwrap_or_default())
}

/// What one graphics-capable client holds.
pub struct ClientImages {
    /// Image id → (decoded bytes, last frame that showed it).
    held: HashMap<u32, (usize, Instant)>,
    held_bytes: usize,
    last_runs: Vec<ImageRun>,
    /// Nothing sent yet: the first frame states its runs even when empty.
    fresh: bool,
}

/// The image part of one frame for one client; [`ClientImages::commit`] it once the frame is
/// queued.
pub struct Outgoing {
    pub runs: Option<Vec<ImageRun>>,
    pub uploads: Vec<ImageUpload>,
    pub drops: Vec<u32>,
    admitted: Vec<(u32, usize)>,
    all_runs: Vec<ImageRun>,
    shown: Vec<u32>,
}

impl Outgoing {
    pub fn is_empty(&self) -> bool {
        self.runs.is_none() && self.uploads.is_empty() && self.drops.is_empty()
    }
}

impl Default for ClientImages {
    fn default() -> Self {
        Self {
            held: HashMap::new(),
            held_bytes: 0,
            last_runs: Vec::new(),
            fresh: true,
        }
    }
}

impl ClientImages {
    /// The image part of the next frame: runs whose image fits the budget (images already held
    /// always do), uploads for the newly admitted ones, drops of unshown images past the budget.
    pub fn prepare(&self, runs: &[ScreenRun], ids: &mut ImageIds, budget: usize) -> Outgoing {
        let keyed: Vec<(u32, &ScreenRun)> = runs.iter().map(|r| (ids.id(r.image.key), r)).collect();
        let wanted: HashSet<u32> = keyed.iter().map(|(id, _)| *id).collect();
        // Room is made by forgetting held images nothing shows now, longest unshown first.
        let mut idle: Vec<(u32, usize, Instant)> = self
            .held
            .iter()
            .filter(|(id, _)| !wanted.contains(id))
            .map(|(id, (size, used))| (*id, *size, *used))
            .collect();
        idle.sort_by_key(|(_, _, used)| std::cmp::Reverse(*used));
        let mut bytes = self.held_bytes;
        let (mut admitted, mut uploads, mut drops) = (Vec::new(), Vec::new(), Vec::new());
        let mut refused: HashSet<u32> = HashSet::new();
        for (id, r) in &keyed {
            if self.held.contains_key(id)
                || admitted.iter().any(|(a, _)| a == id)
                || refused.contains(id)
            {
                continue;
            }
            let size = r.image.rgba.len();
            while bytes + size > budget
                && let Some((old, old_size, _)) = idle.pop()
            {
                bytes -= old_size;
                drops.push(old);
            }
            if bytes + size > budget {
                // Bigger than what is left even after forgetting everything unshown: this
                // image is not shown to this client, and no run names it.
                refused.insert(*id);
                continue;
            }
            bytes += size;
            admitted.push((*id, size));
            uploads.push(ids.upload(*id, &r.image));
        }
        let wire: Vec<ImageRun> = keyed
            .iter()
            .filter(|(id, _)| !refused.contains(id))
            .map(|(id, r)| to_wire(*id, r))
            .collect();
        let shown: HashSet<u32> = wire.iter().map(|r| r.id).collect();
        let changed = self.fresh || wire != self.last_runs;
        Outgoing {
            runs: changed.then(|| wire.clone()),
            uploads,
            drops,
            admitted,
            shown: shown.into_iter().collect(),
            all_runs: wire,
        }
    }

    /// The frame carrying `out` reached the client's queue.
    pub fn commit(&mut self, out: Outgoing) {
        let now = Instant::now();
        for id in &out.drops {
            if let Some((size, _)) = self.held.remove(id) {
                self.held_bytes -= size;
            }
        }
        for (id, size) in out.admitted {
            self.held.insert(id, (size, now));
            self.held_bytes += size;
        }
        for id in out.shown {
            if let Some(entry) = self.held.get_mut(&id) {
                entry.1 = now;
            }
        }
        self.last_runs = out.all_runs;
        self.fresh = false;
    }
}

fn to_wire(id: u32, r: &ScreenRun) -> ImageRun {
    let (w, h) = (r.image.width as f32, r.image.height as f32);
    let x0 = (r.uv[0] * w).floor().max(0.0);
    let y0 = (r.uv[1] * h).floor().max(0.0);
    let x1 = (r.uv[2] * w).round().max(x0 + 1.0);
    let y1 = (r.uv[3] * h).round().max(y0 + 1.0);
    let (cx, cy) = (r.rect[0].floor(), r.rect[1].floor());
    ImageRun {
        id,
        x: cx as u16,
        y: cy as u16,
        cols: ((r.rect[2].ceil() - cx) as u16).max(1),
        src: [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32],
        off: [r.rect[0] - cx, r.rect[1] - cy],
        z: r.z,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn image(w: u32, h: u32) -> Arc<GraphicImage> {
        Arc::new(GraphicImage::new(
            w,
            h,
            vec![255; (w * h * 4) as usize],
            Arc::new(AtomicUsize::new(0)),
        ))
    }

    fn run(image: &Arc<GraphicImage>, rect: [f32; 4], uv: [f32; 4]) -> ScreenRun {
        ScreenRun {
            image: image.clone(),
            rect,
            uv,
            z: 0,
        }
    }

    #[test]
    fn runs_become_wire_runs_with_pixel_sources_and_cell_fractions() {
        let img = image(40, 20);
        let r = to_wire(7, &run(&img, [2.5, 3.0, 6.0, 4.0], [0.0, 0.0, 1.0, 1.0]));
        assert_eq!((r.x, r.y, r.cols), (2, 3, 4));
        assert_eq!(r.src, [0, 0, 40, 20]);
        assert_eq!(r.off, [0.5, 0.0]);
    }

    #[test]
    fn panes_clip_their_runs_and_place_them_on_screen() {
        let img = image(40, 20);
        let graphic = copad_graphics::CopadGraphic {
            rect: [1.0, 0.0, 5.0, 1.0],
            uv: [0.0, 0.0, 1.0, 1.0],
            z: 0,
            image: 0,
        };
        let runs = pane_runs((vec![graphic], vec![img]), 10, 2, 3, 4);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].rect,
            [11.0, 2.0, 13.0, 3.0],
            "cut at the pane's third column"
        );
        assert_eq!(runs[0].uv[2], 0.5, "and the source with it");
    }

    #[test]
    fn chrome_over_a_run_splits_it() {
        let img = image(80, 20);
        let r = run(&img, [0.0, 0.0, 8.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 2));
        let before = remember(std::slice::from_ref(&r), &buf);
        buf.cell_mut(Position::new(3, 0)).unwrap().set_symbol("│");
        buf.cell_mut(Position::new(4, 0)).unwrap().set_symbol("x");
        let out = uncovered(vec![r], &before, &buf);
        let spans: Vec<[f32; 2]> = out.iter().map(|r| [r.rect[0], r.rect[2]]).collect();
        assert_eq!(spans, vec![[0.0, 3.0], [5.0, 8.0]]);
        assert_eq!(out[1].uv[0], 5.0 / 8.0);
    }

    #[test]
    fn uploads_happen_once_and_commit_only_when_sent() {
        let (a, b) = (image(10, 10), image(10, 10));
        let runs = vec![run(&a, [0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0])];
        let mut ids = ImageIds::default();
        let mut client = ClientImages::default();
        // A frame that never reached the client: the next one uploads again.
        let lost = client.prepare(&runs, &mut ids, CLIENT_BUDGET);
        assert_eq!(lost.uploads.len(), 1);
        let sent = client.prepare(&runs, &mut ids, CLIENT_BUDGET);
        assert_eq!(sent.uploads.len(), 1);
        client.commit(sent);
        let again = client.prepare(&runs, &mut ids, CLIENT_BUDGET);
        assert!(again.is_empty(), "same runs, image held: nothing to say");
        // Over budget with nothing idle to forget: the new image is refused (not shown), never
        // a run without its data.
        let both = vec![
            runs[0].clone(),
            run(&b, [1.0, 0.0, 2.0, 1.0], [0.0, 0.0, 1.0, 1.0]),
        ];
        let tight = client.prepare(&both, &mut ids, 400 + 399);
        assert!(tight.uploads.is_empty());
        assert!(tight.runs.is_none(), "still just a's run: unchanged");
    }

    #[test]
    fn past_the_budget_unshown_images_are_dropped_oldest_first() {
        let (a, b) = (image(10, 10), image(10, 10));
        let mut ids = ImageIds::default();
        let mut client = ClientImages::default();
        let ra = vec![run(&a, [0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0])];
        let out = client.prepare(&ra, &mut ids, 1000);
        client.commit(out);
        // Now only b shows, and both do not fit: a goes.
        let rb = vec![run(&b, [0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0])];
        let out = client.prepare(&rb, &mut ids, 500);
        assert_eq!(out.uploads.len(), 1);
        assert_eq!(out.drops, vec![ids.id(a.key)]);
    }
}
