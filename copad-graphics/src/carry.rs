//! A terminal's whole kitty graphics state as bytes, for a host that hands a terminal to another
//! process (AIDE's terminal server replacing itself, aide-mux's upgrade handoff): the store (every
//! image by id with its number and use order), the image fragments every cell holds on both
//! screens and in scrollback, the virtual placements, the placement bookkeeping, the counters, the
//! quota and a chunked transmission in progress. [`Graphics::import`] into a fresh `Graphics` on a
//! terminal whose cells are already the original's (the host replays the text first) makes one
//! that draws the same and answers every later command the same.
//!
//! The format is versioned ([`CARRY_VERSION`]) and only read by the version that wrote it; a
//! reader refuses any other. It is bounded as the store is: no image beyond [`MAX_DIMENSION`] or
//! [`MAX_IMAGE_BYTES`], at most [`MAX_PER_CELL`] fragments a cell and [`MAX_PLACEMENTS`]
//! placements, and no count or length beyond the bytes there are, so a bad file can't allocate
//! wildly: every pixel read is a pixel in the file. The images aren't held to the carried quota (a
//! lowered `set_quota` keeps what is stored, so a real state can be over it); the next
//! transmission makes room as it would have.
//!
//! Pixels go as the store holds them (decoded RGBA, premultiplied): the encoded original isn't
//! kept, and compressing would lengthen the host's pause.

use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{GraphicCell, GraphicImage};

use super::{
    BAD_BASE64, Cmd, Graphics, MAX_DIMENSION, MAX_IMAGE_BYTES, MAX_PER_CELL, MAX_PLACEMENTS,
    MAX_SPAN, Pending, Stored, TOO_LARGE, Virtual, all_lines, transmission_cap,
};

/// The format [`Graphics::export`] writes and [`Graphics::import`] reads.
pub const CARRY_VERSION: u16 = 1;

const MAGIC: &[u8; 4] = b"CPGS";

/// The most a carried counter may be.
const COUNTER_LIMIT: u64 = 1 << 62;

impl Graphics {
    /// The graphics state of `term` (which must be the terminal this one handles) as bytes.
    pub fn export<L: EventListener>(&self, term: &mut Term<L>) -> Vec<u8> {
        let mut out = Out::default();
        out.0.extend_from_slice(MAGIC);
        out.u16(CARRY_VERSION);
        out.u64(self.quota as u64);
        out.u64(self.clock);
        out.u32(self.next_auto_id);
        out.u64(self.next_placement);
        out.u64(self.revision);

        // Every image once, by the cells', the store's and the virtual placements' shared handle.
        let mut table: Vec<Arc<GraphicImage>> = Vec::new();
        let mut index: HashMap<u64, u32> = HashMap::new();
        let mut index_of = |image: &Arc<GraphicImage>| {
            *index.entry(image.key).or_insert_with(|| {
                table.push(image.clone());
                (table.len() - 1) as u32
            })
        };
        let mut store: Vec<(u32, &Stored)> = self.images.iter().map(|(&id, s)| (id, s)).collect();
        store.sort_unstable_by_key(|(id, _)| *id);
        let store: Vec<(u32, u32, u64, u32)> = store
            .into_iter()
            .map(|(id, s)| (id, s.number, s.used, index_of(&s.image)))
            .collect();
        let mut virtuals: Vec<((u32, u32), Virtual)> = self
            .virtuals
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        virtuals.sort_unstable_by_key(|(k, _)| *k);
        let virtuals: Vec<((u32, u32), u32, &Virtual)> = virtuals
            .iter()
            .map(|(k, v)| (*k, index_of(&v.image), v))
            .collect();
        let mut cells = Vec::new();
        for (screen, grid) in term.grids_mut().into_iter().enumerate() {
            for line in all_lines(grid) {
                for col in 0..grid.columns() {
                    let graphics = grid[Line(line)][Column(col)].graphics();
                    if graphics.is_empty() {
                        continue;
                    }
                    let fragments: Vec<(u32, GraphicCell)> = graphics
                        .iter()
                        .map(|g| (index_of(&g.image), g.clone()))
                        .collect();
                    cells.push((screen as u8, line, col as u32, fragments));
                }
            }
        }

        out.u32(table.len() as u32);
        for image in &table {
            out.u32(image.width);
            out.u32(image.height);
            out.bytes(&image.rgba);
        }
        out.u32(store.len() as u32);
        for (id, number, used, image) in store {
            out.u32(id);
            out.u32(number);
            out.u64(used);
            out.u32(image);
        }
        let mut live: Vec<(u64, u32)> = self.live.iter().map(|(&p, &id)| (p, id)).collect();
        live.sort_unstable();
        out.u32(live.len() as u32);
        for (placement, id) in live {
            out.u64(placement);
            out.u32(id);
        }
        let mut named: Vec<((u32, u32), u64)> = self.named.iter().map(|(&k, &p)| (k, p)).collect();
        named.sort_unstable();
        out.u32(named.len() as u32);
        for ((id, client), placement) in named {
            out.u32(id);
            out.u32(client);
            out.u64(placement);
        }
        out.u32(virtuals.len() as u32);
        for ((id, placement), image, v) in virtuals {
            out.u32(id);
            out.u32(placement);
            out.u32(image);
            out.u32(v.cols);
            out.u32(v.rows);
            out.i32(v.z);
        }
        out.u32(cells.len() as u32);
        for (screen, line, col, fragments) in cells {
            out.u8(screen);
            out.i32(line);
            out.u32(col);
            out.u8(fragments.len() as u8);
            for (image, g) in fragments {
                out.u32(image);
                out.u32(g.image_id);
                out.u64(g.placement);
                out.u32(g.client_placement);
                out.i32(g.z);
                for v in g.uv.into_iter().chain(g.dst) {
                    out.u32(v.to_bits());
                }
            }
        }
        match &self.pending {
            None => out.u8(0),
            Some(pending) => {
                out.u8(1);
                out.cmd(&pending.cmd);
                out.bytes(&pending.data);
                out.u64(pending.cap as u64);
                out.str(pending.error.unwrap_or_default());
            }
        }
        out.0
    }

