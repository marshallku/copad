import AppKit
import CNesttyTerm
import Foundation

/// Phase 3.2 — `nestty-term` (alacritty_terminal-backed) pane with
/// CoreText cell rendering. Conforms to `NesttyPanel` so PaneManager
/// / SplitNode / socket commands treat it identically to
/// `TerminalViewController`. See
/// docs/macos-renderer-migration-plan.md for the staged scope.
///
/// What ships in this slice:
///
/// - PTY spawn (`NesttyTermFFI.Handle`) — already lands in 3.1
/// - CoreText cell draw — snapshot → row-by-row attributed strings
///   built from each run's borrowed utf8 + CTLine + CTLineDraw
/// - Periodic refresh (Timer at ~30 Hz) — Phase 3.6 will replace with
///   damage-tracked CADisplayLink
/// - Keyboard input — printable chars + the common control bytes
///   shells need to function (Return, Backspace, Tab, Esc, arrows)
///
/// Deferred:
///
/// - Cursor render (Phase 3.3)
/// - ANSI palette + inverse video (Phase 3.4)
/// - Image background + Zed-pattern materialize (Phase 3.5)
/// - Damage tracking + selection + IME + ligatures + automation
///   parity (Phases 3.6, 4, 5, 6, 7)
@MainActor
final class AlacrittyTerminalViewController: NSViewController, NesttyPanel {
    let panelID: String = UUID().uuidString
    private(set) var currentTitle: String = "Terminal (alacritty)"

    private let config: NesttyConfig
    private var theme: NesttyTheme
    private let initialCwd: String?
    private let initialInput: String?

    private var termHandle: NesttyTermFFI.Handle?
    private var renderView: AlacrittyRenderView?
    private var shellStarted = false

