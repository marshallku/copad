import AppKit
import CoreText

/// IME composition overlay for GPU-mode panes (plan D2). The Metal
/// layer can't host the CoreText preedit drawing, so this transparent
/// child view sits above it and paints the marked text with the same
/// visual contract as the CPU painter's `paintMarkedText`: opaque
/// theme.background fill under the composition (legibility against
/// arbitrary content), accent single underline across the whole
/// composition, double+thick underline on the IME-highlighted
/// sub-range.
///
/// State flows one way: `AlacrittyRenderView` pushes marked-text /
/// cursor / metric changes via `update*` setters; this view never
/// reads back. Hit testing is disabled so every mouse event lands on
/// the render view beneath.
@MainActor
final class IMEPreeditOverlayView: NSView {
    private var theme: CopadTheme
    private var font: NSFont
    private var cellWidth: CGFloat = 0
    private var cellHeight: CGFloat = 0
    private var ascent: CGFloat = 0

    private var markedText: String?
    private var markedSelectedRange = NSRange(location: 0, length: 0)
    private var cursorRow = 0
    private var cursorCol = 0

    /// Cascade-fallback resolver borrowed from the parent render view
    /// so CJK preedit clusters reuse the same fallback cache.
    var resolveFont: ((String, NSFont) -> NSFont)?

    init(theme: CopadTheme, font: NSFont) {
        self.theme = theme
        self.font = font
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = NSColor.clear.cgColor
        isHidden = true
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    override var isFlipped: Bool {
        true
    }

    /// Mouse-transparent: the overlay only ever paints.
    override func hitTest(_: NSPoint) -> NSView? {
        nil
    }

    func updateTheme(_ newTheme: CopadTheme) {
        theme = newTheme
        needsDisplay = true
    }

    func updateFont(_ newFont: NSFont, cellWidth: CGFloat, cellHeight: CGFloat, ascent: CGFloat) {
        font = newFont
        self.cellWidth = cellWidth
        self.cellHeight = cellHeight
        self.ascent = ascent
        needsDisplay = true
    }

    /// Composition state push. Hides the overlay entirely between
    /// compositions so the window server composites nothing while the
    /// user isn't typing through an IME.
    func updateComposition(markedText: String?, selectedRange: NSRange, cursorRow: Int, cursorCol: Int) {
        self.markedText = markedText
        markedSelectedRange = selectedRange
        self.cursorRow = cursorRow
        self.cursorCol = cursorCol
        isHidden = markedText == nil || markedText?.isEmpty == true
        needsDisplay = true
    }

    override func draw(_: NSRect) {
        guard let marked = markedText, !marked.isEmpty,
              cellWidth > 0, cellHeight > 0,
              let ctx = NSGraphicsContext.current?.cgContext
        else { return }

        // Same textMatrix flip the render view's draw(_:) applies —
        // this view is flipped too, and CTLineDraw expects y-up glyph
        // orientation.
        ctx.saveGState()
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        defer { ctx.restoreGState() }

        let baseAttrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor(cgColor: theme.foreground.nsColor.cgColor) ?? .white,
            .underlineStyle: NSUnderlineStyle.single.rawValue,
            .underlineColor: NSColor(cgColor: theme.accent.nsColor.cgColor) ?? .yellow,
        ]
        let attr = NSMutableAttributedString(string: marked, attributes: baseAttrs)
        // Preedit is overwhelmingly CJK — walk composed character
        // sequences and swap `.font` per cluster when cascade picks a
        // different face (same loop as the CPU painter).
        var loc = 0
        marked.enumerateSubstrings(
            in: marked.startIndex ..< marked.endIndex,
            options: .byComposedCharacterSequences,
        ) { cluster, _, _, _ in
            guard let cluster else { return }
            let len = (cluster as NSString).length
            if let resolved = self.resolveFont?(cluster, self.font), resolved !== self.font {
                attr.addAttribute(.font, value: resolved, range: NSRange(location: loc, length: len))
            }
            loc += len
        }
        if markedSelectedRange.length > 0,
           markedSelectedRange.location + markedSelectedRange.length <= (marked as NSString).length
        {
            attr.addAttribute(
                .underlineStyle,
                value: NSUnderlineStyle([.double, .thick]).rawValue,
                range: markedSelectedRange,
            )
        }

        let line = CTLineCreateWithAttributedString(attr)
        var ascentT: CGFloat = 0
        var descentT: CGFloat = 0
        var leadingT: CGFloat = 0
        let width = CGFloat(CTLineGetTypographicBounds(line, &ascentT, &descentT, &leadingT))
        let cellsCovered = max(1, Int(ceil(width / cellWidth)))
        let pxWidth = CGFloat(cellsCovered) * cellWidth

        let x = CGFloat(cursorCol) * cellWidth
        let y = CGFloat(cursorRow) * cellHeight
        ctx.setFillColor(theme.background.nsColor.cgColor)
        ctx.fill(CGRect(x: x, y: y, width: pxWidth, height: cellHeight))

        ctx.textPosition = CGPoint(x: x, y: y + ascent)
        CTLineDraw(line, ctx)
    }
}
