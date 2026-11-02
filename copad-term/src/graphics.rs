//! The kitty graphics protocol (decisions #189): what yazi previews and `kitten icat` draw with.
//!
//! Images live IN the grid: every cell an image covers carries a [`GraphicCell`] fragment (the
//! vendored alacritty_terminal patch), so images scroll with text, move into scrollback, stay on
//! their own screen buffer and go away when their cells are erased — with no position bookkeeping
//! here. The parser hands us each complete APC string in stream order (the vendored vte patch), so
//! a placement inside a synchronized update still lands where the cursor is at that point.
//!
//! Deliberate limits: direct (`t=d`) transmission only — no file, temp-file or shared-memory reads,
//! so a program can never make the terminal open a path; no animation; no Unicode placeholders.
//! Writing text over an image cell removes that cell's fragment (kitty keeps images independent of
//! text).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, GraphicCell, GraphicImage};
use alacritty_terminal::vte::ansi::Handler;
use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

/// Decoded bytes a terminal may hold across store, cells, scrollback and snapshots (kitty's own
/// default storage limit).
pub const QUOTA_BYTES: usize = 320 << 20;
/// Largest single decoded image.
pub const MAX_IMAGE_BYTES: usize = 100 << 20;
/// Largest width or height.
pub const MAX_DIMENSION: u32 = 10_000;
/// Fragments one cell keeps; a further one pushes out the lowest-stacked.
pub const MAX_PER_CELL: usize = 4;
/// Live placements a terminal tracks before it refuses new ones.
pub const MAX_PLACEMENTS: usize = 16_384;

/// Used to size placements while the host has not reported its cell size yet.
const FALLBACK_CELL: (u32, u32) = (10, 20);
/// Largest cell size believed, in pixels.
const MAX_CELL: u32 = 4096;
/// Most columns or rows one placement may ask for (more than any screen; keeps every product
/// below in range).
const MAX_SPAN: u32 = 10_000;

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// What the host changes from outside the PTY thread.
#[derive(Default)]
pub struct Shared {
    /// `[terminal] images`. Off = every graphics command is swallowed without a reply, so programs
    /// see no support and fall back to text.
    pub enabled: AtomicBool,
    /// Cell size in device pixels; 0 until the host reports it.
    pub cell_width: AtomicU32,
    pub cell_height: AtomicU32,
}

impl Shared {
    fn cell(&self) -> (u32, u32) {
        let (w, h) = (
            self.cell_width.load(Ordering::Relaxed),
            self.cell_height.load(Ordering::Relaxed),
        );
        if w == 0 || h == 0 {
            FALLBACK_CELL
        } else {
            (w.min(MAX_CELL), h.min(MAX_CELL))
        }
    }
}

struct Stored {
    image: Arc<GraphicImage>,
    /// The `I=` image number it was sent with, 0 if none.
    number: u32,
    /// Last use, for eviction order.
    used: u64,
}

struct Pending {
    cmd: Cmd,
    data: Vec<u8>,
    cap: usize,
    error: Option<&'static str>,
}

