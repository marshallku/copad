import Foundation

/// An sRGB colour, 0…1 components.
struct RGB: Equatable, Hashable {
    var r: Double, g: Double, b: Double

    init(_ r: Double, _ g: Double, _ b: Double) { self.r = r; self.g = g; self.b = b }

    init(hex: UInt32) {
        r = Double((hex >> 16) & 0xFF) / 255
        g = Double((hex >> 8) & 0xFF) / 255
        b = Double(hex & 0xFF) / 255
    }
}

/// Resolves the render wire's colour strings (ratatui serde: `"Reset"`, `"Red"`, `"#RRGGBB"`,
/// `"12"` for an indexed colour) to RGB. Named and indexed 0–15 colours use Catppuccin Mocha —
/// copad's default theme and the palette comux's own chrome is designed against.
enum Palette {
    static let defaultFG = RGB(hex: 0xCDD6F4)
    static let defaultBG = RGB(hex: 0x11111B)

    /// ANSI 0–15 in ratatui's naming order.
    static let ansi: [RGB] = [
        0x45475A, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xBAC2DE,
        0x585B70, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xA6ADC8,
    ].map { RGB(hex: $0) }

    static let named: [String: Int] = [
        "Black": 0, "Red": 1, "Green": 2, "Yellow": 3, "Blue": 4, "Magenta": 5, "Cyan": 6,
        "Gray": 7, "DarkGray": 8, "LightRed": 9, "LightGreen": 10, "LightYellow": 11,
        "LightBlue": 12, "LightMagenta": 13, "LightCyan": 14, "White": 15,
    ]

    /// `nil` = `"Reset"` (the default for that layer).
    static func resolve(_ s: String) -> RGB? {
        if s == "Reset" { return nil }
        if s.hasPrefix("#"), s.count == 7, let v = UInt32(s.dropFirst(), radix: 16) {
            return RGB(hex: v)
        }
        if let i = named[s] { return ansi[i] }
        if let i = Int(s), (0...255).contains(i) { return indexed(i) }
        return nil
    }

    /// The xterm 256-colour cube.
    static func indexed(_ i: Int) -> RGB {
        if i < 16 { return ansi[i] }
        if i < 232 {
            let n = i - 16
            let step = { (v: Int) -> Double in v == 0 ? 0 : Double(55 + v * 40) / 255 }
            return RGB(step(n / 36), step((n / 6) % 6), step(n % 6))
        }
        let v = Double(8 + (i - 232) * 10) / 255
        return RGB(v, v, v)
    }
}

/// Modifier bits from the wire's bitflag string (`"BOLD | DIM"`).
struct Mods: OptionSet, Hashable {
    let rawValue: UInt16
    static let bold = Mods(rawValue: 1 << 0)
    static let dim = Mods(rawValue: 1 << 1)
    static let italic = Mods(rawValue: 1 << 2)
    static let underlined = Mods(rawValue: 1 << 3)
    static let reversed = Mods(rawValue: 1 << 6)
    static let hidden = Mods(rawValue: 1 << 7)
    static let crossedOut = Mods(rawValue: 1 << 8)

    static func parse(_ s: String) -> Mods {
        var m: Mods = []
        for part in s.split(separator: "|") {
            switch part.trimmingCharacters(in: .whitespaces) {
            case "BOLD": m.insert(.bold)
            case "DIM": m.insert(.dim)
            case "ITALIC": m.insert(.italic)
            case "UNDERLINED": m.insert(.underlined)
            case "REVERSED": m.insert(.reversed)
            case "HIDDEN": m.insert(.hidden)
            case "CROSSED_OUT": m.insert(.crossedOut)
            default: break // blink: not drawn
            }
        }
        return m
    }
}

/// One cell of the mirror grid.
struct Cell: Equatable {
    var sym: String = " "
    var fg: RGB? = nil
    var bg: RGB? = nil
    var mods: Mods = []
    /// The trailing half of a wide glyph: not drawn on its own.
    var skip = false
}

