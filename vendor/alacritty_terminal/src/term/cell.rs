use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use bitflags::bitflags;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::grid::{self, GridCell};
use crate::index::Column;
use crate::vte::ansi::{Color, Hyperlink as VteHyperlink, NamedColor};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct Flags: u16 {
        const INVERSE                   = 0b0000_0000_0000_0001;
        const BOLD                      = 0b0000_0000_0000_0010;
        const ITALIC                    = 0b0000_0000_0000_0100;
        const BOLD_ITALIC               = 0b0000_0000_0000_0110;
        const UNDERLINE                 = 0b0000_0000_0000_1000;
        const WRAPLINE                  = 0b0000_0000_0001_0000;
        const WIDE_CHAR                 = 0b0000_0000_0010_0000;
        const WIDE_CHAR_SPACER          = 0b0000_0000_0100_0000;
        const DIM                       = 0b0000_0000_1000_0000;
        const DIM_BOLD                  = 0b0000_0000_1000_0010;
        const HIDDEN                    = 0b0000_0001_0000_0000;
        const STRIKEOUT                 = 0b0000_0010_0000_0000;
        const LEADING_WIDE_CHAR_SPACER  = 0b0000_0100_0000_0000;
        const DOUBLE_UNDERLINE          = 0b0000_1000_0000_0000;
        const UNDERCURL                 = 0b0001_0000_0000_0000;
        const DOTTED_UNDERLINE          = 0b0010_0000_0000_0000;
        const DASHED_UNDERLINE          = 0b0100_0000_0000_0000;
        const ALL_UNDERLINES            = Self::UNDERLINE.bits() | Self::DOUBLE_UNDERLINE.bits()
                                        | Self::UNDERCURL.bits() | Self::DOTTED_UNDERLINE.bits()
                                        | Self::DASHED_UNDERLINE.bits();
    }
}

/// Counter for hyperlinks without explicit ID.
static HYPERLINK_ID_SUFFIX: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Hyperlink {
    inner: Arc<HyperlinkInner>,
}

impl Hyperlink {
    pub fn new<T: ToString>(id: Option<T>, uri: String) -> Self {
        let inner = Arc::new(HyperlinkInner::new(id, uri));
        Self { inner }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn uri(&self) -> &str {
        &self.inner.uri
    }
}

impl From<VteHyperlink> for Hyperlink {
    fn from(value: VteHyperlink) -> Self {
        Self::new(value.id, value.uri)
    }
}

impl From<Hyperlink> for VteHyperlink {
    fn from(val: Hyperlink) -> Self {
        VteHyperlink {
            id: Some(val.id().to_owned()),
            uri: val.uri().to_owned(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct HyperlinkInner {
    /// Identifier for the given hyperlink.
    id: String,

    /// Resource identifier of the hyperlink.
    uri: String,
}

impl HyperlinkInner {
    pub fn new<T: ToString>(id: Option<T>, uri: String) -> Self {
        let id = match id {
            Some(id) => id.to_string(),
            None => {
                let mut id = HYPERLINK_ID_SUFFIX
                    .fetch_add(1, Ordering::Relaxed)
                    .to_string();
                id.push_str("_alacritty");
                id
            }
        };

        Self { id, uri }
    }
}

/// Trait for determining if a reset should be performed.
pub trait ResetDiscriminant<T> {
    /// Value based on which equality for the reset will be determined.
    fn discriminant(&self) -> T;
}

impl<T: Copy> ResetDiscriminant<T> for T {
    fn discriminant(&self) -> T {
        *self
    }
}

impl ResetDiscriminant<Color> for Cell {
    fn discriminant(&self) -> Color {
        self.bg
    }
}

/// Dynamically allocated cell content.
///
/// This storage is reserved for cell attributes which are rarely set. This allows reducing the
/// allocation required ahead of time for every cell, with some additional overhead when the extra
/// storage is actually required.
#[derive(Default, Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct CellExtra {
    zerowidth: Vec<char>,
    underline_color: Option<Color>,
    hyperlink: Option<Hyperlink>,
    /// Image fragments drawn in this cell, bottom-most first (copad patch).
    #[cfg_attr(feature = "serde", serde(skip))]
    graphics: Vec<GraphicCell>,
}

impl CellExtra {
    /// Nothing left worth an allocation.
    fn is_redundant(&self) -> bool {
        self.zerowidth.is_empty()
            && self.underline_color.is_none()
            && self.hyperlink.is_none()
            && self.graphics.is_empty()
    }
}

/// Decoded pixels of one image, shared by every cell that shows part of it (copad patch).
///
/// `rgba` is premultiplied RGBA8, `width * height * 4` bytes. `live_bytes` is the owning terminal's
/// count of decoded image bytes still alive anywhere — store, cells, scrollback, snapshots — so a
/// quota can be judged on what is actually held.
pub struct GraphicImage {
    /// Unique for the process; renderers cache uploaded textures by it.
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    live_bytes: Arc<AtomicUsize>,
}

impl GraphicImage {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>, live_bytes: Arc<AtomicUsize>) -> Self {
        static NEXT_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        live_bytes.fetch_add(rgba.len(), Ordering::Relaxed);
        Self {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            rgba,
            live_bytes,
        }
    }
}

impl Drop for GraphicImage {
    fn drop(&mut self) {
        self.live_bytes
            .fetch_sub(self.rgba.len(), Ordering::Relaxed);
    }
}

impl std::fmt::Debug for GraphicImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GraphicImage({} {}x{})",
            self.key, self.width, self.height
        )
    }
}

