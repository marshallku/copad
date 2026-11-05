import Foundation

// Pure-logic building blocks for the Metal render path (docs/macos-gpu-renderer-plan.md). No
// AppKit/Metal/CoreText dependency, so the tables are testable from `CopadCoreTests` (the
// executable target can't be imported by the test bundle).
//
// Color convention: RGBA32 packed as `(r << 24) | (g << 16) | (b << 8) | a`, matching the
// executable's `cgColorToRGBA32`.

// MARK: - Cell flags (copad-term wire format)

/// Flag bits mirror `copad_term::flags` (copad-term/src/lib.rs); `drawRow` keeps its own copies.
public enum CopadCellFlags {
    public static let bold: UInt16 = 1 << 0
    public static let italic: UInt16 = 1 << 1
    public static let inverse: UInt16 = 1 << 3
    public static let dim: UInt16 = 1 << 4
    public static let strike: UInt16 = 1 << 5
    public static let wideLeading: UInt16 = 1 << 7
}

// MARK: - Color resolution

public enum GPUCellColor {
    /// Decode `copad_term::color_to_rgba`: high byte is a tag (0x00 default, 0x01 indexed with
    /// the index in the low byte, 0xFF direct RGB in the low 24 bits).
    public static func resolve(_ packed: UInt32, palette: [UInt32], defaultColor: UInt32) -> UInt32 {
        let tag = (packed >> 24) & 0xFF
        switch tag {
        case 0x00:
            return defaultColor
        case 0x01:
            let idx = Int(packed & 0xFF)
            return idx < palette.count ? palette[idx] : defaultColor
        case 0xFF:
            return ((packed & 0x00FF_FFFF) << 8) | 0xFF
        default:
            return defaultColor
        }
    }

    /// Apply the ANSI dim convention: scale alpha to ~65% (the same
    /// value the CPU painter uses via `CGColor.copy(alpha: 0.65)`).
    public static func dimmed(_ rgba: UInt32) -> UInt32 {
        let alpha = rgba & 0xFF
        let scaled = UInt32((Double(alpha) * 0.65).rounded())
        return (rgba & 0xFFFF_FF00) | scaled
    }
}

// MARK: - Per-cell quad decision table

/// Port of `drawRow`'s color-resolve decision table as a pure function.
public enum CellQuadResolver {
    public struct Resolved: Equatable, Sendable {
        public let fg: UInt32
        public let bg: UInt32
        /// Whether the cell needs its own bg quad. Opaque mode: false
        /// when the resolved bg equals the bounds-clear color
        /// (already painted). Transparent mode: false only for
        /// default-sentinel non-inverse cells — those are the only
        /// cells the wallpaper / blurred desktop bleeds through.
        public let paintBg: Bool

        public init(fg: UInt32, bg: UInt32, paintBg: Bool) {
            self.fg = fg
            self.bg = bg
            self.paintBg = paintBg
        }
    }

    public static func resolve(
        fgPacked: UInt32,
        bgPacked: UInt32,
        flags: UInt16,
        transparentMode: Bool,
        palette: [UInt32],
        defaultFg: UInt32,
        defaultBg: UInt32,
    ) -> Resolved {
        // Provenance BEFORE resolution: an explicit ANSI bg that
        // happens to equal theme.background must still paint in
        // transparent mode; a default-sentinel cell must not.
        let bgIsDefault = bgPacked == 0
        let isInverse = flags & CopadCellFlags.inverse != 0

        var fg = GPUCellColor.resolve(fgPacked, palette: palette, defaultColor: defaultFg)
        var bg = GPUCellColor.resolve(bgPacked, palette: palette, defaultColor: defaultBg)
        if isInverse {
            swap(&fg, &bg)
        }
        if flags & CopadCellFlags.dim != 0 {
            fg = GPUCellColor.dimmed(fg)
        }

        let skipFill = transparentMode
            ? (bgIsDefault && !isInverse)
            : bg == defaultBg
        return Resolved(fg: fg, bg: bg, paintBg: !skipFill)
    }
}

