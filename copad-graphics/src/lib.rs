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
    /// The cell size images are laid out against (a fallback until the host reports one).
    pub fn cell(&self) -> (u32, u32) {
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

/// A virtual placement (`U=1`): where Unicode placeholder cells (U+10EEEE) find their image.
#[derive(Clone)]
pub struct Virtual {
    pub image: Arc<GraphicImage>,
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
}

/// `(image id, placement id)` -> its virtual placement; shared with the host, which hands it to
/// [`collect`] (placeholders are text in the grid, so they are resolved when drawing).
pub type Virtuals = Arc<std::sync::Mutex<HashMap<(u32, u32), Virtual>>>;

/// The placeholder character of kitty's Unicode placeholders.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// kitty's row/column diacritics (gen/rowcolumn-diacritics.txt): the n-th means n.
const DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

/// What a command did to the terminal besides its images: a host that mirrors the stream into a
/// second terminal (copad-linux's relay feeds VTE) replays these there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// Lines the scroll region was scrolled up before placing (`Handler::scroll_up`).
    pub scrolled: usize,
    /// Where the placement left the cursor, BEFORE `linefeed`.
    pub cursor: Option<Point>,
    /// The placement ended with a linefeed (the cursor ran past the last column).
    pub linefeed: bool,
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
    /// Filled by `put` while a command runs; returned by `handle`.
    effects: Effects,
    /// Bumped by every change to what is drawn (placement, delete, eviction).
    revision: u64,
    virtuals: Virtuals,
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
            effects: Effects::default(),
            revision: 0,
            virtuals: Virtuals::default(),
        }
    }

    /// The virtual placements, for [`collect`].
    pub fn virtuals(&self) -> Virtuals {
        self.virtuals.clone()
    }

    fn drop_virtuals(&mut self, mut matches: impl FnMut(u32) -> bool) {
        let mut v = self.virtuals.lock().unwrap();
        let before = v.len();
        v.retain(|(id, _), _| !matches(*id));
        if v.len() != before {
            self.revision += 1;
        }
    }

    /// Decoded bytes this terminal may hold (default [`QUOTA_BYTES`]); a host with many
    /// terminals (comux) keeps each one smaller.
    pub fn set_quota(&mut self, bytes: usize) {
        self.quota = bytes;
    }

    /// Changes with every placement, delete or eviction: a host polls it to know when to redraw.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Images are stored or still shown somewhere.
    pub fn has_images(&self) -> bool {
        !self.images.is_empty() || self.live_bytes() > 0
    }

    /// Decoded image bytes still alive anywhere (store, cells, snapshots).
    pub fn live_bytes(&self) -> usize {
        self.live_bytes.load(Ordering::Relaxed)
    }

    /// One APC string from the parser; returns what it did to the cursor and scroll region.
    pub fn handle<L: EventListener>(&mut self, term: &mut Term<L>, data: &[u8]) -> Effects {
        self.effects = Effects::default();
        self.dispatch(term, data);
        std::mem::take(&mut self.effects)
    }

    fn dispatch<L: EventListener>(&mut self, term: &mut Term<L>, data: &[u8]) {
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
            self.drop_virtuals(|v| v == id);
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
                // Its virtual placements hold it too: they go with it, or its bytes would stay
                // charged forever.
                self.drop_virtuals(|v| v == id);
                if !unreferenced {
                    self.revision += 1;
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
            self.revision += 1;
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
        let unicode = cmd.unicode;
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

        if unicode {
            // A virtual placement draws nothing itself: placeholder cells in the text show it.
            // Its grid defaults to the image's natural size in cells, as in kitty.
            let (cw, ch) = self.shared.cell();
            let cols = if cmd.cols != 0 {
                cmd.cols
            } else {
                image.width.div_ceil(cw)
            }
            .clamp(1, MAX_SPAN);
            let rows = if cmd.rows != 0 {
                cmd.rows
            } else {
                image.height.div_ceil(ch)
            }
            .clamp(1, MAX_SPAN);
            let mut virtuals = self.virtuals.lock().unwrap();
            if virtuals.len() >= MAX_PLACEMENTS && !virtuals.contains_key(&(id, cmd.placement)) {
                return Err("ENOSPC:too many placements");
            }
            virtuals.insert(
                (id, cmd.placement),
                Virtual {
                    image,
                    cols,
                    rows,
                    z: cmd.z,
                },
            );
            drop(virtuals);
            self.revision += 1;
            return Ok(id);
        }

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
                self.effects.scrolled = scroll as usize;
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
                self.effects.linefeed = true;
            } else {
                grid.cursor.point = Point::new(Line(line), Column(column as usize));
            }
            self.effects.cursor = Some(grid.cursor.point);
            if self.effects.linefeed {
                Handler::linefeed(term);
            }
        }
        self.revision += 1;
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
                if p == 0 {
                    self.drop_virtuals(|v| v == id);
                } else if self.virtuals.lock().unwrap().remove(&(id, p)).is_some() {
                    self.revision += 1;
                }
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
                    self.drop_virtuals(|v| v == id);
                    let p = cmd.placement;
                    self.remove_placements(term, |g| {
                        g.image_id == id && (p == 0 || g.client_placement == p)
                    });
                }
            }
            (None, b'r') => {
                let (lo, hi) = (cmd.x, cmd.y);
                affected.extend(self.images.keys().filter(|&&id| id >= lo && id <= hi));
                self.drop_virtuals(|v| v >= lo && v <= hi);
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

/// Unicode placeholders to resolve in [`collect`]: the terminal's virtual placements and its
/// cell size in pixels.
pub struct Placeholders<'a> {
    pub virtuals: &'a HashMap<(u32, u32), Virtual>,
    pub cell: (u32, u32),
}

