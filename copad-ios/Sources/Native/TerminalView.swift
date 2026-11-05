import SwiftUI
import UIKit

/// Draws the mirror grid. comux composes the screen; this view only paints cells at the
/// positions comux assigned them (never at the font's own advances, so CJK and emoji stay
/// in their columns) and turns touches into the mouse/wheel events comux already handles.
final class TerminalView: UIView {
    weak var session: MuxSession?
    let input = InputProxy()

    /// The bundled JetBrains Mono Nerd Font Mono (decisions #174): powerline separators and
    /// Nerd icons render, one cell wide, instead of tofu. The system monospaced font stands in
    /// if the bundled one fails to load; CJK and emoji fall back through the system cascade.
    private let font = UIFont(name: "JetBrainsMonoNFM-Regular", size: 13)
        ?? .monospacedSystemFont(ofSize: 13, weight: .regular)
    private lazy var boldFont = UIFont(name: "JetBrainsMonoNFM-Bold", size: 13)
        ?? .monospacedSystemFont(ofSize: 13, weight: .bold)
    private(set) var cellW: CGFloat = 8
    private(set) var cellH: CGFloat = 16
    private var dirty = false
    private var link: CADisplayLink?
    /// Marked (IME) text, drawn at the cursor until the IME commits it.
    var marked: String = "" { didSet { setNeedsDisplay() } }