// MARK: - Atlas shelf packer

/// Minimal shelf packer for the glyph atlas. Returns `nil` on overflow; the flush-and-rebuild
/// policy is the caller's concern.
public struct AtlasShelfPacker: Sendable {
    public struct Placement: Equatable, Sendable {
        public let x: Int
        public let y: Int

        public init(x: Int, y: Int) {
            self.x = x
            self.y = y
        }
    }

    private struct Shelf {
        let y: Int
        let height: Int
        var cursorX: Int
    }

    public let width: Int
    public let height: Int
    /// Empty pixels added on the right/bottom of every placement so
    /// linear-sampled quads never bleed a neighbour's edge texels.
    public let padding: Int

    private var shelves: [Shelf] = []
    private var nextShelfY = 0

    public init(width: Int, height: Int, padding: Int = 1) {
        self.width = width
        self.height = height
        self.padding = padding
    }

    /// The returned origin is the rect's top-left; the padded extent is what gets reserved.
    /// Zero/negative dimensions clamp to 1.
    public mutating func place(width w: Int, height h: Int) -> Placement? {
        let pw = max(1, w) + padding
        let ph = max(1, h) + padding
        guard pw <= width else { return nil }

        // First shelf the rect fits in. Terminal glyph heights cluster around the cell height,
        // so reusing shelves without a best-fit scan wastes little.
        for i in shelves.indices where ph <= shelves[i].height && shelves[i].cursorX + pw <= width {
            let p = Placement(x: shelves[i].cursorX, y: shelves[i].y)
            shelves[i].cursorX += pw
            return p
        }

        guard nextShelfY + ph <= height else { return nil }
        let shelf = Shelf(y: nextShelfY, height: ph, cursorX: pw)
        shelves.append(shelf)
        nextShelfY += ph
        return Placement(x: 0, y: shelf.y)
    }

    public mutating func reset() {
        shelves.removeAll(keepingCapacity: true)
        nextShelfY = 0
    }
}

// MARK: - Grid quad geometry

/// Axis-aligned quad in view points (origin top-left, y down — the
/// flipped coordinate system the render view already uses).
public struct GPUQuad: Equatable, Sendable {
    public var x: Double
    public var y: Double
    public var width: Double
    public var height: Double

    public init(x: Double, y: Double, width: Double, height: Double) {
        self.x = x
        self.y = y
        self.width = width
        self.height = height
    }
}

public enum GridQuadGeometry {
    public struct CellSpan: Equatable, Sendable {
        public let row: Int
        public let firstCol: Int
        public let finalCol: Int

        public init(row: Int, firstCol: Int, finalCol: Int) {
            self.row = row
            self.firstCol = firstCol
            self.finalCol = finalCol
        }
    }

    /// Expand an inclusive (start, end) grid range into per-row spans, shared by
    /// `paintSelection` and `paintSearchMatch`. Block mode paints the same column span on every
    /// row; row-wrapped mode covers start_col...lastCol, then full rows, then 0...end_col.
    /// Spans with `firstCol > finalCol` are dropped (as in the CPU painter).
    public static func spanRows(
        startRow: Int,
        startCol: Int,
        endRow: Int,
        endCol: Int,
        lastCol: Int,
        isBlock: Bool,
    ) -> [CellSpan] {
        guard startRow <= endRow else { return [] }
        var out: [CellSpan] = []

        if isBlock {
            let firstCol = max(0, min(startCol, lastCol))
            let finalCol = max(0, min(endCol, lastCol))
            guard firstCol <= finalCol else { return [] }
            for row in startRow ... endRow {
                out.append(CellSpan(row: row, firstCol: firstCol, finalCol: finalCol))
            }
            return out
        }

        for row in startRow ... endRow {
            let firstCol: Int
            let finalCol: Int
            if startRow == endRow {
                firstCol = startCol
                finalCol = endCol
            } else if row == startRow {
                firstCol = startCol
                finalCol = lastCol
            } else if row == endRow {
                firstCol = 0
                finalCol = endCol
            } else {
                firstCol = 0
                finalCol = lastCol
            }
            guard firstCol <= finalCol else { continue }
            out.append(CellSpan(row: row, firstCol: firstCol, finalCol: finalCol))
        }
        return out
    }