    init(config: NesttyConfig, theme: NesttyTheme, cwd: String? = nil, initialInput: String? = nil) {
        self.config = config
        self.theme = theme
        initialCwd = cwd
        self.initialInput = initialInput
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func loadView() {
        let render = AlacrittyRenderView(theme: theme, font: resolveFont(family: config.fontFamily, size: CGFloat(config.fontSize)))
        render.translatesAutoresizingMaskIntoConstraints = false
        renderView = render
        view = render
    }

    override func viewDidLayout() {
        super.viewDidLayout()
        // Compute terminal grid size from view bounds + cell metrics so
        // the shell sees a winsize matching what we'll actually draw.
        guard let render = renderView else { return }
        let (cols, rows) = render.computeGrid()
        termHandle?.resize(cols: cols, rows: rows)
    }

    func startIfNeeded() {
        guard !shellStarted else { return }
        shellStarted = true
        let (cols, rows) = renderView?.computeGrid() ?? (80, 24)
        termHandle = NesttyTermFFI.Handle(
            cols: cols,
            rows: rows,
            shell: initialCwd != nil ? config.shell : nil,
            cwd: initialCwd,
        )
        if let initialInput {
            termHandle?.input(Array(initialInput.utf8))
        }
        renderView?.bind(handle: termHandle)
        view.window?.makeFirstResponder(view)
    }

    // MARK: - NesttyPanel — background (Phase 3.5 wires the real impl)

    func applyBackground(path _: String, tint _: Double, opacity _: Double) {}
    func clearBackground() {}
    func setTint(_: Double) {}

    // MARK: - Font

    /// Mirrors `TerminalViewController.resolveFont` — PostScript name
    /// → family lookup → case-insensitive fallback → monospaced
    /// system. Trimmed to the cases we need for the alacritty path.
    private func resolveFont(family: String, size: CGFloat) -> NSFont {
        if let font = NSFont(name: family, size: size) { return font }
        let manager = NSFontManager.shared
        if let font = manager.font(withFamily: family, traits: [], weight: 5, size: size) {
            return font
        }
        let lower = family.lowercased()
        for fam in manager.availableFontFamilies where fam.lowercased() == lower {
            if let font = manager.font(withFamily: fam, traits: [], weight: 5, size: size) {
                return font
            }
        }
        return .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

// MARK: - Render view

/// Custom NSView that draws the terminal grid via CoreText. Snapshots
/// are taken under the `nestty-term` handle's `FairMutex`; the lock is
/// dropped before `setNeedsDisplay` so AppKit's redraw doesn't block
/// the PTY reader thread.
///
/// Coordinate system is **flipped** (origin top-left, y down) so row 0
/// renders at the top of the view — matching the terminal convention
/// and keeping cell math straightforward.
@MainActor
private final class AlacrittyRenderView: NSView, @preconcurrency NSTextInputClient {
    private let theme: NesttyTheme
    private var font: NSFont
    private(set) var cellWidth: CGFloat = 0
    private(set) var cellHeight: CGFloat = 0
    private var ascent: CGFloat = 0

    private weak var termHandle: NesttyTermFFI.Handle?
    /// nonisolated(unsafe) so deinit (Swift 6 nonisolated) can
    /// invalidate the timer without crossing the main-actor barrier.
    /// Same RAII pattern used by NesttyTermFFI.Handle/Snapshot.
    private nonisolated(unsafe) var refreshTimer: Timer?

    /// Cached snapshot for the most recent paint. Phase 3.6 will
    /// switch to damage-tracked partial repaints; for now the whole
    /// view repaints when the timer fires.
    private var snapshotCache: NesttyTermFFI.Snapshot?

    init(theme: NesttyTheme, font: NSFont) {
        self.theme = theme
        self.font = font
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = theme.background.nsColor.cgColor
        recomputeCellMetrics()
        startRefreshTimer()
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    deinit {
        refreshTimer?.invalidate()
    }

    override var isFlipped: Bool {
        true
    }

    override var acceptsFirstResponder: Bool {
        true
    }

    func bind(handle: NesttyTermFFI.Handle?) {
        termHandle = handle
    }

    func computeGrid() -> (cols: UInt16, rows: UInt16) {
        let w = max(1, Int(bounds.width / cellWidth))
        let h = max(1, Int(bounds.height / cellHeight))
        return (UInt16(min(w, Int(UInt16.max))), UInt16(min(h, Int(UInt16.max))))
    }

    private func recomputeCellMetrics() {
        // Monospaced advance: measure the wide-but-canonical "M".
        // Falls back to font.maximumAdvancement if measurement fails
        // (shouldn't on a real monospaced face).
        let attrs: [NSAttributedString.Key: Any] = [.font: font]
        let m = NSAttributedString(string: "M", attributes: attrs)
        cellWidth = ceil(m.size().width)
        if cellWidth <= 0 { cellWidth = ceil(font.maximumAdvancement.width) }
        ascent = ceil(font.ascender)
        let descent = ceil(abs(font.descender))
        let leading = ceil(font.leading)
        cellHeight = ascent + descent + leading
        if cellHeight <= 0 { cellHeight = 16 }
    }

    private func startRefreshTimer() {
        refreshTimer?.invalidate()
        // ~30 Hz. CADisplayLink + damage tracking is Phase 3.6; for
        // 3.2 a Timer is sufficient and avoids the display-link
        // run-loop integration tax up front.
        refreshTimer = Timer.scheduledTimer(withTimeInterval: 1.0 / 30.0, repeats: true) { [weak self] _ in
            // Timer fires on the main runloop (scheduledTimer's
            // default). Assume-isolated lets us call the @MainActor
            // tick() without a hop, matching the actual thread.
            MainActor.assumeIsolated { self?.tick() }
        }
    }

    private func tick() {
        guard let handle = termHandle else { return }
        // Take a fresh snapshot and trigger a redraw. The snapshot is
        // a copy of the grid (Rust-side `Box`); holding it across the
        // draw is cheap.
        let snap = handle.snapshot()
        snapshotCache = snap
        needsDisplay = true
    }

    override func draw(_: NSRect) {
        guard let snap = snapshotCache,
              let ctx = NSGraphicsContext.current?.cgContext
        else { return }

        // Fill the whole bounds with theme background. Phase 3.5
        // adds the per-cell materialize that interacts with image
        // backgrounds; for now a single fill is correct.
        ctx.setFillColor(theme.background.nsColor.cgColor)
        ctx.fill(bounds)

        // CTLineDraw uses CoreGraphics-native y-up glyph orientation.
        // Our view is `isFlipped = true` (so row 0 is at the top
        // visually) — without this textMatrix flip the glyphs render
        // upside-down + mirrored against the flipped CTM. Save/restore
        // the prior state so we don't leak the flip into non-text
        // drawing later.
        ctx.saveGState()
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        defer { ctx.restoreGState() }

        let snapRows = snap.rows
        let textColor = theme.foreground.nsColor
        for row in 0 ..< snapRows {
            let runs = snap.rowRuns(row)
            let utf8 = snap.rowUtf8(row)
            guard runs.count > 0, utf8.count > 0 else { continue }
            drawRow(row: row, runs: runs, utf8: utf8, textColor: textColor, ctx: ctx)
        }
    }

    private func drawRow(
        row: UInt16,
        runs: UnsafeBufferPointer<NesttyRun>,
        utf8: UnsafeBufferPointer<UInt8>,
        textColor: NSColor,
        ctx: CGContext,
    ) {
        // Baseline in flipped coords: top of row + ascent.
        let baselineY = CGFloat(row) * cellHeight + ascent
        for i in 0 ..< runs.count {
            let run = runs[i]
            let len = Int(run.utf8_len)
            let offset = Int(run.utf8_offset)
            guard offset + len <= utf8.count else { continue }
            guard
                let str = String(bytes: UnsafeBufferPointer(rebasing: utf8[offset ..< offset + len]), encoding: .utf8),
                !str.isEmpty
            else { continue }

            let attrs: [NSAttributedString.Key: Any] = [
                .font: font,
                .foregroundColor: textColor,
            ]
            let attr = NSAttributedString(string: str, attributes: attrs)
            let line = CTLineCreateWithAttributedString(attr)
            let x = CGFloat(run.start_col) * cellWidth
            ctx.textPosition = CGPoint(x: x, y: baselineY)
            CTLineDraw(line, ctx)
        }
    }

    // MARK: - Keyboard

    /// Route keyDown through `interpretKeyEvents` so the system IME
    /// (Korean 2-Set, Japanese, …) sees the keystrokes. Without this,
    /// IME-active keystrokes don't deliver committed text back via
    /// `event.characters` and Korean/Japanese input silently drops.
    /// Preedit-text rendering during composition is still Phase 6;
    /// this slice just lets COMMITTED IME text flow into the PTY.
    ///
    /// Ctrl-letter combinations bypass IME entirely because shells +
    /// TUIs rely on them as raw control bytes (Ctrl+C = 0x03 → SIGINT,
    /// Ctrl+D = 0x04 → EOF). Cmd-modified keys go to the responder
    /// chain (menu shortcuts, clipboard) by calling super.
    override func keyDown(with event: NSEvent) {
        let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        if mods.contains(.command) {
            super.keyDown(with: event)
            return
        }
        if mods == .control, let bytes = controlBytes(for: event) {
            termHandle?.input(bytes)
            return
        }
        interpretKeyEvents([event])
    }

    /// Map Ctrl+letter / Ctrl+@ / Ctrl+[ / Ctrl+\ / Ctrl+] / Ctrl+^
    /// / Ctrl+_ / Ctrl+Space to their canonical control bytes
    /// (0x00–0x1f, 0x7f). Returns nil for combinations not in the
    /// standard ASCII control set so the responder chain can handle
    /// them.
    private func controlBytes(for event: NSEvent) -> [UInt8]? {
        guard
            let chars = event.charactersIgnoringModifiers?.lowercased(),
            let scalar = chars.unicodeScalars.first
        else { return nil }
        let v = scalar.value
        // a-z → 0x01-0x1a
        if (0x61 ... 0x7A).contains(v) { return [UInt8(v - 0x60)] }
        switch v {
        case 0x20: return [0x00] // Ctrl+Space → NUL
        case 0x40: return [0x00] // Ctrl+@ → NUL
        case 0x5B: return [0x1B] // Ctrl+[ → ESC
        case 0x5C: return [0x1C] // Ctrl+\
        case 0x5D: return [0x1D] // Ctrl+]
        case 0x5E: return [0x1E] // Ctrl+^
        case 0x5F: return [0x1F] // Ctrl+_
        case 0x3F: return [0x7F] // Ctrl+? → DEL
        default: return nil
        }
    }

    // NSTextInputClient — IME routes commits through `insertText` and
    // special keys through `doCommand`. Phase 6 will flesh out the
    // marked-text path so preedit characters render in-cell during
    // composition.

    func insertText(_ string: Any, replacementRange _: NSRange) {
        let text: String
        if let s = string as? String { text = s }
        else if let a = string as? NSAttributedString { text = a.string }
        else { return }
        guard !text.isEmpty else { return }
        termHandle?.input(Array(text.utf8))
    }

    override func doCommand(by selector: Selector) {
        if let bytes = commandBytes(for: selector) {
            termHandle?.input(bytes)
        }
        // Unmapped selectors fall on the floor — better than calling
        // super which would try to interpret them as text editing on
        // a view that has no document model.
    }

    /// Selectors AppKit's text-input system synthesizes for keys that
    /// aren't plain printable characters. Mapped to the byte sequences
    /// a VT100-ish terminal expects.
    private func commandBytes(for selector: Selector) -> [UInt8]? {
        switch selector {
        case #selector(NSStandardKeyBindingResponding.insertNewline(_:)):
            [0x0D]
        case #selector(NSStandardKeyBindingResponding.insertTab(_:)):
            [0x09]
        case #selector(NSStandardKeyBindingResponding.deleteBackward(_:)):
            [0x7F]
        case #selector(NSStandardKeyBindingResponding.deleteForward(_:)):
            [0x1B, 0x5B, 0x33, 0x7E] // ESC [ 3 ~
        case #selector(NSStandardKeyBindingResponding.cancelOperation(_:)):
            [0x1B]
        case #selector(NSStandardKeyBindingResponding.moveLeft(_:)):
            [0x1B, 0x5B, 0x44]
        case #selector(NSStandardKeyBindingResponding.moveRight(_:)):
            [0x1B, 0x5B, 0x43]
        case #selector(NSStandardKeyBindingResponding.moveUp(_:)):
            [0x1B, 0x5B, 0x41]
        case #selector(NSStandardKeyBindingResponding.moveDown(_:)):
            [0x1B, 0x5B, 0x42]
        default:
            nil
        }
    }

    // Stubs — Phase 6 implements preedit rendering and candidate
    // window positioning via these methods.

    func setMarkedText(_: Any, selectedRange _: NSRange, replacementRange _: NSRange) {}
    func unmarkText() {}
    func selectedRange() -> NSRange {
        NSRange(location: NSNotFound, length: 0)
    }

    func markedRange() -> NSRange {
        NSRange(location: NSNotFound, length: 0)
    }

    func hasMarkedText() -> Bool {
        false
    }

    func attributedSubstring(forProposedRange _: NSRange, actualRange _: NSRangePointer?) -> NSAttributedString? {
        nil
    }

    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        []
    }

    func firstRect(forCharacterRange _: NSRange, actualRange _: NSRangePointer?) -> NSRect {
        .zero
    }

    func characterIndex(for _: NSPoint) -> Int {
        NSNotFound
    }
}