/// The client's mirror of the server's composed screen.
///
/// EVERY frame is applied here as it arrives — only presentation is rate-limited — because
/// the server advances its diff baseline the moment a frame is queued: a skipped delta would
/// leave cells stale forever (plan § iOS native client, codex C3).
struct Grid {
    private(set) var cols = 0
    private(set) var rows = 0
    private(set) var cells: [Cell] = []
    var cursor: (x: Int, y: Int)? = nil
    var cursorVisible = true
    /// A `full` frame has been applied since the last reset; deltas before it are dropped.
    private(set) var hasBaseline = false

    subscript(x: Int, y: Int) -> Cell {
        get { cells[y * cols + x] }
        set { cells[y * cols + x] = newValue }
    }

    /// Forget everything (reconnect): wait for a fresh `full` frame.
    mutating func reset() {
        cols = 0; rows = 0; cells = []; cursor = nil; hasBaseline = false
    }

    /// Apply one frame. Returns whether the grid changed.
    @discardableResult
    mutating func apply(_ f: FrameMsg) -> Bool {
        if f.full {
            cols = max(f.cols, 1); rows = max(f.rows, 1)
            cells = Array(repeating: Cell(), count: cols * rows)
            hasBaseline = true
        } else if !hasBaseline || f.cols != cols || f.rows != rows {
            // A delta for a size we don't hold yet — its `full` frame is coming.
            return false
        }
        for c in f.cells where c.x < cols && c.y < rows {
            self[c.x, c.y] = Cell(sym: c.sym, fg: Palette.resolve(c.fg), bg: Palette.resolve(c.bg),
                                  mods: Mods.parse(c.mods), skip: c.skip)
        }
        fixWideSpacers()
        if let cur = f.cursor, cur.count == 2 { cursor = (cur[0], cur[1]) } else { cursor = nil }
        cursorVisible = f.cursorVisible
        return true
    }

    /// The wire omits the trailing spacer of a wide glyph (ratatui's diff drops the cell after
    /// it), so rebuild spacer structure from the glyphs — a port of comux's
    /// `client::fix_wide_spacers`, or a wide glyph that MOVED would leave its old spacer behind.
    mutating func fixWideSpacers() {
        for y in 0..<rows {
            var prevWide = false
            for x in 0..<cols {
                let i = y * cols + x
                if prevWide {
                    cells[i].sym = " "
                    cells[i].skip = true
                    prevWide = false
                } else {
                    cells[i].skip = false
                    prevWide = CellWidth.of(cells[i].sym) >= 2
                }
            }
        }
    }

    /// Row `y` as plain text (spacers dropped) — for tests and accessibility.
    func text(row y: Int) -> String {
        guard y < rows else { return "" }
        return (0..<cols).compactMap { x in self[x, y].skip ? nil : self[x, y].sym }.joined()
    }
}

/// Terminal column width of a grapheme — the East Asian Wide/Fullwidth ranges plus emoji
/// presentation, which is what decides a 2-column cell in comux's grid.
enum CellWidth {
    static func of(_ s: String) -> Int {
        guard let u = s.unicodeScalars.first else { return 1 }
        if s.unicodeScalars.contains(where: { $0.value == 0xFE0F }) { return 2 }
        if u.properties.isEmojiPresentation { return 2 }
        let v = u.value
        let wide: [ClosedRange<UInt32>] = [
            0x1100...0x115F, 0x231A...0x231B, 0x2329...0x232A, 0x2E80...0x303E,
            0x3041...0x33FF, 0x3400...0x4DBF, 0x4E00...0x9FFF, 0xA000...0xA4CF,
            0xA960...0xA97F, 0xAC00...0xD7A3, 0xF900...0xFAFF, 0xFE10...0xFE19,
            0xFE30...0xFE6F, 0xFF00...0xFF60, 0xFFE0...0xFFE6, 0x1F300...0x1F64F,
            0x1F900...0x1F9FF, 0x20000...0x2FFFD, 0x30000...0x3FFFD,
        ]
        return wide.contains { $0.contains(v) } ? 2 : 1
    }
}
