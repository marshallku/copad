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

    // Wheel inertia.
    private var panCarry: CGFloat = 0
    private var velocity: CGFloat = 0
    private var inertiaLink: CADisplayLink?
    private var panCell = (x: 0, y: 0)

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

    /// The session applied a frame.
    func gridChanged() { dirty = true }

    @objc private func tick() {
        guard dirty else { return }
        dirty = false
        setNeedsDisplay()
        updateAccessibility()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let cols = Int(bounds.width / cellW), rows = Int(bounds.height / cellH)
        session?.resize(cols: cols, rows: rows)
    }

    // MARK: drawing

    override func draw(_ rect: CGRect) {
        guard let ctx = UIGraphicsGetCurrentContext(), let g = session?.grid else { return }
        ctx.setFillColor(UIColor(rgb: Palette.defaultBG).cgColor)
        ctx.fill(bounds)
        guard g.cols > 0 else { return }
        let firstRow = max(0, Int(rect.minY / cellH)), lastRow = min(g.rows - 1, Int(rect.maxY / cellH))
        guard firstRow <= lastRow else { return }
        for y in firstRow...lastRow {
            drawRow(g, y, ctx)
        }
        drawCursor(g, ctx)
    }

    private func colors(_ c: Cell) -> (UIColor, UIColor) {
        var fg = c.fg ?? Palette.defaultFG, bg = c.bg ?? Palette.defaultBG
        if c.mods.contains(.reversed) { swap(&fg, &bg) }
        var f = UIColor(rgb: fg)
        if c.mods.contains(.dim) { f = f.withAlphaComponent(0.55) }
        return (f, UIColor(rgb: bg))
    }

    private func drawRow(_ g: Grid, _ y: Int, _ ctx: CGContext) {
        let top = CGFloat(y) * cellH
        // Backgrounds first, merged into runs.
        var x = 0
        while x < g.cols {
            let bg = colors(g[x, y]).1
            var end = x + 1
            while end < g.cols, colors(g[end, y]).1 == bg { end += 1 }
            if bg != UIColor(rgb: Palette.defaultBG) {
                ctx.setFillColor(bg.cgColor)
                ctx.fill(CGRect(x: CGFloat(x) * cellW, y: top, width: CGFloat(end - x) * cellW, height: cellH))
            }
            x = end
        }
        // Glyphs: a run of plain ASCII with one style is one draw (monospace keeps it aligned);
        // anything else is drawn in its own cell box.
        x = 0
        while x < g.cols {
            let c = g[x, y]
            if c.skip || c.sym == " " || c.sym.isEmpty || c.mods.contains(.hidden) { x += 1; continue }
            let attrs = attributes(c)
            if c.sym.utf8.count == 1 {
                var s = c.sym, end = x + 1
                while end < g.cols {
                    let n = g[end, y]
                    guard n.sym.utf8.count == 1, !n.skip, n.fg == c.fg, n.bg == c.bg, n.mods == c.mods else { break }
                    s += n.sym; end += 1
                }
                (s as NSString).draw(at: CGPoint(x: CGFloat(x) * cellW, y: top), withAttributes: attrs)
                x = end
            } else {
                let w = CGFloat(CellWidth.of(c.sym)) * cellW
                let size = (c.sym as NSString).size(withAttributes: attrs)
                let ox = CGFloat(x) * cellW + max(0, (w - size.width) / 2)
                (c.sym as NSString).draw(at: CGPoint(x: ox, y: top), withAttributes: attrs)
                x += 1
            }
        }
    }

    private func attributes(_ c: Cell) -> [NSAttributedString.Key: Any] {
        var a: [NSAttributedString.Key: Any] = [
            .font: c.mods.contains(.bold) ? boldFont : font,
            .foregroundColor: colors(c).0,
        ]
        if c.mods.contains(.underlined) { a[.underlineStyle] = NSUnderlineStyle.single.rawValue }
        if c.mods.contains(.crossedOut) { a[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }
        if c.mods.contains(.italic) {
            a[.obliqueness] = 0.18
        }
        return a
    }

    private func drawCursor(_ g: Grid, _ ctx: CGContext) {
        guard let (cx, cy) = g.cursor, cx < g.cols, cy < g.rows else { return }
        let origin = CGPoint(x: CGFloat(cx) * cellW, y: CGFloat(cy) * cellH)
        if !marked.isEmpty {
            // The IME's in-progress syllable, drawn where it will land.
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

    private func updateAccessibility() {
        guard let g = session?.grid else { return }
        accessibilityValue = (0..<g.rows).map { g.text(row: $0) }.joined(separator: "\n")
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
        // A tap is a click comux already understands: focus a pane, switch a tab chip, …
        input.commitComposition()
        session?.send(.mouse(x: x, y: y, kind: .click))
        session?.send(.mouse(x: x, y: y, kind: .up))
    }

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

    /// Cancel inertia — on navigation, layout changes, backgrounding.
    func stopInertia() {
        inertiaLink?.invalidate()
        inertiaLink = nil
        velocity = 0
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
    /// goes (codex C2): a drawer tap must not deliver a half-typed syllable to the new pane.
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