    /// Four thin quads for a 1-unit-inset border: the Metal stand-in for
    /// `CGContext.stroke(insetBy: 0.5)` at line width 1.
    public static func outlineQuads(of q: GPUQuad, thickness: Double = 1) -> [GPUQuad] {
        let t = min(thickness, min(q.width, q.height) / 2)
        guard t > 0 else { return [] }
        return [
            GPUQuad(x: q.x, y: q.y, width: q.width, height: t),
            GPUQuad(x: q.x, y: q.y + q.height - t, width: q.width, height: t),
            GPUQuad(x: q.x, y: q.y + t, width: t, height: q.height - 2 * t),
            GPUQuad(x: q.x + q.width - t, y: q.y + t, width: t, height: q.height - 2 * t),
        ]
    }

    /// Cursor quad set for one frame. `accent` quads fill with
    /// theme.accent; `outline` quads fill with theme.background (the
    /// 1-px legibility frame used when the backdrop is unpredictable
    /// — wallpaper or non-opaque window).
    public struct CursorQuads: Equatable, Sendable {
        public let accent: [GPUQuad]
        public let outline: [GPUQuad]
        /// Block style + key window only: the cell glyph re-renders
        /// tinted theme.background on top of the accent fill.
        public let redrawGlyphInverted: Bool

        public init(accent: [GPUQuad], outline: [GPUQuad], redrawGlyphInverted: Bool) {
            self.accent = accent
            self.outline = outline
            self.redrawGlyphInverted = redrawGlyphInverted
        }

        public static let none = CursorQuads(accent: [], outline: [], redrawGlyphInverted: false)
    }

    /// Port of `drawCursor`'s geometry: style 0 hidden, 1 block (hollow outline when not key),
    /// 2 two-px beam, 3 two-px underline. A TUI-requested blinking cursor (blink == 1) draws
    /// nothing on the OFF phase.
    public static func cursorQuads(
        style: UInt8,
        blink: UInt8,
        blinkVisible: Bool,
        isKeyWindow: Bool,
        needsOutline: Bool,
        col: Int,
        row: Int,
        cellWidth: Double,
        cellHeight: Double,
    ) -> CursorQuads {
        guard style != 0, cellWidth > 0, cellHeight > 0,
              blink == 0 || blinkVisible
        else { return .none }

        let cell = GPUQuad(
            x: Double(col) * cellWidth,
            y: Double(row) * cellHeight,
            width: cellWidth,
            height: cellHeight,
        )

        switch style {
        case 1 where isKeyWindow:
            return CursorQuads(
                accent: [cell],
                outline: needsOutline ? outlineQuads(of: cell) : [],
                redrawGlyphInverted: true,
            )
        case 1:
            return CursorQuads(accent: outlineQuads(of: cell), outline: [], redrawGlyphInverted: false)
        case 2:
            let bar = GPUQuad(x: cell.x, y: cell.y, width: 2, height: cellHeight)
            return CursorQuads(
                accent: [bar],
                outline: needsOutline ? outlineQuads(of: bar) : [],
                redrawGlyphInverted: false,
            )
        case 3:
            let bar = GPUQuad(x: cell.x, y: cell.y + cellHeight - 2, width: cellWidth, height: 2)
            return CursorQuads(
                accent: [bar],
                outline: needsOutline ? outlineQuads(of: bar) : [],
                redrawGlyphInverted: false,
            )
        default:
            return .none
        }
    }
}