    /// Takes the state [`Graphics::export`] wrote, onto `term`, whose cells are the exported
    /// terminal's apart from their image fragments (same size, same history). This `Graphics` must
    /// be fresh: nothing stored, shown or pending. On an error nothing is kept.
    pub fn import<L: EventListener>(
        &mut self,
        term: &mut Term<L>,
        bytes: &[u8],
    ) -> Result<(), String> {
        if !self.images.is_empty()
            || self.live_bytes() > 0
            || self.pending.is_some()
            || !self.virtuals.lock().unwrap().is_empty()
        {
            return Err("the graphics state isn't fresh".into());
        }
        let carried = read(bytes, self.live_bytes.clone())?;
        // Cells first, all checked before any is touched.
        let grids = term.grids_mut();
        for (screen, line, col, fragments) in &carried.cells {
            let grid = grids
                .get(*screen as usize)
                .ok_or("a cell on an unknown screen")?;
            if !all_lines(grid).contains(line) || *col as usize >= grid.columns() {
                return Err(format!("a cell at {line}:{col} outside the terminal"));
            }
            if !grid[Line(*line)][Column(*col as usize)]
                .graphics()
                .is_empty()
            {
                return Err(format!("the cell at {line}:{col} already holds images"));
            }
            if fragments.is_empty() || fragments.len() > MAX_PER_CELL {
                return Err("a cell with too many fragments".into());
            }
        }
        for (screen, line, col, fragments) in carried.cells {
            let cell = &mut grids[screen as usize][Line(line)][Column(col as usize)];
            for g in fragments {
                cell.add_graphic(g, MAX_PER_CELL);
            }
        }
        self.quota = carried.quota;
        self.clock = carried.clock;
        self.next_auto_id = carried.next_auto_id;
        self.next_placement = carried.next_placement;
        // Anything drawn from before is stale: a renderer starts afresh.
        self.revision = carried.revision + 1;
        self.images = carried.store;
        self.live = carried.live;
        self.named = carried.named;
        *self.virtuals.lock().unwrap() = carried.virtuals;
        self.pending = carried.pending;
        term.mark_fully_damaged();
        Ok(())
    }
}

type CarriedCell = (u8, i32, u32, Vec<GraphicCell>);