pub struct Graphics {
    shared: Arc<Shared>,
    reply: Box<dyn FnMut(String) + Send>,
    live_bytes: Arc<AtomicUsize>,
    quota: usize,
    images: HashMap<u32, Stored>,
    clock: u64,
    next_auto_id: u32,
    next_placement: u64,
    /// Placements believed alive → their image id. Stale entries (cells overwritten, scrolled out of
    /// history) are pruned by a scan when the cap is hit.
    live: HashMap<u64, u32>,
    /// `(image id, client placement id)` → internal placement, for the ones the client named.
    named: HashMap<(u32, u32), u64>,
    pending: Option<Pending>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Cmd {
    action: u8,
    quiet: u8,
    medium: u8,
    format: u32,
    compressed: bool,
    id: u32,
    number: u32,
    placement: u32,
    width: u32,
    height: u32,
    more: bool,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    off_x: u32,
    off_y: u32,
    cols: u32,
    rows: u32,
    no_move: bool,
    unicode: bool,
    z: i32,
    delete: u8,
    /// Keys other than `m` and `q` — none means "the next chunk of the pending transmission".
    keys: usize,
}

fn parse(control: &[u8]) -> Result<Cmd, &'static str> {
    let mut cmd = Cmd {
        action: b't',
        medium: b'd',
        format: 32,
        delete: b'a',
        ..Cmd::default()
    };
    for pair in control.split(|&b| b == b',').filter(|p| !p.is_empty()) {
        let [key, b'=', value @ ..] = pair else {
            return Err("EINVAL:malformed key");
        };
        let num = || {
            std::str::from_utf8(value)
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
        };
        let char_value = || match value {
            [c] => Ok(*c),
            _ => Err("EINVAL:malformed value"),
        };
        let bad = "EINVAL:malformed value";
        if !matches!(key, b'm' | b'q') {
            cmd.keys += 1;
        }
        match key {
            b'a' => cmd.action = char_value()?,
            b't' => cmd.medium = char_value()?,
            b'o' => cmd.compressed = char_value()? == b'z',
            b'd' => cmd.delete = char_value()?,
            b'z' => {
                cmd.z = std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .ok_or(bad)?
            }
            _ => {
                let n = num().ok_or(bad)?;
                match key {
                    b'q' => cmd.quiet = n.min(2) as u8,
                    b'f' => cmd.format = n,
                    b'i' => cmd.id = n,
                    b'I' => cmd.number = n,
                    b'p' => cmd.placement = n,
                    b's' => cmd.width = n,
                    b'v' => cmd.height = n,
                    b'm' => cmd.more = n == 1,
                    b'x' => cmd.x = n,
                    b'y' => cmd.y = n,
                    b'w' => cmd.w = n,
                    b'h' => cmd.h = n,
                    b'X' => cmd.off_x = n,
                    b'Y' => cmd.off_y = n,
                    b'c' => cmd.cols = n.min(MAX_SPAN),
                    b'r' => cmd.rows = n.min(MAX_SPAN),
                    b'C' => cmd.no_move = n == 1,
                    b'U' => cmd.unicode = n == 1,
                    // Keys this implementation has no use for (S, O, P, Q, H, V): accepted, ignored.
                    _ => {}
                }
            }
        }
    }
    Ok(cmd)
}

/// Bytes per pixel of a raw format, `None` for PNG.
fn raw_bpp(format: u32) -> Result<Option<usize>, &'static str> {
    match format {
        24 => Ok(Some(3)),
        32 => Ok(Some(4)),
        100 => Ok(None),
        _ => Err("EINVAL:unknown format"),
    }
}

fn check_dimensions(width: u32, height: u32) -> Result<usize, &'static str> {
    if width == 0 || height == 0 {
        return Err("EINVAL:image has no size");
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err("EFBIG:image too large");
    }
    let bytes = width as usize * height as usize * 4;
    if bytes > MAX_IMAGE_BYTES {
        return Err("EFBIG:image too large");
    }
    Ok(bytes)
}

/// The most transmitted (pre-decompression) bytes a command may accumulate.
fn transmission_cap(cmd: &Cmd) -> Result<usize, &'static str> {
    match raw_bpp(cmd.format)? {
        Some(bpp) if !cmd.compressed => {
            check_dimensions(cmd.width, cmd.height)?;
            Ok(cmd.width as usize * cmd.height as usize * bpp)
        }
        _ => Ok(MAX_IMAGE_BYTES),
    }
}

/// Decode a finished transmission to premultiplied RGBA8.
fn decode(cmd: &Cmd, data: Vec<u8>) -> Result<(u32, u32, Vec<u8>), &'static str> {
    let bpp = raw_bpp(cmd.format)?;
    let data = if cmd.compressed {
        let limit = match bpp {
            Some(bpp) => {
                check_dimensions(cmd.width, cmd.height)?;
                cmd.width as usize * cmd.height as usize * bpp
            }
            None => MAX_IMAGE_BYTES,
        };
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(&data[..])
            .take(limit as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|_| "EINVAL:bad zlib data")?;
        if out.len() > limit {
            return Err("EFBIG:decompressed data too large");
        }
        out
    } else {
        data
    };
    let (width, height, mut rgba) = match bpp {
        Some(bpp) => {
            check_dimensions(cmd.width, cmd.height)?;
            let pixels = cmd.width as usize * cmd.height as usize;
            if data.len() != pixels * bpp {
                return Err("ENODATA:pixel data does not match the size");
            }
            let rgba = if bpp == 4 {
                data
            } else {
                let mut out = Vec::with_capacity(pixels * 4);
                for px in data.chunks_exact(3) {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
                out
            };
            (cmd.width, cmd.height, rgba)
        }
        None => decode_png(&data)?,
    };
    premultiply(&mut rgba);
    Ok((width, height, rgba))
}