    private var panCarry: CGFloat = 0
    private var velocity: CGFloat = 0
    private var inertiaLink: CADisplayLink?
    private var panCell = (x: 0, y: 0)
    /// The long-press menu (Paste): the terminal is not a text view, so iOS offers none itself.
    private lazy var editMenu = UIEditMenuInteraction(delegate: self)

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = UIColor(rgb: Palette.defaultBG)
        isOpaque = true
        contentMode = .redraw
        let m = ("M" as NSString).size(withAttributes: [.font: font])
        cellW = ceil(m.width * 100) / 100
        cellH = ceil(font.lineHeight)
        addSubview(input)
        input.frame = CGRect(x: 0, y: 0, width: 1, height: 1)
        input.terminal = self
        let tap = UITapGestureRecognizer(target: self, action: #selector(onTap(_:)))
        addGestureRecognizer(tap)
        let pan = UIPanGestureRecognizer(target: self, action: #selector(onPan(_:)))
        pan.maximumNumberOfTouches = 1
        addGestureRecognizer(pan)
        addGestureRecognizer(UILongPressGestureRecognizer(target: self, action: #selector(onLongPress(_:))))
        addInteraction(editMenu)
        accessibilityIdentifier = "terminal"
        isAccessibilityElement = true
    }

    required init?(coder: NSCoder) { fatalError() }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        link?.invalidate()
        guard window != nil else { return }
        // Presentation is capped to 30 fps; the grid itself takes every frame as it arrives.
        let l = CADisplayLink(target: self, selector: #selector(tick))
        l.preferredFrameRateRange = CAFrameRateRange(minimum: 15, maximum: 30, preferred: 30)
        l.add(to: .main, forMode: .common)
        link = l
    }

    func gridChanged() { dirty = true }

    @objc private func tick() {
        guard dirty else { return }
        dirty = false
        // Only the rows the frames touched (a streaming agent changes a line or two a frame).
        guard let rowsToPaint = session?.takeDirtyRows() else {
            setNeedsDisplay()
            return
        }
        for range in rowsToPaint.rangeView {
            setNeedsDisplay(CGRect(x: 0, y: CGFloat(range.lowerBound) * cellH,
                                   width: bounds.width, height: CGFloat(range.count) * cellH))
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let cols = Int(bounds.width / cellW), rows = Int(bounds.height / cellH)
        session?.resize(cols: cols, rows: rows)
    }

    // MARK: drawing

    override func draw(_ rect: CGRect) {
        guard let ctx = UIGraphicsGetCurrentContext(), let g = session?.grid else { return }
        ctx.setFillColor(cgColor(Palette.defaultBG))
        ctx.fill(rect)
        guard g.cols > 0 else { return }
        let firstRow = max(0, Int(rect.minY / cellH)), lastRow = min(g.rows - 1, Int(rect.maxY / cellH))
        guard firstRow <= lastRow else { return }
        for y in firstRow...lastRow {
            drawRow(g, y, ctx)
        }
        drawCursor(g, ctx)
    }

    /// Resolved (foreground, background) of a cell, as RGB: comparing these costs nothing,
    /// where building a UIColor per cell twice a frame did.
    private func colors(_ c: Cell) -> (fg: RGB, bg: RGB) {
        var fg = c.fg ?? Palette.defaultFG, bg = c.bg ?? Palette.defaultBG
        if c.mods.contains(.reversed) { swap(&fg, &bg) }
        return (fg, bg)
    }

    private var cgColors: [RGB: CGColor] = [:]
    private var dimColors: [RGB: CGColor] = [:]

    private func cgColor(_ rgb: RGB, dim: Bool = false) -> CGColor {
        // True-colour gradients would otherwise grow these for the view's life.
        if cgColors.count + dimColors.count > 4096 {
            cgColors.removeAll(keepingCapacity: true)
            dimColors.removeAll(keepingCapacity: true)
        }
        if dim {
            if let hit = dimColors[rgb] { return hit }
            let color = CGColor(srgbRed: rgb.r, green: rgb.g, blue: rgb.b, alpha: 0.55)
            dimColors[rgb] = color
            return color
        }
        if let hit = cgColors[rgb] { return hit }
        let color = CGColor(srgbRed: rgb.r, green: rgb.g, blue: rgb.b, alpha: 1)
        cgColors[rgb] = color
        return color
    }

    /// Glyph ids for ASCII, per face: 0..<128, 0 = not in the font.
    private lazy var asciiGlyphs = Self.glyphTable(font)
    private lazy var asciiBoldGlyphs = Self.glyphTable(boldFont)

    private static func glyphTable(_ font: UIFont) -> [CGGlyph] {
        var chars = (0 ..< 128).map { UniChar($0) }
        var glyphs = [CGGlyph](repeating: 0, count: 128)
        CTFontGetGlyphsForCharacters(font as CTFont, &chars, &glyphs, 128)
        return glyphs
    }

    private func drawRow(_ g: Grid, _ y: Int, _ ctx: CGContext) {
        let top = CGFloat(y) * cellH
        var x = 0
        while x < g.cols {
            let bg = colors(g[x, y]).bg
            var end = x + 1
            while end < g.cols, colors(g[end, y]).bg == bg { end += 1 }
            if bg != Palette.defaultBG {
                ctx.setFillColor(cgColor(bg))
                ctx.fill(CGRect(x: CGFloat(x) * cellW, y: top, width: CGFloat(end - x) * cellW, height: cellH))
            }
            x = end
        }
        // Glyphs: a run of plain ASCII with one style is one CTFontDrawGlyphs at the cells' own
        // positions (no attributed string, no line layout); anything else is drawn in its own
        // cell box.
        x = 0
        var glyphs: [CGGlyph] = []
        var positions: [CGPoint] = []
        while x < g.cols {
            let c = g[x, y]
            if c.skip || c.sym == " " || c.sym.isEmpty || c.mods.contains(.hidden) { x += 1; continue }
            if c.sym.utf8.count == 1, let ascii = c.sym.utf8.first, ascii < 128 {
                let bold = c.mods.contains(.bold)
                let table = bold ? asciiBoldGlyphs : asciiGlyphs
                glyphs.removeAll(keepingCapacity: true)
                positions.removeAll(keepingCapacity: true)
                var end = x
                while end < g.cols {
                    let n = g[end, y]
                    guard n.fg == c.fg, n.bg == c.bg, n.mods == c.mods, !n.skip,
                          n.sym.utf8.count == 1, let b = n.sym.utf8.first, b < 128 else { break }
                    if b != 0x20 {
                        glyphs.append(table[Int(b)])
                        positions.append(CGPoint(x: CGFloat(end) * cellW, y: 0))
                    }
                    end += 1
                }
                drawGlyphs(glyphs, positions, cell: c, from: x, to: end, top: top, ctx)
                x = end
            } else {
                let laid = laidOut(c)
                let w = CGFloat(CellWidth.of(c.sym)) * cellW
                let ox = CGFloat(x) * cellW + max(0, (w - laid.width) / 2)
                ctx.saveGState()
                ctx.textMatrix = textMatrix(italic: c.mods.contains(.italic))
                ctx.textPosition = CGPoint(x: ox, y: top + laid.ascent)
                CTLineDraw(laid.line, ctx)
                ctx.restoreGState()
                drawDecorations(c, from: x, to: x + max(1, CellWidth.of(c.sym)), top: top, ctx)
                x += 1
            }
        }
    }

    private struct LaidOutKey: Hashable {
        let sym: String
        let fg: RGB
        let mods: Mods
    }

    /// A non-ASCII cell (CJK, emoji, Nerd icon) laid out once per glyph and style; laying it
    /// out (and measuring it to centre it) on every draw was the bulk of a frame.
    private var laidOutCache: [LaidOutKey: (line: CTLine, width: CGFloat, ascent: CGFloat)] = [:]

    private func laidOut(_ c: Cell) -> (line: CTLine, width: CGFloat, ascent: CGFloat) {
        let key = LaidOutKey(sym: c.sym, fg: colors(c).fg, mods: c.mods)
        if let hit = laidOutCache[key] { return hit }
        let attributed = NSAttributedString(string: c.sym, attributes: attributes(c))
        let line = CTLineCreateWithAttributedString(attributed)
        let width = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
        // Same baseline as the ASCII runs: the base font's, not the fallback face's.
        let entry = (line: line, width: width, ascent: (c.mods.contains(.bold) ? boldFont : font).ascender)
        if laidOutCache.count > 2048 { laidOutCache.removeAll(keepingCapacity: true) }
        laidOutCache[key] = entry
        return entry
    }

    private func drawGlyphs(
        _ glyphs: [CGGlyph], _ positions: [CGPoint], cell c: Cell, from x0: Int, to x1: Int,
        top: CGFloat, _ ctx: CGContext,
    ) {
        let face = c.mods.contains(.bold) ? boldFont : font
        let color = cgColor(colors(c).fg, dim: c.mods.contains(.dim))
        let baseline = top + face.ascender
        if !glyphs.isEmpty {
            ctx.saveGState()
            ctx.setFillColor(color)
            ctx.textMatrix = textMatrix(italic: c.mods.contains(.italic))
            ctx.translateBy(x: 0, y: baseline)
            positions.withUnsafeBufferPointer { p in
                glyphs.withUnsafeBufferPointer { g in
                    CTFontDrawGlyphs(face as CTFont, g.baseAddress!, p.baseAddress!, glyphs.count, ctx)
                }
            }
            ctx.restoreGState()
        }
        drawDecorations(c, from: x0, to: x1, top: top, ctx)
    }

    /// UIKit's context is y-down; glyphs draw y-up. Italic is a shear (the bundled font has no
    /// italic face).
    private func textMatrix(italic: Bool) -> CGAffineTransform {
        let flip = CGAffineTransform(scaleX: 1, y: -1)
        return italic ? flip.concatenating(CGAffineTransform(a: 1, b: 0, c: -0.18, d: 1, tx: 0, ty: 0)) : flip
    }

    /// Underline and strike-through as rects: Core Text draws neither the way UIKit's string
    /// drawing did, for ASCII runs and single cells alike.
    private func drawDecorations(_ c: Cell, from x0: Int, to x1: Int, top: CGFloat, _ ctx: CGContext) {
        guard c.mods.contains(.underlined) || c.mods.contains(.crossedOut) else { return }
        let face = c.mods.contains(.bold) ? boldFont : font
        let color = cgColor(colors(c).fg, dim: c.mods.contains(.dim))
        let baseline = top + face.ascender
        let lineWidth = max(1, face.lineHeight / 16)
        if c.mods.contains(.underlined) {
            ctx.setFillColor(color)
            ctx.fill(CGRect(x: CGFloat(x0) * cellW, y: baseline + lineWidth, width: CGFloat(x1 - x0) * cellW, height: lineWidth))
        }
        if c.mods.contains(.crossedOut) {
            ctx.setFillColor(color)
            ctx.fill(CGRect(x: CGFloat(x0) * cellW, y: baseline - face.xHeight / 2, width: CGFloat(x1 - x0) * cellW, height: lineWidth))
        }
    }

    /// Font and colour only: italic, underline and strike-through are drawn by `drawRow`.
    private func attributes(_ c: Cell) -> [NSAttributedString.Key: Any] {
        [
            .font: c.mods.contains(.bold) ? boldFont : font,
            .foregroundColor: UIColor(rgb: colors(c).fg).withAlphaComponent(c.mods.contains(.dim) ? 0.55 : 1),
        ]
    }

    private func drawCursor(_ g: Grid, _ ctx: CGContext) {
        guard let (cx, cy) = g.cursor, cx < g.cols, cy < g.rows else { return }
        let origin = CGPoint(x: CGFloat(cx) * cellW, y: CGFloat(cy) * cellH)
        if !marked.isEmpty {
            let a: [NSAttributedString.Key: Any] = [
                .font: font, .foregroundColor: UIColor(rgb: Palette.defaultFG),
                .backgroundColor: UIColor(rgb: RGB(hex: 0x45475A)),
                .underlineStyle: NSUnderlineStyle.single.rawValue,
            ]
            (marked as NSString).draw(at: origin, withAttributes: a)
            return
        }
        guard g.cursorVisible else { return }
        ctx.setFillColor(UIColor(rgb: Palette.defaultFG).withAlphaComponent(input.isFirstResponder ? 0.75 : 0.3).cgColor)
        ctx.fill(CGRect(x: origin.x, y: origin.y, width: cellW, height: cellH))
    }

    /// The screen as text, built when assistive tech (or a UI test) asks — not every frame.
    override var accessibilityValue: String? {
        get {
            guard let g = session?.grid else { return nil }
            return (0..<g.rows).map { g.text(row: $0) }.joined(separator: "\n")
        }
        set {}
    }

    // MARK: touch

    private func cell(at p: CGPoint) -> (x: Int, y: Int)? {
        guard let g = session?.grid, g.cols > 0 else { return nil }
        let x = Int(p.x / cellW), y = Int(p.y / cellH)
        guard x >= 0, y >= 0, x < g.cols, y < g.rows else { return nil }
        return (x, y)
    }

    @objc private func onTap(_ r: UITapGestureRecognizer) {
        stopInertia()
        if !input.isFirstResponder { input.becomeFirstResponder() }
        guard let (x, y) = cell(at: r.location(in: self)) else { return }
        input.commitComposition()
        session?.send(.mouse(x: x, y: y, kind: .click))
        session?.send(.mouse(x: x, y: y, kind: .up))
    }

    @objc private func onLongPress(_ r: UILongPressGestureRecognizer) {
        guard r.state == .began else { return }
        stopInertia()
        editMenu.presentEditMenu(with: UIEditMenuConfiguration(identifier: nil, sourcePoint: r.location(in: self)))
    }

    // The menu's Paste is the system command, resolved from this view: as the system's own it
    // reads the pasteboard without a paste prompt, and the input view sends it as a paste.
    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        action == #selector(paste(_:)) ? UIPasteboard.general.hasStrings : super.canPerformAction(action, withSender: sender)
    }

    override func paste(_ sender: Any?) { input.paste(sender) }

    @objc private func onPan(_ r: UIPanGestureRecognizer) {
        switch r.state {
        case .began:
            stopInertia()
            panCarry = 0
            if let c = cell(at: r.location(in: self)) { panCell = c }
        case .changed:
            let dy = r.translation(in: self).y
            r.setTranslation(.zero, in: self)
            wheel(dy)
        case .ended:
            velocity = r.velocity(in: self).y
            startInertia()
        default:
            break
        }
    }

    /// Translate finger movement into wheel events, one per row, carrying the remainder.
    /// Finger DOWN = content follows it = scroll back into history (wheel up).
    private func wheel(_ dy: CGFloat) {
        panCarry += dy
        while abs(panCarry) >= cellH {
            let up = panCarry > 0
            session?.send(.mouse(x: panCell.x, y: panCell.y, kind: up ? .scrollUp : .scrollDown))
            panCarry += up ? -cellH : cellH
        }
    }

    private func startInertia() {
        guard abs(velocity) > 200 else { return }
        let l = CADisplayLink(target: self, selector: #selector(inertiaStep(_:)))
        l.add(to: .main, forMode: .common)
        inertiaLink = l
    }

    @objc private func inertiaStep(_ l: CADisplayLink) {
        let dt = CGFloat(l.targetTimestamp - l.timestamp)
        wheel(velocity * dt)
        velocity *= pow(0.135, dt) // ~UIScrollView normal deceleration
        if abs(velocity) < 60 { stopInertia() }
    }

    func stopInertia() {
        inertiaLink?.invalidate()
        inertiaLink = nil
        velocity = 0
    }
}

extension TerminalView: UIEditMenuInteractionDelegate {
    func editMenuInteraction(_ interaction: UIEditMenuInteraction, menuFor configuration: UIEditMenuConfiguration,
                             suggestedActions: [UIMenuElement]) -> UIMenu? {
        // Only Paste: Select All, Look Up and the rest act on the invisible input view.
        // An empty menu, not nil, when there is nothing to paste: nil means the system's default.
        UIMenu(children: Self.find(#selector(UIResponderStandardEditActions.paste(_:)), in: suggestedActions).map { [$0] } ?? [])
    }

    private static func find(_ action: Selector, in elements: [UIMenuElement]) -> UIMenuElement? {
        for e in elements {
            if let c = e as? UICommand, c.action == action { return c }
            if let m = e as? UIMenu, let c = find(action, in: m.children) { return c }
        }
        return nil
    }
}

extension UIColor {
    convenience init(rgb: RGB) { self.init(red: rgb.r, green: rgb.g, blue: rgb.b, alpha: 1) }
}

/// Owns the terminal view for SwiftUI, and is what the drawers call to settle input before
/// they navigate.
@MainActor
final class TerminalController: ObservableObject {
    let view = TerminalView()

    init(session: MuxSession) {
        view.session = session
        view.input.session = session
        session.onGrid = { [weak view] in view?.gridChanged() }
        session.onBell = { UIImpactFeedbackGenerator(style: .light).impactOccurred() }
        session.onCopy = { UIPasteboard.general.string = $0 }
    }

    /// Commit the IME's syllable and stop scrolling BEFORE anything that changes where input
    /// goes: a drawer tap must not deliver a half-typed syllable to the new pane.
    func settleInput() {
        view.input.commitComposition()
        view.stopInertia()
    }

    func focus() { view.input.becomeFirstResponder() }
}

struct TerminalRepresentable: UIViewRepresentable {
    let controller: TerminalController
    func makeUIView(context: Context) -> TerminalView { controller.view }
    func updateUIView(_ v: TerminalView, context: Context) {}
}