struct Carried {
    quota: usize,
    clock: u64,
    next_auto_id: u32,
    next_placement: u64,
    revision: u64,
    store: HashMap<u32, Stored>,
    live: HashMap<u64, u32>,
    named: HashMap<(u32, u32), u64>,
    virtuals: HashMap<(u32, u32), Virtual>,
    cells: Vec<CarriedCell>,
    pending: Option<Pending>,
}

fn read(bytes: &[u8], live_bytes: Arc<std::sync::atomic::AtomicUsize>) -> Result<Carried, String> {
    let mut r = In { bytes, at: 0 };
    if r.take(4)? != MAGIC {
        return Err("not a graphics state".into());
    }
    let version = r.u16()?;
    if version != CARRY_VERSION {
        return Err(format!(
            "graphics state version {version} (this reads {CARRY_VERSION})"
        ));
    }
    let quota = r.u64()? as usize;
    let clock = r.u64()?;
    let next_auto_id = r.u32()?;
    let next_placement = r.u64()?;
    let revision = r.u64()?;
    // Each only ever counts up by one: far from overflowing in a real terminal's life.
    if [clock, next_placement, revision]
        .iter()
        .any(|n| *n > COUNTER_LIMIT)
    {
        return Err("a counter beyond any real one".into());
    }

    let count = r.count(8)?;
    let mut table = Vec::with_capacity(count);
    for _ in 0..count {
        let (width, height) = (r.u32()?, r.u32()?);
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(format!("an image of {width}x{height}"));
        }
        let len = width as usize * height as usize * 4;
        let rgba = r.bytes()?;
        if rgba.len() != len || len > MAX_IMAGE_BYTES {
            return Err(format!(
                "an image of {width}x{height} with {} bytes",
                rgba.len()
            ));
        }
        table.push(Arc::new(GraphicImage::new(
            width,
            height,
            rgba.to_vec(),
            live_bytes.clone(),
        )));
    }
    let image = |i: u32| table.get(i as usize).cloned().ok_or("an unknown image");

    let mut store = HashMap::new();
    for _ in 0..r.count(20)? {
        let (id, number, used, at) = (r.u32()?, r.u32()?, r.u64()?, r.u32()?);
        let stored = Stored {
            image: image(at)?,
            number,
            used,
        };
        if store.insert(id, stored).is_some() {
            return Err(format!("image {id} twice"));
        }
    }
    let count = r.count(12)?;
    if count > MAX_PLACEMENTS {
        return Err("too many placements".into());
    }
    let mut live = HashMap::with_capacity(count);
    for _ in 0..count {
        live.insert(r.u64()?, r.u32()?);
    }
    let count = r.count(16)?;
    if count > MAX_PLACEMENTS {
        return Err("too many named placements".into());
    }
    let mut named = HashMap::with_capacity(count);
    for _ in 0..count {
        named.insert((r.u32()?, r.u32()?), r.u64()?);
    }
    let count = r.count(24)?;
    if count > MAX_PLACEMENTS {
        return Err("too many virtual placements".into());
    }
    let mut virtuals = HashMap::with_capacity(count);
    for _ in 0..count {
        let key = (r.u32()?, r.u32()?);
        let v = Virtual {
            image: image(r.u32()?)?,
            cols: r.u32()?,
            rows: r.u32()?,
            z: r.i32()?,
        };
        // As `put` makes them: renderers multiply these by the cell size.
        if !(1..=MAX_SPAN).contains(&v.cols) || !(1..=MAX_SPAN).contains(&v.rows) {
            return Err(format!(
                "a virtual placement of {}x{} cells",
                v.cols, v.rows
            ));
        }
        virtuals.insert(key, v);
    }
    let count = r.count(10)?;
    let mut cells = Vec::with_capacity(count);
    for _ in 0..count {
        let (screen, line, col) = (r.u8()?, r.i32()?, r.u32()?);
        let n = r.u8()? as usize;
        let mut fragments = Vec::with_capacity(n.min(MAX_PER_CELL));
        for _ in 0..n {
            let image = image(r.u32()?)?;
            let (image_id, placement, client_placement, z) =
                (r.u32()?, r.u64()?, r.u32()?, r.i32()?);
            let mut box8 = [0f32; 8];
            for v in &mut box8 {
                *v = f32::from_bits(r.u32()?);
            }
            // Fractions of the image and of the cell, as `put` makes them.
            if box8.iter().any(|v| !(0.0..=1.0).contains(v)) {
                return Err("a fragment outside its image or cell".into());
            }
            fragments.push(GraphicCell {
                image,
                image_id,
                placement,
                client_placement,
                z,
                uv: [box8[0], box8[1], box8[2], box8[3]],
                dst: [box8[4], box8[5], box8[6], box8[7]],
            });
        }
        cells.push((screen, line, col, fragments));
    }
    let pending = match r.u8()? {
        0 => None,
        1 => {
            let cmd = r.cmd()?;
            let data = r.bytes()?;
            let cap = r.u64()? as usize;
            if data.len() > cap || cap > MAX_IMAGE_BYTES {
                return Err("a transmission beyond its cap".into());
            }
            let data = data.to_vec();
            // The same `&'static str`: what its start refused it with, or a chunk did.
            let error = match r.str()? {
                "" => None,
                e => Some(
                    transmission_cap(&cmd)
                        .err()
                        .into_iter()
                        .chain([TOO_LARGE, BAD_BASE64])
                        .find(|known| *known == e)
                        .ok_or("a transmission with an unknown error")?,
                ),
            };
            Some(Pending {
                cmd,
                data,
                cap,
                error,
            })
        }
        _ => return Err("a bad transmission marker".into()),
    };
    if r.at != bytes.len() {
        return Err("bytes after the graphics state".into());
    }
    Ok(Carried {
        quota,
        clock,
        next_auto_id,
        next_placement,
        revision,
        store,
        live,
        named,
        virtuals,
        cells,
        pending,
    })
}

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    /// Length-prefixed.
    fn bytes(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
    fn cmd(&mut self, c: &Cmd) {
        for v in [
            c.action,
            c.quiet,
            c.medium,
            c.compressed as u8,
            c.more as u8,
            c.no_move as u8,
            c.unicode as u8,
            c.delete,
        ] {
            self.u8(v);
        }
        for v in [
            c.format,
            c.id,
            c.number,
            c.placement,
            c.width,
            c.height,
            c.x,
            c.y,
            c.w,
            c.h,
            c.off_x,
            c.off_y,
            c.cols,
            c.rows,
        ] {
            self.u32(v);
        }
        self.i32(c.z);
        self.u64(c.keys as u64);
    }
}