fn decode_png(data: &[u8]) -> Result<(u32, u32, Vec<u8>), &'static str> {
    let bad = "EBADPNG:cannot decode PNG";
    let mut decoder = png::Decoder::new_with_limits(
        data,
        png::Limits {
            bytes: MAX_IMAGE_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|_| bad)?;
    let info = reader.info();
    check_dimensions(info.width, info.height)?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(|_| bad)?;
    buf.truncate(frame.buffer_size());
    let pixels = frame.width as usize * frame.height as usize;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err(bad),
    };
    if rgba.len() != pixels * 4 {
        return Err(bad);
    }
    Ok((frame.width, frame.height, rgba))
}

fn premultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u16;
        if a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u16 * a + 127) / 255) as u8;
            }
        }
    }
}

/// Every line of a grid, history included.
fn all_lines<T>(grid: &Grid<T>) -> std::ops::Range<i32> {
    -(grid.history_size() as i32)..grid.screen_lines() as i32
}

/// Remove matching fragments everywhere in `grid`; returns the placements touched.
fn strip(grid: &mut Grid<Cell>, mut matches: impl FnMut(&GraphicCell) -> bool) -> HashSet<u64> {
    let mut touched = HashSet::new();
    let columns = grid.columns();
    for line in all_lines(grid) {
        let row = &mut grid[Line(line)];
        for col in 0..columns {
            let cell = &mut row[Column(col)];
            if cell.graphics().is_empty() {
                continue;
            }
            cell.remove_graphics(|g| {
                let hit = matches(g);
                if hit {
                    touched.insert(g.placement);
                }
                hit
            });
        }
    }
    touched
}

/// Whether any cell of either screen buffer still shows image `key`.
fn shows<L>(term: &mut Term<L>, key: u64) -> bool {
    term.grids_mut().into_iter().any(|grid| {
        all_lines(grid).any(|line| {
            (0..grid.columns()).any(|col| {
                grid[Line(line)][Column(col)]
                    .graphics()
                    .iter()
                    .any(|g| g.image.key == key)
            })
        })
    })
}

/// Placements with a fragment on the visible screen that `matches` (cell point, fragment).
fn visible_placements<L>(
    term: &Term<L>,
    mut matches: impl FnMut(Point, &GraphicCell) -> bool,
) -> HashSet<u64> {
    let grid = term.grid();
    let mut found = HashSet::new();
    for line in 0..grid.screen_lines() as i32 {
        for col in 0..grid.columns() {
            let point = Point::new(Line(line), Column(col));
            for g in grid[point].graphics() {
                if matches(point, g) {
                    found.insert(g.placement);
                }
            }
        }
    }
    found
}

impl Graphics {
    pub fn new(shared: Arc<Shared>, reply: Box<dyn FnMut(String) + Send>) -> Self {
        Self {
            shared,
            reply,
            live_bytes: Arc::new(AtomicUsize::new(0)),
            quota: QUOTA_BYTES,
            images: HashMap::new(),
            clock: 0,
            next_auto_id: 1 << 31,
            next_placement: 1,
            live: HashMap::new(),
            named: HashMap::new(),
            pending: None,
        }
    }

    /// Decoded image bytes still alive anywhere (store, cells, snapshots).
    pub fn live_bytes(&self) -> usize {
        self.live_bytes.load(Ordering::Relaxed)
    }