/// The part of one image placement that falls in one cell (copad patch).
///
/// `uv` is the source rectangle in the image, `dst` the rectangle it is drawn into inside the
/// cell, both `[left, top, right, bottom]` normalized to 0..=1.
#[derive(Clone, Debug)]
pub struct GraphicCell {
    pub image: Arc<GraphicImage>,
    /// The client's image id.
    pub image_id: u32,
    /// Unique per placement within the terminal (also for placements the client did not name).
    pub placement: u64,
    /// The client's placement id, 0 if it gave none.
    pub client_placement: u32,
    pub z: i32,
    pub uv: [f32; 4],
    pub dst: [f32; 4],
}

impl GraphicCell {
    /// Stacking order: lower draws first.
    pub fn order(&self) -> (i32, u32, u64) {
        (self.z, self.image_id, self.placement)
    }
}

impl PartialEq for GraphicCell {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.image, &other.image)
            && self.placement == other.placement
            && self.z == other.z
            && self.uv.map(f32::to_bits) == other.uv.map(f32::to_bits)
            && self.dst.map(f32::to_bits) == other.dst.map(f32::to_bits)
    }
}

impl Eq for GraphicCell {}

/// Content and attributes of a single cell in the terminal grid.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
    pub extra: Option<Arc<CellExtra>>,
}

impl Default for Cell {
    #[inline]
    fn default() -> Cell {
        Cell {
            c: ' ',
            bg: Color::Named(NamedColor::Background),
            fg: Color::Named(NamedColor::Foreground),
            flags: Flags::empty(),
            extra: None,
        }
    }
}

impl Cell {
    /// Zerowidth characters stored in this cell.
    #[inline]
    pub fn zerowidth(&self) -> Option<&[char]> {
        self.extra.as_ref().map(|extra| extra.zerowidth.as_slice())
    }

    /// Write a new zerowidth character to this cell.
    #[inline]
    pub fn push_zerowidth(&mut self, character: char) {
        let extra = self.extra.get_or_insert(Default::default());
        Arc::make_mut(extra).zerowidth.push(character);
    }

    /// Image fragments in this cell, bottom-most first (copad patch).
    #[inline]
    pub fn graphics(&self) -> &[GraphicCell] {
        self.extra.as_ref().map_or(&[], |extra| &extra.graphics)
    }

    /// Add an image fragment, replacing one of the same placement, keeping stacking order and at
    /// most `max` fragments (the lowest-stacked goes first) (copad patch).
    pub fn add_graphic(&mut self, graphic: GraphicCell, max: usize) {
        let extra = Arc::make_mut(self.extra.get_or_insert(Default::default()));
        extra.graphics.retain(|g| g.placement != graphic.placement);
        let at = extra
            .graphics
            .partition_point(|g| g.order() <= graphic.order());
        extra.graphics.insert(at, graphic);
        while extra.graphics.len() > max {
            extra.graphics.remove(0);
        }
    }

    /// Drop the image fragments `drop` matches; true if any went (copad patch).
    pub fn remove_graphics(&mut self, mut drop: impl FnMut(&GraphicCell) -> bool) -> bool {
        let Some(extra) = self.extra.as_mut() else {
            return false;
        };
        if !extra.graphics.iter().any(&mut drop) {
            return false;
        }
        let owned = Arc::make_mut(extra);
        owned.graphics.retain(|g| !drop(g));
        if owned.is_redundant() {
            self.extra = None;
        }
        true
    }