struct In<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("the graphics state ends early")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let len = usize::try_from(self.u64()?).map_err(|_| "a length beyond memory")?;
        self.take(len)
    }
    fn str(&mut self) -> Result<&'a str, String> {
        std::str::from_utf8(self.bytes()?).map_err(|_| "a string that isn't UTF-8".into())
    }
    /// A count of items at least `each` bytes long: never more than the bytes left could hold.
    fn count(&mut self, each: usize) -> Result<usize, String> {
        let n = self.u32()? as usize;
        if n.saturating_mul(each) > self.bytes.len() - self.at {
            return Err("a count beyond the graphics state".into());
        }
        Ok(n)
    }
    fn cmd(&mut self) -> Result<Cmd, String> {
        let mut flags = [0u8; 8];
        for v in &mut flags {
            *v = self.u8()?;
        }
        let mut n = [0u32; 14];
        for v in &mut n {
            *v = self.u32()?;
        }
        let bool_of = |b: u8| match b {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("a bad flag".to_string()),
        };
        Ok(Cmd {
            action: flags[0],
            quiet: flags[1],
            medium: flags[2],
            compressed: bool_of(flags[3])?,
            more: bool_of(flags[4])?,
            no_move: bool_of(flags[5])?,
            unicode: bool_of(flags[6])?,
            delete: flags[7],
            format: n[0],
            id: n[1],
            number: n[2],
            placement: n[3],
            width: n[4],
            height: n[5],
            x: n[6],
            y: n[7],
            w: n[8],
            h: n[9],
            off_x: n[10],
            off_y: n[11],
            cols: n[12],
            rows: n[13],
            z: self.i32()?,
            keys: usize::try_from(self.u64()?).map_err(|_| "a bad key count")?,
        })
    }
}