/// Runs being built: fragments that continue each other on a row merge into one.
struct Runs {
    runs: Vec<((i32, u32, u64), usize, CopadGraphic)>,
    images: Vec<Arc<GraphicImage>>,
    image_index: HashMap<u64, u32>,
    /// (image, z) -> the run it last extended on this row, and that run's last column.
    /// Keyed by image rather than placement, so fragments that continue each other merge
    /// even when a client placed every cell on its own (yazi does: one 1x1 placement per
    /// cell, cut from one image) — a row of them is one run, not one per cell.
    open: HashMap<(u64, i32), (usize, usize)>,
}

impl Runs {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        image: &Arc<GraphicImage>,
        order: (i32, u32, u64),
        col: usize,
        row: usize,
        uv: [f32; 4],
        dst: [f32; 4],
    ) {
        let (x, y) = (col as f32, row as f32);
        let rect = [x + dst[0], y + dst[1], x + dst[2], y + dst[3]];
        let key = (image.key, order.0);
        if let Some(&(at, last)) = self.open.get(&key) {
            let run = &mut self.runs[at].2;
            if last + 1 == col
                && (rect[0] - run.rect[2]).abs() < EPSILON
                && (rect[1] - run.rect[1]).abs() < EPSILON
                && (rect[3] - run.rect[3]).abs() < EPSILON
                && (uv[0] - run.uv[2]).abs() < EPSILON
                && (uv[1] - run.uv[1]).abs() < EPSILON
                && (uv[3] - run.uv[3]).abs() < EPSILON
            {
                run.rect[2] = rect[2];
                run.uv[2] = uv[2];
                self.open.insert(key, (at, col));
                return;
            }
        }
        let next = self.images.len() as u32;
        let images = &mut self.images;
        let index = *self.image_index.entry(image.key).or_insert_with(|| {
            images.push(image.clone());
            next
        });
        self.runs.push((
            order,
            self.runs.len(),
            CopadGraphic {
                rect,
                uv,
                z: order.0,
                image: index,
            },
        ));
        self.open.insert(key, (self.runs.len() - 1, col));
    }
}

/// A placeholder cell's image id and placement id, from its colours.
fn placeholder_ids(cell: &Cell) -> Option<(u32, u32)> {
    let id = |c: alacritty_terminal::vte::ansi::Color| match c {
        alacritty_terminal::vte::ansi::Color::Spec(rgb) => {
            Some(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32)
        }
        alacritty_terminal::vte::ansi::Color::Indexed(n) => Some(n as u32),
        alacritty_terminal::vte::ansi::Color::Named(_) => None,
    };
    Some((
        id(cell.fg)?,
        cell.underline_color().and_then(id).unwrap_or(0),
    ))
}