    /// Remove all wide char data from a cell.
    #[inline(never)]
    pub fn clear_wide(&mut self) {
        self.flags.remove(Flags::WIDE_CHAR);
        if let Some(extra) = self.extra.as_mut() {
            Arc::make_mut(extra).zerowidth = Vec::new();
        }
        self.c = ' ';
    }

    /// Set underline color on the cell.
    pub fn set_underline_color(&mut self, color: Option<Color>) {
        // If we reset color and we don't have zerowidth we should drop extra storage.
        if color.is_none()
            && self.extra.as_ref().is_none_or(|extra| {
                extra.zerowidth.is_empty() && extra.hyperlink.is_none() && extra.graphics.is_empty()
            })
        {
            self.extra = None;
        } else {
            let extra = self.extra.get_or_insert(Default::default());
            Arc::make_mut(extra).underline_color = color;
        }
    }

    /// Underline color stored in this cell.
    #[inline]
    pub fn underline_color(&self) -> Option<Color> {
        self.extra.as_ref()?.underline_color
    }

    /// Set hyperlink.
    pub fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        let should_drop = hyperlink.is_none()
            && self.extra.as_ref().is_none_or(|extra| {
                extra.zerowidth.is_empty()
                    && extra.underline_color.is_none()
                    && extra.graphics.is_empty()
            });

        if should_drop {
            self.extra = None;
        } else {
            let extra = self.extra.get_or_insert(Default::default());
            Arc::make_mut(extra).hyperlink = hyperlink;
        }
    }

    /// Hyperlink stored in this cell.
    #[inline]
    pub fn hyperlink(&self) -> Option<Hyperlink> {
        self.extra.as_ref()?.hyperlink.clone()
    }
}

impl GridCell for Cell {
    #[inline]
    fn is_empty(&self) -> bool {
        (self.c == ' ' || self.c == '\t')
            && self.bg == Color::Named(NamedColor::Background)
            && self.fg == Color::Named(NamedColor::Foreground)
            && !self.flags.intersects(
                Flags::INVERSE
                    | Flags::ALL_UNDERLINES
                    | Flags::STRIKEOUT
                    | Flags::WRAPLINE
                    | Flags::WIDE_CHAR_SPACER
                    | Flags::LEADING_WIDE_CHAR_SPACER,
            )
            && self
                .extra
                .as_ref()
                .map(|extra| extra.zerowidth.is_empty() && extra.graphics.is_empty())
                != Some(false)
    }

    #[inline]
    fn flags(&self) -> &Flags {
        &self.flags
    }

    #[inline]
    fn flags_mut(&mut self) -> &mut Flags {
        &mut self.flags
    }

    #[inline]
    fn reset(&mut self, template: &Self) {
        *self = Cell {
            bg: template.bg,
            ..Cell::default()
        };
    }
}

impl From<Color> for Cell {
    #[inline]
    fn from(color: Color) -> Self {
        Self {
            bg: color,
            ..Cell::default()
        }
    }
}

/// Get the length of occupied cells in a line.
pub trait LineLength {
    /// Calculate the occupied line length.
    fn line_length(&self) -> Column;
}

impl LineLength for grid::Row<Cell> {
    fn line_length(&self) -> Column {
        let mut length = Column(0);

        if self[Column(self.len() - 1)].flags.contains(Flags::WRAPLINE) {
            return Column(self.len());
        }

        for (index, cell) in self[..].iter().rev().enumerate() {
            if cell.c != ' '
                || cell.extra.as_ref().map(|extra| extra.zerowidth.is_empty()) == Some(false)
            {
                length = Column(self.len() - index);
                break;
            }
        }

        length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::mem;

    use crate::grid::Row;
    use crate::index::Column;

    #[test]
    fn cell_size_is_below_cap() {
        // Expected cell size on 64-bit architectures.
        const EXPECTED_CELL_SIZE: usize = 24;

        // Ensure that cell size isn't growing by accident.
        assert!(mem::size_of::<Cell>() <= EXPECTED_CELL_SIZE);
    }

    #[test]
    fn line_length_works() {
        let mut row = Row::<Cell>::new(10);
        row[Column(5)].c = 'a';

        assert_eq!(row.line_length(), Column(6));
    }

    #[test]
    fn line_length_works_with_wrapline() {
        let mut row = Row::<Cell>::new(10);
        row[Column(9)].flags.insert(super::Flags::WRAPLINE);

        assert_eq!(row.line_length(), Column(10));
    }
}