    /// One APC string from the parser.
    pub fn handle<L: EventListener>(&mut self, term: &mut Term<L>, data: &[u8]) {
        let Some(body) = data.strip_prefix(b"G") else {
            return;
        };
        if !self.shared.enabled.load(Ordering::Relaxed) {
            self.pending = None;
            return;
        }
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &[][..]),
        };
        let cmd = match parse(control) {
            Ok(cmd) => cmd,
            // Nothing reliable to address a reply with.
            Err(_) => {
                self.pending = None;
                return;
            }
        };
        // A command with keys of its own abandons an unfinished transmission, as in kitty.
        if let Some(mut pending) = self.pending.take()
            && cmd.keys == 0
        {
            pending.cmd.more = cmd.more;
            if cmd.quiet != 0 {
                pending.cmd.quiet = cmd.quiet;
            }
            self.chunk(term, pending, payload);
            return;
        }
        match cmd.action {
            b't' | b'T' | b'q' => {
                if cmd.medium != b'd' {
                    return self.respond(&cmd, Err("ENOTSUPPORTED:only direct transmission (t=d)"));
                }
                if cmd.unicode {
                    return self.respond(&cmd, Err("ENOTSUPPORTED:Unicode placeholders"));
                }
                let pending = match transmission_cap(&cmd) {
                    Ok(cap) => Pending {
                        cmd,
                        data: Vec::new(),
                        cap,
                        error: None,
                    },
                    Err(e) => Pending {
                        cmd,
                        data: Vec::new(),
                        cap: 0,
                        error: Some(e),
                    },
                };
                self.chunk(term, pending, payload);
            }
            b'p' => {
                let result = self.put(term, &cmd);
                self.respond(&cmd, result);
            }
            b'd' => self.delete(term, &cmd),
            b'f' | b'a' | b'c' => self.respond(&cmd, Err("ENOTSUPPORTED:animation")),
            _ => self.respond(&cmd, Err("EINVAL:unknown action")),
        }
    }

    fn chunk<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        mut pending: Pending,
        payload: &[u8],
    ) {
        if pending.error.is_none() {
            match BASE64.decode(payload) {
                Ok(bytes) if pending.data.len() + bytes.len() <= pending.cap => {
                    pending.data.extend_from_slice(&bytes)
                }
                Ok(_) => pending.error = Some("EFBIG:transmission too large"),
                Err(_) => pending.error = Some("EINVAL:bad base64"),
            }
            if pending.error.is_some() {
                // Keep consuming the remaining chunks, but stop holding the bytes.
                pending.data = Vec::new();
            }
        }
        if pending.cmd.more {
            self.pending = Some(pending);
            return;
        }
        let Pending {
            cmd, data, error, ..
        } = pending;
        let result = match error {
            Some(e) => Err(e),
            None => self.finish(term, &cmd, data),
        };
        self.respond(&cmd, result);
    }

    fn finish<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        cmd: &Cmd,
        data: Vec<u8>,
    ) -> Result<u32, &'static str> {
        let (width, height, rgba) = decode(cmd, data)?;
        if cmd.action == b'q' {
            return Ok(cmd.id);
        }
        self.make_room(term, rgba.len())?;
        let id = match (cmd.id, cmd.number) {
            (0, 0) if cmd.action == b't' => return Err("EINVAL:no image id"),
            (0, 0) => 0,
            (0, _) => self.auto_id(),
            (id, _) => id,
        };
        let image = Arc::new(GraphicImage::new(
            width,
            height,
            rgba,
            self.live_bytes.clone(),
        ));
        if self.images.contains_key(&id) {
            // Retransmitting an id replaces the image and drops what showed the old one.
            self.remove_placements(term, |g| g.image_id == id);
        }
        self.clock += 1;
        self.images.insert(
            id,
            Stored {
                image,
                number: cmd.number,
                used: self.clock,
            },
        );
        if cmd.action == b'T' {
            let mut place = cmd.clone();
            place.id = id;
            self.put(term, &place)?;
            if id == 0 {
                // An anonymous image lives only as long as its placement.
                self.images.remove(&0);
            }
        }
        Ok(id)
    }

    fn auto_id(&mut self) -> u32 {
        loop {
            let id = self.next_auto_id;
            self.next_auto_id = self.next_auto_id.checked_add(1).unwrap_or(1 << 31);
            if !self.images.contains_key(&id) {
                return id;
            }
        }
    }

    /// Free enough of the quota for `needed` more bytes, or say why not.
    fn make_room<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        needed: usize,
    ) -> Result<(), &'static str> {
        if self.live_bytes() + needed <= self.quota {
            return Ok(());
        }
        let mut by_age: Vec<(u64, u32)> = self.images.iter().map(|(&id, s)| (s.used, id)).collect();
        by_age.sort_unstable();
        // Images nothing shows any more go first, then the oldest still on screen or in history.
        for only_unreferenced in [true, false] {
            for &(_, id) in &by_age {
                if self.live_bytes() + needed <= self.quota {
                    return Ok(());
                }
                let Some(stored) = self.images.get(&id) else {
                    continue;
                };
                let unreferenced = Arc::strong_count(&stored.image) == 1;
                if only_unreferenced != unreferenced {
                    continue;
                }
                let key = stored.image.key;
                self.images.remove(&id);
                if !unreferenced {
                    for grid in term.grids_mut() {
                        for p in strip(grid, |g| g.image.key == key) {
                            self.forget(p);
                        }
                    }
                    term.mark_fully_damaged();
                }
            }
        }
        if self.live_bytes() + needed <= self.quota {
            Ok(())
        } else {
            Err("ENOMEM:image storage quota exceeded")
        }
    }

    fn forget(&mut self, placement: u64) {
        if self.live.remove(&placement).is_some() {
            self.named.retain(|_, p| *p != placement);
        }
    }

    /// Remove every fragment `matches` in the current grid (history included).
    fn remove_placements<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        matches: impl FnMut(&GraphicCell) -> bool,
    ) {
        let touched = strip(term.grid_mut(), matches);
        if !touched.is_empty() {
            term.mark_fully_damaged();
        }
        for p in touched {
            self.forget(p);
        }
    }

    /// Rebuild the live set from what the grids actually hold.
    fn prune_live<L: EventListener>(&mut self, term: &mut Term<L>) {
        let mut seen = HashSet::new();
        for grid in term.grids_mut() {
            for line in all_lines(grid) {
                for col in 0..grid.columns() {
                    for g in grid[Line(line)][Column(col)].graphics() {
                        seen.insert(g.placement);
                    }
                }
            }
        }
        self.live.retain(|p, _| seen.contains(p));
        self.named.retain(|_, p| seen.contains(p));
    }

    /// Display an already transmitted image at the cursor (`a=p`, and the place half of `a=T`).
    fn put<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        cmd: &Cmd,
    ) -> Result<u32, &'static str> {
        if cmd.unicode {
            return Err("ENOTSUPPORTED:Unicode placeholders");
        }
        let id = if cmd.id == 0 && cmd.number != 0 {
            self.images
                .iter()
                .filter(|(_, s)| s.number == cmd.number)
                .max_by_key(|(_, s)| s.used)
                .map(|(&id, _)| id)
                .ok_or("ENOENT:no image with that number")?
        } else {
            cmd.id
        };
        self.clock += 1;
        let clock = self.clock;
        let stored = self.images.get_mut(&id).ok_or("ENOENT:no such image")?;
        stored.used = clock;
        let image = stored.image.clone();

        if cmd.placement != 0
            && let Some(&old) = self.named.get(&(id, cmd.placement))
        {
            self.remove_placements(term, |g| g.placement == old);
        }
        if self.live.len() >= MAX_PLACEMENTS {
            self.prune_live(term);
            if self.live.len() >= MAX_PLACEMENTS {
                return Err("ENOSPC:too many placements");
            }
        }

        let (cw, ch) = self.shared.cell();
        let (iw, ih) = (image.width, image.height);
        let sx = cmd.x.min(iw);
        let sy = cmd.y.min(ih);
        let sw = if cmd.w == 0 {
            iw - sx
        } else {
            cmd.w.min(iw - sx)
        };
        let sh = if cmd.h == 0 {
            ih - sy
        } else {
            cmd.h.min(ih - sy)
        };
        if sw == 0 || sh == 0 {
            return Err("EINVAL:empty source rectangle");
        }
        let off_x = cmd.off_x.min(cw - 1);
        let off_y = cmd.off_y.min(ch - 1);
        let (cols, rows) = cell_span(cmd, (sw, sh), (off_x, off_y), (cw, ch));
        let dest = dest_rect(cmd, (sw, sh), (off_x, off_y), (cw, ch));
        let uv_box = [
            sx as f32 / iw as f32,
            sy as f32 / ih as f32,
            (sx + sw) as f32 / iw as f32,
            (sy + sh) as f32 / ih as f32,
        ];

        let placement = self.next_placement;
        self.next_placement += 1;

        let screen = term.screen_lines() as i32;
        let columns = term.columns() as i32;
        let region = term.scroll_region();
        let (top, bottom) = (region.start.0, region.end.0 - 1);
        let cursor = term.grid().cursor.point;
        let (mut r0, c0) = (cursor.line.0, cursor.column.0 as i32);
        let in_region = r0 >= top && r0 <= bottom;
        let last_line = if in_region { bottom } else { screen - 1 };
        let rows = rows as i32;
        if !cmd.no_move && in_region && r0 + rows - 1 > bottom {
            let scroll = (r0 + rows - 1 - bottom).min(r0 - top);
            if scroll > 0 {
                Handler::scroll_up(term, scroll as usize);
                r0 -= scroll;
            }
        }

        let grid = term.grid_mut();
        for row in 0..rows {
            let line = r0 + row;
            if line > last_line {
                break;
            }
            for col in 0..cols as i32 {
                let column = c0 + col;
                if column >= columns {
                    break;
                }
                let cell = [
                    (col as u32 * cw) as f32,
                    (row as u32 * ch) as f32,
                    ((col as u32 + 1) * cw) as f32,
                    ((row as u32 + 1) * ch) as f32,
                ];
                let ix = [
                    dest[0].max(cell[0]),
                    dest[1].max(cell[1]),
                    dest[2].min(cell[2]),
                    dest[3].min(cell[3]),
                ];
                if ix[0] >= ix[2] || ix[1] >= ix[3] {
                    continue;
                }
                let (dw, dh) = (dest[2] - dest[0], dest[3] - dest[1]);
                let (uw, uh) = (uv_box[2] - uv_box[0], uv_box[3] - uv_box[1]);
                let uv = [
                    uv_box[0] + (ix[0] - dest[0]) / dw * uw,
                    uv_box[1] + (ix[1] - dest[1]) / dh * uh,
                    uv_box[0] + (ix[2] - dest[0]) / dw * uw,
                    uv_box[1] + (ix[3] - dest[1]) / dh * uh,
                ];
                let dst = [
                    (ix[0] - cell[0]) / cw as f32,
                    (ix[1] - cell[1]) / ch as f32,
                    (ix[2] - cell[0]) / cw as f32,
                    (ix[3] - cell[1]) / ch as f32,
                ];
                grid[Line(line)][Column(column as usize)].add_graphic(
                    GraphicCell {
                        image: image.clone(),
                        image_id: id,
                        placement,
                        client_placement: cmd.placement,
                        z: cmd.z,
                        uv,
                        dst,
                    },
                    MAX_PER_CELL,
                );
            }
        }
        self.live.insert(placement, id);
        if cmd.placement != 0 {
            self.named.insert((id, cmd.placement), placement);
        }

        if !cmd.no_move {
            let line = (r0 + rows - 1).min(last_line).max(0);
            let column = c0 + cols as i32;
            let grid = term.grid_mut();
            grid.cursor.input_needs_wrap = false;
            if column >= columns {
                grid.cursor.point = Point::new(Line(line), Column(0));
                Handler::linefeed(term);
            } else {
                grid.cursor.point = Point::new(Line(line), Column(column as usize));
            }
        }
        term.mark_fully_damaged();
        Ok(id)
    }

    fn delete<L: EventListener>(&mut self, term: &mut Term<L>, cmd: &Cmd) {
        let free = cmd.delete.is_ascii_uppercase();
        let (cx, cy) = (cmd.x as usize, cmd.y as i32);
        let cursor = term.grid().cursor.point;
        let mut affected: HashSet<u32> = HashSet::new();
        // Selectors on what is visible pick placements on the screen and remove them whole (also
        // the part already scrolled into history); id selectors reach every placement.
        let visible = match cmd.delete.to_ascii_lowercase() {
            b'a' => Some(visible_placements(term, |_, _| true)),
            b'c' => Some(visible_placements(term, |p, _| p == cursor)),
            b'p' => Some(visible_placements(term, |p, _| {
                p.column.0 + 1 == cx && p.line.0 + 1 == cy
            })),
            b'q' => Some(visible_placements(term, |p, g| {
                p.column.0 + 1 == cx && p.line.0 + 1 == cy && g.z == cmd.z
            })),
            b'x' => Some(visible_placements(term, |p, _| p.column.0 + 1 == cx)),
            b'y' => Some(visible_placements(term, |p, _| p.line.0 + 1 == cy)),
            b'z' => Some(visible_placements(term, |_, g| g.z == cmd.z)),
            _ => None,
        };
        match (visible, cmd.delete.to_ascii_lowercase()) {
            (Some(placements), _) => {
                if !placements.is_empty() {
                    self.remove_placements(term, |g| {
                        let hit = placements.contains(&g.placement);
                        if hit {
                            affected.insert(g.image_id);
                        }
                        hit
                    });
                }
            }
            (None, b'i') => {
                affected.insert(cmd.id);
                let (id, p) = (cmd.id, cmd.placement);
                self.remove_placements(term, |g| {
                    g.image_id == id && (p == 0 || g.client_placement == p)
                });
            }
            (None, b'n') => {
                let newest = self
                    .images
                    .iter()
                    .filter(|(_, s)| s.number == cmd.number)
                    .max_by_key(|(_, s)| s.used)
                    .map(|(&id, _)| id);
                if let Some(id) = newest {
                    affected.insert(id);
                    let p = cmd.placement;
                    self.remove_placements(term, |g| {
                        g.image_id == id && (p == 0 || g.client_placement == p)
                    });
                }
            }
            (None, b'r') => {
                let (lo, hi) = (cmd.x, cmd.y);
                affected.extend(self.images.keys().filter(|&&id| id >= lo && id <= hi));
                self.remove_placements(term, |g| g.image_id >= lo && g.image_id <= hi);
            }
            // Animation frames (f/F) and anything unknown: nothing to delete.
            _ => {}
        }
        if free {
            for id in affected {
                // Judged by the grids, not the Arc count: a snapshot the renderer still holds
                // must not keep a deleted image addressable.
                if let Some(key) = self.images.get(&id).map(|s| s.image.key)
                    && !shows(term, key)
                {
                    self.images.remove(&id);
                }
            }
        }
    }

    /// Answer a command — only when it named an image (kitty's rule), and as `q=` allows.
    fn respond(&mut self, cmd: &Cmd, result: Result<u32, &'static str>) {
        if cmd.id == 0 && cmd.number == 0 {
            return;
        }
        let message = match result {
            Ok(_) if cmd.quiet >= 1 => return,
            Ok(_) => "OK",
            Err(_) if cmd.quiet >= 2 => return,
            Err(e) => e,
        };
        let id = match result {
            Ok(id) if id != 0 => id,
            _ => cmd.id,
        };
        let mut keys = Vec::new();
        if id != 0 {
            keys.push(format!("i={id}"));
        }
        if cmd.number != 0 {
            keys.push(format!("I={}", cmd.number));
        }
        if cmd.placement != 0 {
            keys.push(format!("p={}", cmd.placement));
        }
        (self.reply)(format!("\x1b_G{};{message}\x1b\\", keys.join(",")));
    }
}