/// Where one placeholder cell's part of a virtual placement lands: the image fit into the
/// placement's `cols x rows` box, aspect kept and centred (kitty's rule), cut to the cell.
fn placeholder_slice(
    v: &Virtual,
    row: u32,
    col: u32,
    (cw, ch): (u32, u32),
) -> Option<([f32; 4], [f32; 4])> {
    let (bw, bh) = ((v.cols * cw) as f32, (v.rows * ch) as f32);
    let (iw, ih) = (v.image.width as f32, v.image.height as f32);
    let scale = (bw / iw).min(bh / ih);
    let (dw, dh) = (iw * scale, ih * scale);
    let dest = [
        (bw - dw) / 2.0,
        (bh - dh) / 2.0,
        (bw + dw) / 2.0,
        (bh + dh) / 2.0,
    ];
    let cell = [
        (col * cw) as f32,
        (row * ch) as f32,
        ((col + 1) * cw) as f32,
        ((row + 1) * ch) as f32,
    ];
    let ix = [
        dest[0].max(cell[0]),
        dest[1].max(cell[1]),
        dest[2].min(cell[2]),
        dest[3].min(cell[3]),
    ];
    if ix[0] >= ix[2] || ix[1] >= ix[3] {
        return None;
    }
    let uv = [
        (ix[0] - dest[0]) / dw,
        (ix[1] - dest[1]) / dh,
        (ix[2] - dest[0]) / dw,
        (ix[3] - dest[1]) / dh,
    ];
    let dst = [
        (ix[0] - cell[0]) / cw as f32,
        (ix[1] - cell[1]) / ch as f32,
        (ix[2] - cell[0]) / cw as f32,
        (ix[3] - cell[1]) / ch as f32,
    ];
    Some((uv, dst))
}

/// The visible fragments of `rows` viewport rows (`display_offset` into history), merged into
/// per-row runs and sorted bottom-most first, plus the images they reference. With
/// `placeholders`, Unicode placeholder cells show their virtual placements too.
pub fn collect(
    grid: &Grid<Cell>,
    display_offset: i32,
    rows: usize,
    placeholders: Option<&Placeholders>,
) -> (Vec<CopadGraphic>, Vec<Arc<GraphicImage>>) {
    let mut out = Runs {
        runs: Vec::new(),
        images: Vec::new(),
        image_index: HashMap::new(),
        open: HashMap::new(),
    };
    let columns = grid.columns();
    let (top, bottom) = (-(grid.history_size() as i32), grid.screen_lines() as i32);
    let diacritic = |c: char| DIACRITICS.iter().position(|&d| d == c).map(|i| i as u32);
    for viewport_row in 0..rows {
        let line = Line(viewport_row as i32 - display_offset);
        // A viewport reaching above the retained history (a host scrolled further back than
        // this grid keeps) shows nothing there, at the same rows.
        if line.0 < top || line.0 >= bottom {
            continue;
        }
        out.open.clear();
        // The previous placeholder on this row: (image id low bits, placement, row, col, msb).
        let mut prev: Option<(u32, u32, u32, u32, u32)> = None;
        for col in 0..columns {
            let cell = &grid[line][Column(col)];
            for g in cell.graphics() {
                out.add(&g.image, g.order(), col, viewport_row, g.uv, g.dst);
            }
            let Some(ph) = placeholders.filter(|_| cell.c == PLACEHOLDER) else {
                prev = None;
                continue;
            };
            let Some((low, pid)) = placeholder_ids(cell) else {
                prev = None;
                continue;
            };
            let marks: Vec<u32> = cell
                .zerowidth()
                .unwrap_or(&[])
                .iter()
                .filter_map(|&c| diacritic(c))
                .collect();
            // kitty's inheritance from the placeholder to the left.
            let same = prev.filter(|p| p.0 == low && p.1 == pid);
            let (row, pcol, msb) = match (marks.as_slice(), same) {
                ([], Some(p)) => (p.2, p.3 + 1, p.4),
                ([r], Some(p)) if p.2 == *r => (*r, p.3 + 1, p.4),
                ([r, c], _) => (*r, *c, 0),
                ([r, c, m, ..], _) => (*r, *c, *m),
                _ => {
                    prev = None;
                    continue;
                }
            };
            prev = Some((low, pid, row, pcol, msb));
            let id = (msb << 24) | (low & 0x00FF_FFFF);
            let v = ph.virtuals.get(&(id, pid)).or_else(|| {
                (pid == 0)
                    .then(|| {
                        ph.virtuals
                            .iter()
                            .filter(|((i, _), _)| *i == id)
                            .map(|(_, v)| v)
                            .next()
                    })
                    .flatten()
            });
            let Some(v) = v else { continue };
            if row >= v.rows || pcol >= v.cols {
                continue;
            }
            if let Some((uv, dst)) = placeholder_slice(v, row, pcol, ph.cell) {
                out.add(&v.image, (v.z, id, 0), col, viewport_row, uv, dst);
            }
        }
    }
    out.runs.sort_by_key(|&(order, seq, _)| (order, seq));
    (
        out.runs.into_iter().map(|(_, _, run)| run).collect(),
        out.images,
    )
}

#[cfg(test)]
mod tests;