/// Cells a placement covers, exactly as kitty's `update_dest_rect` counts them (this is also how
/// far the cursor moves).
fn cell_span(
    cmd: &Cmd,
    (sw, sh): (u32, u32),
    (ox, oy): (u32, u32),
    (cw, ch): (u32, u32),
) -> (u32, u32) {
    let (mut cols, mut rows) = (cmd.cols, cmd.rows);
    if cmd.cols == 0 {
        cols = if cmd.rows == 0 {
            (sw + ox).div_ceil(cw)
        } else {
            let height = (ch * cmd.rows + oy) as f64;
            (height * sw as f64 / sh as f64 / cw as f64).ceil() as u32
        };
    }
    if cmd.rows == 0 {
        rows = if cmd.cols == 0 {
            (sh + oy).div_ceil(ch)
        } else {
            let width = (cw * cmd.cols + ox) as f64;
            (width * sh as f64 / sw as f64 / ch as f64).ceil() as u32
        };
    }
    (cols.clamp(1, MAX_SPAN), rows.clamp(1, MAX_SPAN))
}

/// Where the image is drawn, in pixels from the top-left of the cursor cell — kitty's renderer:
/// given `c`/`r` stretch to that cell boundary, a missing one follows the aspect ratio.
fn dest_rect(
    cmd: &Cmd,
    (sw, sh): (u32, u32),
    (ox, oy): (u32, u32),
    (cw, ch): (u32, u32),
) -> [f32; 4] {
    let (left, top) = (ox as f32, oy as f32);
    let aspect = sw as f32 / sh as f32;
    let (right, bottom) = if cmd.rows != 0 {
        let bottom = (cmd.rows * ch) as f32;
        let right = if cmd.cols != 0 {
            (cmd.cols * cw) as f32
        } else {
            left + (bottom - top) * aspect
        };
        (right, bottom)
    } else {
        let right = if cmd.cols != 0 {
            (cmd.cols * cw) as f32
        } else {
            left + sw as f32
        };
        (right, top + (right - left) / aspect)
    };
    [left, top, right, bottom]
}

/// One image run on one viewport row, in draw order (`#[repr(C)]`: handed to the renderer).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CopadGraphic {
    /// `[left, top, right, bottom]` in cells from the viewport's top-left.
    pub rect: [f32; 4],
    /// `[left, top, right, bottom]` in the image, 0..=1.
    pub uv: [f32; 4],
    pub z: i32,
    /// Index into the snapshot's images.
    pub image: u32,
}

const EPSILON: f32 = 1e-4;

/// The visible fragments of `rows` viewport rows (`display_offset` into history), merged into
/// per-row runs and sorted bottom-most first, plus the images they reference.
pub fn collect(
    grid: &Grid<Cell>,
    display_offset: i32,
    rows: usize,
) -> (Vec<CopadGraphic>, Vec<Arc<GraphicImage>>) {
    // (order, sequence) keys keep runs of equal stacking in reading order.
    let mut runs: Vec<((i32, u32, u64), usize, CopadGraphic)> = Vec::new();
    let mut images: Vec<Arc<GraphicImage>> = Vec::new();
    let mut image_index: HashMap<u64, u32> = HashMap::new();
    let columns = grid.columns();
    for viewport_row in 0..rows {
        let line = Line(viewport_row as i32 - display_offset);
        // Placement -> the run it last extended on this row, and that run's last column.
        let mut open: HashMap<u64, (usize, usize)> = HashMap::new();
        for col in 0..columns {
            for g in grid[line][Column(col)].graphics() {
                let (x, y) = (col as f32, viewport_row as f32);
                let rect = [x + g.dst[0], y + g.dst[1], x + g.dst[2], y + g.dst[3]];
                if let Some(&(at, last)) = open.get(&g.placement) {
                    let run = &mut runs[at].2;
                    if last + 1 == col
                        && (rect[0] - run.rect[2]).abs() < EPSILON
                        && (rect[1] - run.rect[1]).abs() < EPSILON
                        && (rect[3] - run.rect[3]).abs() < EPSILON
                        && (g.uv[0] - run.uv[2]).abs() < EPSILON
                        && (g.uv[1] - run.uv[1]).abs() < EPSILON
                        && (g.uv[3] - run.uv[3]).abs() < EPSILON
                    {
                        run.rect[2] = rect[2];
                        run.uv[2] = g.uv[2];
                        open.insert(g.placement, (at, col));
                        continue;
                    }
                }
                let next = images.len() as u32;
                let image = *image_index.entry(g.image.key).or_insert_with(|| {
                    images.push(g.image.clone());
                    next
                });
                runs.push((
                    g.order(),
                    runs.len(),
                    CopadGraphic {
                        rect,
                        uv: g.uv,
                        z: g.z,
                        image,
                    },
                ));
                open.insert(g.placement, (runs.len() - 1, col));
            }
        }
    }
    runs.sort_by_key(|&(order, seq, _)| (order, seq));
    (runs.into_iter().map(|(_, _, run)| run).collect(), images)
}

#[cfg(test)]
#[path = "graphics_tests.rs"]
mod tests;
