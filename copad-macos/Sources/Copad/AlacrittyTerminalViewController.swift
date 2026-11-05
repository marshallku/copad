import AppKit
import CCopadTerm
import CopadCore
import Foundation
import Metal
import QuartzCore

/// `copad-term` (alacritty_terminal-backed) pane with CoreText cell rendering. Conforms to
/// `CopadPanel` so PaneManager, SplitNode and socket commands treat it like any other panel.
/// See docs/macos-renderer-migration-plan.md.
@MainActor
final class AlacrittyTerminalViewController: NSViewController, CopadPanel, Zoomable, TerminalCapable {
    /// Stable pane identity; a persisted id is injected on session restore (decision #61).
    let panelID: String
    private(set) var currentTitle: String = "Terminal (alacritty)"
    /// For an unavailable-plugin placeholder (decision #61 C5): the plugin name, so the snapshot
    /// re-captures it as a plugin pane until the plugin is reinstalled.
    var unavailablePluginRef: String?
    /// `tab.rename` title override; while set, OSC 0/2 title updates from the program are ignored.
    private(set) var customTitle: String?

    /// Set by `PaneManager.assignEventBus`; used to publish `terminal.output` on keyboard/paste
    /// input, like `copad-linux/src/tabs.rs` `connect_commit`. `nil` drops events. The setter
    /// forwards to the render view, where the input call sites live.
    weak var eventBus: EventBus? {
        didSet { renderView?.eventBus = eventBus }
    }

    /// Set by `PaneManager.wirePanel` so the pane closes when its PTY child exits (gated on
    /// `[terminal] close_on_exit`). Passes `self` so PaneManager can find the leaf.
    var onChildExited: ((any CopadPanel) -> Void)?

    /// Latest cwd reported by the in-shell `copad-cwd` hook (`coctl call panel.report_cwd`),
    /// preferred over `proc_pidinfo` because it needs no entitlements.
    private(set) var reportedCwd: String?

    /// Tracked cwd, by priority: shell-reported, then `proc_pidinfo` (EPERM on un-entitled dev
    /// builds), then spawn-time `initialCwd`. Mirrors Linux's VTE + `/proc/<pid>/cwd` + last_cwd.
    var currentCwd: String? {
        reportedCwd ?? termHandle?.childCwd() ?? initialCwd
    }

    /// Called from the `panel.report_cwd` handler when the in-shell hook reports a cwd.
    func setReportedCwd(_ cwd: String) {
        reportedCwd = cwd
    }

    private let config: CopadConfig
    private var theme: CopadTheme
    private let initialCwd: String?
    private let initialInput: String?

    private var termHandle: CopadTermFFI.Handle?
    private var renderView: AlacrittyRenderView?
    private var shellStarted = false
    private var findBar: FindBar?

    /// Active font size for Cmd+= / Cmd+- / Cmd+0 zoom, separate from `configFontSize` so a zoom
    /// survives a config hot-reload.
    private var currentFontSize: CGFloat
    private var configFontSize: CGFloat
    private var currentFontFamily: String

    /// Focus target for `panel.focusTarget`: the render view, not the layout container.
    var focusTarget: NSView {
        renderView ?? view
    }

    init(config: CopadConfig, theme: CopadTheme, cwd: String? = nil, initialInput: String? = nil, restoreID: String? = nil) {
        self.panelID = restoreID ?? UUID().uuidString
        self.config = config
        self.theme = theme
        // Fall back to $HOME so Finder/Dock launches (launchd's cwd) do not land in `/`.
        initialCwd = cwd ?? NSHomeDirectory()
        self.initialInput = initialInput
        let base = CGFloat(config.fontSize)
        configFontSize = base
        currentFontSize = base
        currentFontFamily = config.fontFamily
        windowOpacity = config.windowOpacity
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    /// container (plain NSView) holding renderView (AlacrittyRenderView). The background image and
    /// tint live in `TabViewController.contentArea` so splits share one image;
    /// `setImageBackgroundActive` keeps default-bg cells transparent for it.
    ///
    /// Focus: the container is only a layout host and does not accept first responder, so focusing
    /// `panel.view` is a silent no-op. The render view takes focus via `startIfNeeded`, mouse
    /// clicks (`mouseDown`) and the tab-switch path through PaneManager.
    override func loadView() {
        let frame = NSRect(x: 0, y: 0, width: 1200, height: 800)
        let container = NSView(frame: frame)
        container.wantsLayer = true

        // GPU mode resolves before the view exists: the backing-layer class is fixed at init, so
        // a missing Metal device (VM, headless) must fall back to CoreText here.
        var gpuDevice: MTLDevice?
        if config.rendererGPU {
            gpuDevice = MTLCreateSystemDefaultDevice()
            if gpuDevice == nil {
                FileHandle.standardError.write(
                    Data("[copad] [renderer] gpu = true but no Metal device — using CoreText painter\n".utf8),
                )
            }
        }

        let render = AlacrittyRenderView(
            theme: theme,
            font: resolveFont(family: config.fontFamily, size: CGFloat(config.fontSize)),
            fontThicken: config.fontThicken,
            alphaBlending: config.alphaBlending,
            transparentDefaultBg: config.transparentDefaultBg,
            windowOpacity: config.windowOpacity,
            osc52Policy: config.osc52,
            optionAsAlt: config.optionAsAlt,
            forceMetaKeys: config.forceMetaKeys,
            gpuDevice: gpuDevice,
        )
        render.frame = container.bounds
        render.autoresizingMask = [.width, .height]
        container.addSubview(render)
        // Bind the panel identity before input handling starts so `terminal.output` events carry
        // the right panel_id from the first keystroke. `eventBus` arrives later via the VC didSet.
        render.panelID = panelID
        render.eventBus = eventBus
        render.onChildExited = { [weak self] in
            guard let self else { return }
            onChildExited?(self)
        }
        renderView = render

        // Find bar, hidden by default; Cmd+F toggles it, and it handles its own next/prev/close.
        let bar = FindBar(theme: theme) { [weak self] action in
            self?.handleFindAction(action)
        }
        bar.isHidden = true
        // Auto Layout, so the bottom anchor holds regardless of the parent's `isFlipped`; manual
        // frames with autoresizing put the bar on the wrong edge in some combinations.
        bar.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(bar)
        findBar = bar
        let barMargin: CGFloat = 8
        let barHeight: CGFloat = 32
        NSLayoutConstraint.activate([
            bar.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: barMargin),
            bar.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -barMargin),
            bar.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -barMargin),
            bar.heightAnchor.constraint(equalToConstant: barHeight),
        ])

        view = container

        // Runs before viewDidAppear; NSImageView accepts an image off-screen and applyBackground
        // re-snaps layer state.
        if let path = config.backgroundPath {
            applyBackground(path: path, tint: config.backgroundTint, opacity: config.backgroundOpacity)
        }
    }

    override func viewDidLayout() {
        super.viewDidLayout()
        // Grid size from view bounds + cell metrics, so the shell's winsize matches the drawing.
        guard let render = renderView else { return }
        let (cols, rows) = render.computeGrid()
        termHandle?.resize(cols: cols, rows: rows)
    }

    func startIfNeeded() {
        guard !shellStarted else { return }
        shellStarted = true
        let (cols, rows) = renderView?.computeGrid() ?? (80, 24)
        // The in-shell hook's `coctl call` must hit this GUI, not the well-known daemon socket.
        let socketPath = "/tmp/copad-\(ProcessInfo.processInfo.processIdentifier).sock"
        // Decision #64: panes spawn the login shell directly; copad does not manage tmux.
        termHandle = CopadTermFFI.Handle(
            cols: cols,
            rows: rows,
            shell: initialCwd != nil ? config.shell : nil,
            cwd: initialCwd,
            panelID: panelID,
            socketPath: socketPath,
        )
        // Push the palette now so the first OSC 4/10/11/12 query (often nvim/fish init) gets the
        // rendered colors; applyTheme repeats this on hot-reload.
        termHandle?.applyPaletteFromTheme(theme)
        termHandle?.setKittyKeyboard(config.kittyKeyboard)
        termHandle?.setScrollback(config.scrollback)
        termHandle?.setImages(config.images)
        if let initialInput {
            termHandle?.input(Array(initialInput.utf8))
        }
        renderView?.bind(handle: termHandle)
        // Target the render view directly rather than via the container.
        if let render = renderView {
            view.window?.makeFirstResponder(render)
        }
    }

    /// Config hot-reload: `[terminal] kitty_keyboard`. Turning it off drops whatever flags a
    /// running program negotiated (it gets legacy keys from then on).
    func applyKittyKeyboard(_ enabled: Bool) {
        termHandle?.setKittyKeyboard(enabled)
    }

    func applyScrollback(_ lines: Int) {
        termHandle?.setScrollback(lines)
    }

    /// Config hot-reload: `[terminal] images`. Images already on screen stay until erased.
    func applyImages(_ enabled: Bool) {
        termHandle?.setImages(enabled)
    }

    /// Config hot-reload: apply the `[security] osc52` policy to already-open panes.
    func applyOSC52Policy(_ policy: OSC52Policy) {
        renderView?.setOSC52Policy(policy)
    }

    /// Config hot-reload: `[window] opacity`. The cell-renderer half; the window-level swap is in
    /// `AppDelegate.applyWindowTransparency`.
    func applyWindowOpacity(_ opacity: Double) {
        windowOpacity = opacity
        renderView?.setWindowOpacity(opacity)
    }

    /// Config hot-reload: swap the theme on a running pane.
    func applyTheme(_ newTheme: CopadTheme) {
        theme = newTheme
        renderView?.setTheme(newTheme)
        termHandle?.applyPaletteFromTheme(newTheme)
        findBar?.applyTheme(newTheme)
    }

    // MARK: - Find

    /// Cmd+F toggle (via `AppDelegate.performFindPanelAction`): show and focus, or hide and clear
    /// search state.
    func toggleFindBar() {
        guard let bar = findBar else { return }
        if bar.isHidden {
            bar.isHidden = false
            bar.focusSearchField()
            // Re-apply the existing query so the last highlight returns.
            performFindIfPattern(forward: true)
        } else {
            closeFindBar()
        }
    }

    /// Esc / close button: hides the bar, clears the Rust search state, refocuses the render view.
    func closeFindBar() {
        findBar?.isHidden = true
        if let h = termHandle {
            h.searchClear()
            renderView?.refreshSnapshotForSearchChange(h)
        }
        if let render = renderView {
            view.window?.makeFirstResponder(render)
        }
    }

    /// Cmd+G / Cmd+Shift+G; acts only when the bar is visible.
    func findNext() {
        performFindAction(forward: true)
    }

    func findPrevious() {
        performFindAction(forward: false)
    }

    private func performFindAction(forward: Bool) {
        guard let bar = findBar, !bar.isHidden else { return }
        let pattern = bar.currentPattern
        guard !pattern.isEmpty, let h = termHandle else { return }
        _ = h.searchNext(pattern: pattern, caseSensitive: bar.caseSensitive, forward: forward)
        // searchNext changes Rust-side state only (no grid damage), so force a snapshot refresh for
        // `draw(_:)` to see the new `search_match`.
        renderView?.refreshSnapshotForSearchChange(h)
    }

    private func performFindIfPattern(forward: Bool) {
        guard let bar = findBar else { return }
        let pattern = bar.currentPattern
        guard let h = termHandle else { return }
        if pattern.isEmpty {
            // The query was deleted: drop the cached match so the highlight vanishes.
            h.searchClear()
            renderView?.refreshSnapshotForSearchChange(h)
            return
        }
        _ = h.searchNext(pattern: pattern, caseSensitive: bar.caseSensitive, forward: forward)
        renderView?.refreshSnapshotForSearchChange(h)
    }

    private func handleFindAction(_ action: FindBar.Action) {
        switch action {
        case .queryChanged:
            performFindIfPattern(forward: true)
        case .next:
            performFindAction(forward: true)
        case .prev:
            performFindAction(forward: false)
        case .caseToggle:
            performFindIfPattern(forward: true)
        case .close:
            closeFindBar()
        }
    }

    /// Config hot-reload: `[terminal] font_thicken`.
    func applyFontThicken(_ value: FontThicken) {
        renderView?.setFontThicken(value)
    }

    func applyAlphaBlending(_ value: AlphaBlending) {
        renderView?.setAlphaBlending(value)
    }

    func applyFont(family: String, baseSize: CGFloat) {
        configFontSize = baseSize
        currentFontFamily = family
        // Re-apply at the current (possibly zoomed) size so zoom survives the config reload;
        // only the `configFontSize` baseline moves, so a live Cmd++ is not clobbered.
        applyFontInternal(family: family, size: currentFontSize)
    }

    private func applyFontInternal(family: String, size: CGFloat) {
        let newFont = resolveFont(family: family, size: size)
        guard let render = renderView else { return }
        let metricsChanged = render.setFont(newFont)
        if metricsChanged {
            let (cols, rows) = render.computeGrid()
            termHandle?.resize(cols: cols, rows: rows)
        }
    }

    // MARK: - TerminalCapable (socket commands)

    /// `tab.rename` entry: stores the title and repaints the tab bar; OSC 0/2 are then ignored.
    func setCustomTitle(_ title: String) {
        customTitle = title
        currentTitle = title
        NotificationCenter.default.post(name: .terminalTitleChanged, object: self)
    }

    /// Raw UTF-8 text to the PTY. Kernel-buffered, so feeds before `startIfNeeded` reach the child.
    func feedText(_ text: String) {
        termHandle?.input(Array(text.utf8))
    }

    /// Feed plus a trailing bare LF (no CR), as SwiftTerm does, so scripts behave the same.
    func execCommand(_ command: String) {
        feedText(command + "\n")
    }

    /// `terminal.state`: grid metrics, cursor as `[row, col]` (SwiftTerm's shape), window title.
    func terminalState() -> [String: Any] {
        guard let snap = termHandle?.snapshot() else { return [:] }
        let cursor = snap.cursor
        return [
            "cols": Int(snap.cols),
            "rows": Int(snap.rows),
            "cursor": [Int(cursor.row), Int(cursor.col)],
            "title": view.window?.title ?? "copad",
        ]
    }

    /// `terminal.read`: the visible viewport as plain text, one line per row. Bytes are copied out
    /// of the borrowed `rowUtf8` before the snapshot is dropped.
    func readScreen() -> [String: Any] {
        guard let snap = termHandle?.snapshot() else { return [:] }
        var lines: [String] = []
        lines.reserveCapacity(Int(snap.rows))
        for r in 0 ..< snap.rows {
            let buf = snap.rowUtf8(r)
            lines.append(buf.isEmpty ? "" : String(decoding: buf, as: UTF8.self))
        }
        let cursor = snap.cursor
        return [
            "text": lines.joined(separator: "\n"),
            "cursor": [Int(cursor.row), Int(cursor.col)],
            "rows": Int(snap.rows),
            "cols": Int(snap.cols),
        ]
    }

    /// `terminal.history`: last `lines` rows of scrollback above the viewport via
    /// `copad_term_history`. `rows`/`cols` are viewport metrics, so callers can pair the text with
    /// a follow-up `terminal.state`.
    func history(lines: Int = 100) -> [String: Any] {
        guard let handle = termHandle, let snap = handle.snapshot() else { return [:] }
        let text = handle.history(lines: lines) ?? ""
        return [
            "text": text,
            "lines_requested": lines,
            "rows": Int(snap.rows),
            "cols": Int(snap.cols),
        ]
    }

    func context(historyLines: Int = 50) -> [String: Any] {
        [
            "state": terminalState(),
            "screen": readScreen(),
            "history": history(lines: historyLines),
        ]
    }

    // MARK: - Zoom (Cmd+= / Cmd+- / Cmd+0)

    /// Zoom in/out/reset, with the same step and clamp as the SwiftTerm path. `currentFontSize` is
    /// the source of truth; each step re-runs `applyFontInternal`, which resizes the PTY grid
    /// (SIGWINCH) so shells re-wrap.
    func zoomIn() {
        let newSize = min(currentFontSize + 1, 72)
        setFontSize(newSize)
    }

    func zoomOut() {
        let newSize = max(currentFontSize - 1, 6)
        setFontSize(newSize)
    }

    func zoomReset() {
        setFontSize(configFontSize)
    }

    private func setFontSize(_ size: CGFloat) {
        guard size != currentFontSize else { return }
        currentFontSize = size
        applyFontInternal(family: currentFontFamily, size: size)
    }

    // MARK: - CopadPanel — background

    /// Mirror of the render view's `windowOpacity`, set from config and updated on hot-reload; the
    /// render view keeps its own copy because the draw loop reads it every frame.
    private var windowOpacity: Double = 1.0

    /// Only updates renderer state so the draw path skips the opaque default-bg fill
    /// (`isTransparentBgActive`). The image and tint are owned by TabViewController.contentArea;
    /// the parameters exist for `CopadPanel` conformance.
    func applyBackground(path _: String, tint _: Double, opacity _: Double) {
        renderView?.setImageBackgroundActive(true)
    }

    func clearBackground() {
        renderView?.setImageBackgroundActive(false)
    }

    func setTint(_: Double) {
        // No-op: the tint is rendered at the window level (TabViewController).
    }

    // MARK: - Font

    /// Mirrors `TerminalViewController.resolveFont`: PostScript name, then family (case-
    /// insensitive), then monospaced system.
    private func resolveFont(family: String, size: CGFloat) -> NSFont {
        resolveTerminalFont(family: family, size: size)
    }
}

/// The configured `font_family` as the terminal draws it: a font name, else a family (any case),
/// else system monospace. Shared with the comux chrome so it uses the same face.
@MainActor
func resolveTerminalFont(family: String, size: CGFloat) -> NSFont {
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

// MARK: - Render view

/// NSView that draws the terminal grid via CoreText. Snapshots are taken under the handle's
/// `FairMutex`, which is dropped before `setNeedsDisplay` so redraws do not block the PTY reader
/// thread. The coordinate system is flipped (origin top-left) so row 0 is at the top.
@MainActor
private final class AlacrittyRenderView: NSView, @preconcurrency NSTextInputClient {
    /// Hot-swapped by `setTheme(_:)`; draw paths read it each frame.
    private var theme: CopadTheme
    private var font: NSFont
    private var boldFont: NSFont
    private var italicFont: NSFont
    private var boldItalicFont: NSFont
    private(set) var cellWidth: CGFloat = 0
    private(set) var cellHeight: CGFloat = 0
    private var ascent: CGFloat = 0

    /// Cached CGColors for the ANSI palette + xterm 256: 0-15 from `theme.palette`, 16-231 the
    /// 6x6x6 cube, 232-255 grayscale. Rebuilt by `setTheme(_:)`.
    private var paletteCache: [CGColor]

    private weak var termHandle: CopadTermFFI.Handle?
    /// Fires once per display refresh (up to ProMotion's 120 Hz), vsync-aligned; the per-tick
    /// `takeDamage` gate keeps an idle terminal at zero work. `nonisolated(unsafe)` so the
    /// nonisolated deinit can invalidate it.
    private nonisolated(unsafe) var vsyncLink: CADisplayLink?

    /// Snapshot of the latest paint, refreshed only when `copad_term_take_damage` reports changes.
    private var snapshotCache: CopadTermFFI.Snapshot?
    /// CPU painter's decoded kitty-graphics images by key, trimmed to what the snapshot shows.
    private var imageCache: [UInt64: CGImage] = [:]

    /// Panel identity stamped into every `terminal.output` event; empty for test-only views.
    var panelID: String = ""

    /// Bus for publishing `terminal.output` on keyboard/paste input; the VC's setter forwards
    /// later assignments.
    weak var eventBus: EventBus?

    /// Fires once when the PTY child exits; forwarded to the VC's `onChildExited`, which
    /// PaneManager wires to the close cascade (`[terminal] close_on_exit`).
    var onChildExited: (() -> Void)?

    /// User opt-in from config: with an active image background (`imageBackgroundActive`),
    /// default (sentinel-zero) cells render without a bg fill so the image shows through.
    private let transparentDefaultBg: Bool
    /// `[terminal] font_thicken`, resolved against the backing scale on every use so moving
    /// between Retina and 1x displays picks the right rasterization.
    private var fontThicken: FontThicken
    private var smoothFonts: Bool {
        fontThicken.smooths(scale: window?.backingScaleFactor ?? 2)
    }
    /// `[terminal] alpha_blending`. GPU painter only; CoreText always blends like Core Graphics.
    private var alphaBlending: AlphaBlending
    private var imageBackgroundActive = false

    /// `[window] opacity`: below 1.0 the window is non-opaque and default-bg cells are skipped so
    /// the tinted layer or blurred desktop shows. ANSI bg and reverse-video cells stay opaque.
    private var windowOpacity: Double

    /// Single gate for skipping default-bg cell fills: `[window] opacity < 1.0` or
    /// `transparentDefaultBg && imageBackgroundActive`. Used by the bounds fill, the per-cell
    /// skip and the cursor-outline heuristic; do not duplicate the OR.
    private var isTransparentBgActive: Bool {
        windowOpacity < 1.0 || (transparentDefaultBg && imageBackgroundActive)
    }

    /// OSC 52 policy: `.allow` writes to NSPasteboard.general, `.deny` drops with a stderr
    /// warning. Hot-reloadable via `applyOSC52Policy`.
    private var osc52Policy: OSC52Policy

    func setOSC52Policy(_ policy: OSC52Policy) {
        osc52Policy = policy
    }

    /// When true, Option+key writes `ESC + base_char` instead of going through the IME (which
    /// would deliver `¡™£¢`), so Alt-bindings fire.
    private let optionAsAlt: Bool

    /// Virtual keyCodes that send `ESC + <byte>` on Option even though `optionAsAlt` filters out
    /// control characters; from `config.forceMetaKeys` and independent of `optionAsAlt`.
    private let forceMetaKeyCodes: Set<UInt16>

    /// Cursor blink, honored only when the program asks via DECSCUSR (`cursor.blink == 1`). While
    /// blinking and idle, the display-link callback redraws every `blinkInterval` (2/sec).
    private var blinkVisible = true
    private var lastBlinkToggle = Date.distantPast
    private let blinkInterval: TimeInterval = 0.5

    /// Trackpad pixel deltas accumulate here so slow fractional swipes still produce whole-cell
    /// scrolls. Mouse wheels (`hasPreciseScrollingDeltas == false`) bypass it.
    private var accumulatedScrollDelta: CGFloat = 0

    /// IME composition state: while composing (Korean, kana, Pinyin) the system delivers
    /// `setMarkedText`, nothing reaches the PTY until `insertText`, and the marked text is painted
    /// as an overlay at the cursor. `markedSelectedRange` is the IME-highlighted sub-range, drawn
    /// with a stronger underline.
    private var markedText: String?
    private var markedSelectedRange: NSRange = .init(location: 0, length: 0)

    /// Codepoint to fallback NSFont, filled lazily by `resolveRunFont` for runs with a non-ASCII
    /// scalar (all-ASCII runs skip it). Keyed by the first non-ASCII scalar, since `walk_row`
    /// groups by attribute and a run mixing Hangul with Powerline glyphs is unreachable in
    /// practice. Invalidated on `setFont`.
    private var fallbackCache: [UInt32: NSFont] = [:]

    /// (text, font, fg, decoration) to retained CTLine cache, so the redraw loop avoids about 19k
    /// `NSAttributedString` + `CTLineCreate` allocations per second. Per render view, as each pane
    /// has its own font and zoom.
    ///
    /// The key holds the full text, so Dictionary equality handles collisions. LRU via a tick
    /// counter with an O(n) scan on eviction, which only runs when full.
    ///
    /// Flushed on `setFont` and `setTheme` (`fgRGBA` goes stale). Not on
    /// `setImageBackgroundActive` / `setWindowOpacity`: only the bg fill reads those.
    private struct CTLineCacheKey: Hashable {
        let text: String
        let fontId: ObjectIdentifier
        let fgRGBA: UInt32
        /// bits 0-7: underline_style (raw u8 from `CopadRun`); bits 8-10: bold, italic, strike.
        /// dim/inverse are not bits; the caller bakes them into `fgRGBA`.
        let styleBits: UInt16
        let underlineRGBA: UInt32
        let strikeRGBA: UInt32
    }

    private struct CTLineCacheEntry {
        let line: CTLine
        var tick: UInt64
    }

    private var ctLineCache: [CTLineCacheKey: CTLineCacheEntry] = [:]
    private var ctLineTick: UInt64 = 0
    private static let ctLineCacheMax: Int = 2048

    private static let styleBitBold: UInt16 = 1 << 8
    private static let styleBitItalic: UInt16 = 1 << 9
    private static let styleBitStrike: UInt16 = 1 << 10

    /// GPU mode: a non-nil device makes `makeBackingLayer` return a CAMetalLayer painted by
    /// `metalRenderer`; nil uses the CoreText `draw(_:)` path. Fixed at init.
    private let gpuDevice: MTLDevice?
    private var metalRenderer: MetalGridRenderer?
    /// Pending-repaint latch for GPU mode, set by `requestRepaint()` and cleared only after a
    /// frame encodes and presents, so a nil `nextDrawable` cannot drop an owed repaint.
    private var gpuFrameDirty = false
    /// Theme colors pre-packed for the Metal painter; rebuilt in `setTheme`.
    private var gpuTheme: MetalGridRenderer.ThemeColors?
    /// CG-drawn IME composition overlay, GPU mode only; the CPU painter draws it in `draw(_:)`.
    private var imeOverlay: IMEPreeditOverlayView?

    /// Perf harness, enabled by `COPAD_RENDER_METRICS`: each painter times its main-thread
    /// per-frame work and reports windowed percentiles to stderr every `metricsReportInterval`
    /// frames, labeled `gpu`/`cpu` + grid size. Main-thread time is the comparable metric (it
    /// governs input latency); async GPU execution is out of scope. Zero overhead when off.
    private let renderMetricsEnabled =
        ProcessInfo.processInfo.environment["COPAD_RENDER_METRICS"] != nil
    /// Render work per frame: GPU = instance assembly + encode (drawable wait excluded); CPU = the
    /// CoreText `draw(_:)` body.
    private var frameMetrics = FrameMetricsRecorder(capacity: 240)
    /// GPU only: `nextDrawable` vsync back-pressure, reported beside the work number so display
    /// throttling is not misread as render cost.
    private var drawableWaitMetrics = FrameMetricsRecorder(capacity: 240)
    private var metricsFrameCounter = 0
    private static let metricsReportInterval = 120

    init(
        theme: CopadTheme,
        font: NSFont,
        fontThicken: FontThicken,
        alphaBlending: AlphaBlending,
        transparentDefaultBg: Bool,
        windowOpacity: Double,
        osc52Policy: OSC52Policy,
        optionAsAlt: Bool,
        forceMetaKeys: [String],
        gpuDevice: MTLDevice? = nil,
    ) {
        self.theme = theme
        self.font = font
        boldFont = Self.deriveTrait(font, mask: .boldFontMask)
        italicFont = Self.deriveTrait(font, mask: .italicFontMask)
        boldItalicFont = Self.deriveTrait(font, mask: [.boldFontMask, .italicFontMask])
        paletteCache = Self.buildPalette(theme: theme)
        self.fontThicken = fontThicken
        self.alphaBlending = alphaBlending
        self.transparentDefaultBg = transparentDefaultBg
        self.windowOpacity = windowOpacity
        self.osc52Policy = osc52Policy
        self.optionAsAlt = optionAsAlt
        self.gpuDevice = gpuDevice
        forceMetaKeyCodes = Self.parseForceMetaKeyCodes(forceMetaKeys)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = Self.layerBg(theme: theme, opacity: windowOpacity, imageActive: false)
        recomputeCellMetrics()
        setupGPUPipelineIfRequested()
        // Drop shapes: `.fileURL` (Finder, one drop for multi-select), `.png`/`.tiff` raw image
        // bytes (materialized to a temp file so the pasted path is readable by the CLI), and
        // `.URL` (web links, pasted as text).
        registerForDraggedTypes([.fileURL, .png, .tiff, .URL])
        // CADisplayLink needs a window, so it is hooked up in `viewDidMoveToWindow`.
    }

    /// CAMetalLayer when GPU mode resolved at construction, else AppKit's default. Never opaque:
    /// the transparent modes need the alpha-0 clear to composite.
    override func makeBackingLayer() -> CALayer {
        guard let device = gpuDevice else { return super.makeBackingLayer() }
        let metalLayer = CAMetalLayer()
        metalLayer.device = device
        metalLayer.pixelFormat = .bgra8Unorm
        metalLayer.framebufferOnly = true
        metalLayer.isOpaque = false
        return metalLayer
    }

    /// Build the Metal painter and IME overlay. On failure (should not happen on a real device)
    /// a plain CALayer is swapped back in so the CoreText painter still renders.
    private func setupGPUPipelineIfRequested() {
        guard let device = gpuDevice else { return }
        if let metalLayer = layer as? CAMetalLayer,
           let renderer = MetalGridRenderer(
               layer: metalLayer,
               device: device,
               fonts: gpuFonts(),
               metrics: gpuMetrics(),
           )
        {
            renderer.alphaBlending = alphaBlending
            metalRenderer = renderer
            rebuildGPUTheme()
            let overlay = IMEPreeditOverlayView(theme: theme, font: font)
            overlay.frame = bounds
            overlay.autoresizingMask = [.width, .height]
            overlay.resolveFont = { [weak self] str, base in
                self?.resolveRunFont(str, base: base) ?? base
            }
            overlay.updateFont(font, cellWidth: cellWidth, cellHeight: cellHeight, ascent: ascent)
            addSubview(overlay)
            imeOverlay = overlay
        } else {
            FileHandle.standardError.write(
                Data("[copad] Metal renderer init failed — falling back to CoreText painter\n".utf8),
            )
            let fallback = CALayer()
            fallback.backgroundColor = Self.layerBg(
                theme: theme,
                opacity: windowOpacity,
                imageActive: false,
            )
            layer = fallback
        }
    }

    private func gpuFonts() -> MetalGridRenderer.Fonts {
        MetalGridRenderer.Fonts(
            regular: font,
            bold: boldFont,
            italic: italicFont,
            boldItalic: boldItalicFont,
        )
    }

    private func gpuMetrics() -> MetalGridRenderer.Metrics {
        MetalGridRenderer.Metrics(
            cellWidth: cellWidth,
            cellHeight: cellHeight,
            ascent: ascent,
            scale: window?.backingScaleFactor ?? 2,
            smoothFonts: smoothFonts,
        )
    }

    private func rebuildGPUTheme() {
        guard metalRenderer != nil else { return }
        gpuTheme = MetalGridRenderer.ThemeColors(
            defaultFg: cgColorToRGBA32(theme.foreground.nsColor.cgColor),
            defaultBg: cgColorToRGBA32(theme.background.nsColor.cgColor),
            accent: cgColorToRGBA32(theme.accent.nsColor.cgColor),
            surface2: cgColorToRGBA32(theme.surface2.nsColor.cgColor),
            palette: paletteCache.map { cgColorToRGBA32($0) },
        )
    }

    /// Single repaint funnel: every invalidation (PTY damage, theme/opacity/background flips,
    /// focus, selection, search, font swap) routes here so GPU mode cannot miss a non-damage
    /// change. CPU mode forwards to AppKit; GPU mode latches the dirty flag for the next
    /// display-link tick.
    func requestRepaint() {
        if metalRenderer != nil {
            gpuFrameDirty = true
        } else {
            needsDisplay = true
        }
    }

    /// Hot-reload: swap the theme and rebuild the palette cache; also refreshes the layer bg, as
    /// `setImageBackgroundActive` does for the image path.
    func setTheme(_ newTheme: CopadTheme) {
        theme = newTheme
        paletteCache = Self.buildPalette(theme: newTheme)
        flushCTLineCache()
        rebuildGPUTheme()
        imeOverlay?.updateTheme(newTheme)
        if !imageBackgroundActive {
            layer?.backgroundColor = Self.layerBg(
                theme: newTheme,
                opacity: windowOpacity,
                imageActive: false,
            )
        }
        requestRepaint()
    }

    /// Hot-reload of `[window] opacity`: updates the alpha and, with no image active, the layer bg
    /// (the image path uses `.clear` via `setImageBackgroundActive`).
    func setWindowOpacity(_ newOpacity: Double) {
        windowOpacity = newOpacity
        if !imageBackgroundActive {
            layer?.backgroundColor = Self.layerBg(
                theme: theme,
                opacity: newOpacity,
                imageActive: false,
            )
        }
        requestRepaint()
    }

    /// Layer background CGColor: `.clear` when an image is active (the image view and tint paint
    /// the bg), else theme.background with alpha = opacity so a non-opaque window shows through.
    private static func layerBg(theme: CopadTheme, opacity: Double, imageActive: Bool) -> CGColor {
        if imageActive { return NSColor.clear.cgColor }
        let nsColor = theme.background.nsColor
        return opacity < 1.0
            ? nsColor.withAlphaComponent(CGFloat(opacity)).cgColor
            : nsColor.cgColor
    }

    /// Live config reload of `alpha_blending`: the next frame blends in the new space.
    func setAlphaBlending(_ value: AlphaBlending) {
        guard value != alphaBlending else { return }
        alphaBlending = value
        metalRenderer?.alphaBlending = value
        requestRepaint()
    }

    /// Live config reload of `font_thicken`: re-rasterize every glyph.
    func setFontThicken(_ value: FontThicken) {
        guard value != fontThicken else { return }
        fontThicken = value
        flushCTLineCache()
        metalRenderer?.setFonts(gpuFonts(), metrics: gpuMetrics())
        needsDisplay = true
        requestRepaint()
    }

    /// Returns true when the cell size changed.
    @discardableResult
    func setFont(_ newFont: NSFont) -> Bool {
        font = newFont
        boldFont = Self.deriveTrait(newFont, mask: .boldFontMask)
        italicFont = Self.deriveTrait(newFont, mask: .italicFontMask)
        boldItalicFont = Self.deriveTrait(newFont, mask: [.boldFontMask, .italicFontMask])
        fallbackCache.removeAll(keepingCapacity: true)
        flushCTLineCache()
        let oldW = cellWidth
        let oldH = cellHeight
        recomputeCellMetrics()
        metalRenderer?.setFonts(gpuFonts(), metrics: gpuMetrics())
        imeOverlay?.updateFont(newFont, cellWidth: cellWidth, cellHeight: cellHeight, ascent: ascent)
        requestRepaint()
        return cellWidth != oldW || cellHeight != oldH
    }

    /// Called when `applyBackground` / `clearBackground` flips the layered-view state. Toggles the
    /// layer-bg clear/opaque and the bounds-fill skip; both are needed, since the bounds fill
    /// would re-cover the image inside `draw(_:)`.
    func setImageBackgroundActive(_ active: Bool) {
        imageBackgroundActive = active
        layer?.backgroundColor = Self.layerBg(
            theme: theme,
            opacity: windowOpacity,
            imageActive: active,
        )
        requestRepaint()
    }

    /// Refresh `snapshotCache` now, bypassing the per-tick damage gate: a find changes the search
    /// match inside the snapshot without any grid damage.
    func refreshSnapshotForSearchChange(_ handle: CopadTermFFI.Handle) {
        snapshotCache = handle.snapshot()
        requestRepaint()
    }

    /// Apply font traits via NSFontManager, falling back to the regular face when the family has
    /// no such variant (synthesized italics look awkward).
    private static func deriveTrait(_ regular: NSFont, mask: NSFontTraitMask) -> NSFont {
        let mgr = NSFontManager.shared
        if let variant = mgr.convert(regular, toHaveTrait: mask) as NSFont? {
            return variant
        }
        return regular
    }

    /// Resolve a run's font, with system cascade fallback for codepoints the base font lacks
    /// (CJK, Nerd Font PUA U+E000-F8FF, emoji); otherwise `CTLineCreateWithAttributedString`
    /// draws `.notdef` boxes because the `.font` attribute pins one font.
    ///
    /// The fast path skips only runs whose every scalar is ASCII: a run can mix ASCII with
    /// non-ASCII combining marks (`a` + U+0301) since `walk_row` groups by attribute, not script.
    /// The cache key is the first non-ASCII scalar, which drives `CTFontCreateForString`.
    private func resolveRunFont(_ str: String, base: NSFont) -> NSFont {
        var key: UInt32?
        for scalar in str.unicodeScalars where scalar.value >= 0x80 {
            key = scalar.value
            break
        }
        guard let cp = key else { return base }
        if let cached = fallbackCache[cp] { return cached }
        let resolved = CTFontCreateForString(
            base as CTFont,
            str as CFString,
            CFRange(location: 0, length: (str as NSString).length),
        ) as NSFont
        fallbackCache[cp] = resolved
        return resolved
    }

    /// Pack a `CGColor` into RGBA8888 for a cache key. Unexpected color spaces give 0, which can
    /// only cause cache misses: the `text` field's String equality rules out wrong-glyph hits.
    @inline(__always)
    private func cgColorToRGBA32(_ color: CGColor) -> UInt32 {
        guard let comps = color.components else { return 0 }
        let r: CGFloat, g: CGFloat, b: CGFloat, a: CGFloat
        switch comps.count {
        case 2:
            r = comps[0]; g = comps[0]; b = comps[0]; a = comps[1]
        case 4:
            r = comps[0]; g = comps[1]; b = comps[2]; a = comps[3]
        default:
            return 0
        }
        let rr = UInt32(max(0, min(255, Int(r * 255))))
        let gg = UInt32(max(0, min(255, Int(g * 255))))
        let bb = UInt32(max(0, min(255, Int(b * 255))))
        let aa = UInt32(max(0, min(255, Int(a * 255))))
        return (rr << 24) | (gg << 16) | (bb << 8) | aa
    }

    /// LRU lookup/build: a hit bumps the tick; a miss builds via
    /// `CTLineCreateWithAttributedString`, stores, and evicts the lowest-tick entry when over
    /// `ctLineCacheMax`. `underlineColor`/`strikeColor` are 0 when not applied but stay in the key
    /// so toggling them never reuses the wrong line.
    private func ctLineFor(
        text: String,
        font: NSFont,
        fgRGBA: UInt32,
        styleBits: UInt16,
        underlineRGBA: UInt32,
        strikeRGBA: UInt32,
    ) -> CTLine {
        ctLineTick &+= 1
        let key = CTLineCacheKey(
            text: text,
            fontId: ObjectIdentifier(font),
            fgRGBA: fgRGBA,
            styleBits: styleBits,
            underlineRGBA: underlineRGBA,
            strikeRGBA: strikeRGBA,
        )
        if var entry = ctLineCache[key] {
            entry.tick = ctLineTick
            ctLineCache[key] = entry
            return entry.line
        }

        // Miss: build the attributed string. `styleBits` selects the decoration attrs; `font`
        // already carries the right bold/italic face.
        var attrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor(red: CGFloat((fgRGBA >> 24) & 0xFF) / 255.0,
                                      green: CGFloat((fgRGBA >> 16) & 0xFF) / 255.0,
                                      blue: CGFloat((fgRGBA >> 8) & 0xFF) / 255.0,
                                      alpha: CGFloat(fgRGBA & 0xFF) / 255.0),
        ]
        // Underlines are not Core Text attributes: Core Text breaks them at spaces and cannot
        // draw a curly one, so `drawRow` strokes every SGR 4:x style itself (as the GPU path does).
        if styleBits & Self.styleBitStrike != 0 {
            attrs[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
            attrs[.strikethroughColor] = NSColor(
                red: CGFloat((strikeRGBA >> 24) & 0xFF) / 255.0,
                green: CGFloat((strikeRGBA >> 16) & 0xFF) / 255.0,
                blue: CGFloat((strikeRGBA >> 8) & 0xFF) / 255.0,
                alpha: CGFloat(strikeRGBA & 0xFF) / 255.0,
            )
        }
        let attr = NSAttributedString(string: text, attributes: attrs)
        let line = CTLineCreateWithAttributedString(attr)

        // Evict the lowest-tick entry when full; the O(n) scan is effectively never reached.
        if ctLineCache.count >= Self.ctLineCacheMax {
            var oldestKey: CTLineCacheKey?
            var oldestTick: UInt64 = .max
            for (k, v) in ctLineCache where v.tick < oldestTick {
                oldestTick = v.tick
                oldestKey = k
            }
            if let k = oldestKey {
                ctLineCache.removeValue(forKey: k)
            }
        }
        ctLineCache[key] = CTLineCacheEntry(line: line, tick: ctLineTick)
        return line
    }

    private func flushCTLineCache() {
        ctLineCache.removeAll(keepingCapacity: true)
        ctLineTick = 0
    }

    /// 256-color table: 0-15 from theme.palette, 16-231 the xterm 6x6x6 cube, 232-255 grayscale.
    private static func buildPalette(theme: CopadTheme) -> [CGColor] {
        var out: [CGColor] = []
        out.reserveCapacity(256)
        for c in theme.palette {
            out.append(c.nsColor.cgColor)
        }
        // Pad to 16 entries (black) in case a theme ships fewer.
        while out.count < 16 {
            out.append(CGColor(red: 0, green: 0, blue: 0, alpha: 1))
        }
        let cubeLevels: [CGFloat] = [0, 95, 135, 175, 215, 255].map { $0 / 255.0 }
        for r in 0 ..< 6 {
            for g in 0 ..< 6 {
                for b in 0 ..< 6 {
                    out.append(CGColor(red: cubeLevels[r], green: cubeLevels[g], blue: cubeLevels[b], alpha: 1))
                }
            }
        }
        for i in 0 ..< 24 {
            let v = CGFloat(8 + i * 10) / 255.0
            out.append(CGColor(red: v, green: v, blue: v, alpha: 1))
        }
        return out
    }

    /// Decode the fg/bg encoding from `copad-term::color_to_rgba`: high byte 0x00 = default,
    /// 0x01 = indexed (low byte), 0xFF = direct RGB (low 24 bits). Tagged because the old "alpha
    /// byte 0 means indexed" scheme sent RGB colors with R=0 (skyblue, pure green) to the
    /// indexed path, rendering them grayscale.
    private func resolveColor(_ packed: UInt32, defaultColor: CGColor) -> CGColor {
        let tag = (packed >> 24) & 0xFF
        switch tag {
        case 0x00:
            return defaultColor
        case 0x01:
            let idx = Int(packed & 0xFF)
            return idx < paletteCache.count ? paletteCache[idx] : defaultColor
        case 0xFF:
            let r = CGFloat((packed >> 16) & 0xFF) / 255.0
            let g = CGFloat((packed >> 8) & 0xFF) / 255.0
            let b = CGFloat(packed & 0xFF) / 255.0
            return CGColor(red: r, green: g, blue: b, alpha: 1.0)
        default:
            return defaultColor
        }
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    deinit {
        vsyncLink?.invalidate()
        NotificationCenter.default.removeObserver(self)
    }

    /// Bind the display link once the view has a window, and tear it down when AppKit passes a
    /// nil window. Also observes key-window changes so the cursor flips between filled and hollow
    /// without waiting for terminal damage.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        let center = NotificationCenter.default
        guard let win = window else {
            vsyncLink?.invalidate()
            vsyncLink = nil
            center.removeObserver(self)
            // A pane off screen (a background tab) need not keep full-size drawables; the
            // repaint on the way back in sizes them again.
            metalRenderer?.releaseDrawables()
            return
        }
        if vsyncLink == nil {
            // `displayLink(target:selector:)` is NSView's factory (macOS 14+); the property is
            // `vsyncLink` so it does not shadow the method.
            let link = displayLink(target: self, selector: #selector(displayLinkFired(_:)))
            link.add(to: .current, forMode: .common)
            vsyncLink = link
        }
        // The real backingScaleFactor is known only now; init-time metrics assumed 2x.
        reportCellPixels()
        if metalRenderer != nil {
            layer?.contentsScale = win.backingScaleFactor
            metalRenderer?.setFonts(gpuFonts(), metrics: gpuMetrics())
            requestRepaint()
        }
        center.removeObserver(self)
        center.addObserver(self, selector: #selector(windowFocusChanged(_:)),
                           name: NSWindow.didBecomeKeyNotification, object: win)
        center.addObserver(self, selector: #selector(windowFocusChanged(_:)),
                           name: NSWindow.didResignKeyNotification, object: win)
    }

    @objc private func windowFocusChanged(_: Notification) {
        // The cursor draw depends on `window?.isKeyWindow`, so invalidate the cached bitmap; the
        // cached snapshot is reused.
        requestRepaint()
        reportFocus((window?.isKeyWindow ?? false) && window?.firstResponder === self)
    }

    /// Keyboard focus is the key window's first responder; forwarded as `?1004` focus reports.
    private func reportFocus(_ focused: Bool) {
        termHandle?.setFocused(focused)
    }

    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became {
            reportFocus(window?.isKeyWindow ?? false)
        }
        return became
    }

    override var isFlipped: Bool {
        true
    }

    override var acceptsFirstResponder: Bool {
        true
    }

    /// Display switch or Retina/non-Retina move: the atlas is rasterized at the old scale, so
    /// re-derive metrics (which flushes it) and repaint. CPU mode needs neither.
    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        reportCellPixels()
        guard metalRenderer != nil else { return }
        layer?.contentsScale = window?.backingScaleFactor ?? 2
        metalRenderer?.setFonts(gpuFonts(), metrics: gpuMetrics())
        requestRepaint()
    }

    /// GPU mode redraws on resize explicitly: there is no AppKit `draw(_:)` pass, and the
    /// drawable size follows the view size inside `render`.
    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        if metalRenderer != nil {
            requestRepaint()
        }
    }

    func bind(handle: CopadTermFFI.Handle?) {
        termHandle = handle
        reportCellPixels()
    }

    /// Tell the terminal the cell size in device pixels, which programs size images against
    /// (kitty graphics, `TIOCGWINSZ`, `CSI 14/16 t`); drawn at `cellWidth x cellHeight` points.
    private func reportCellPixels() {
        let scale = window?.backingScaleFactor ?? 2
        termHandle?.setCellPixels(
            width: Int((cellWidth * scale).rounded()),
            height: Int((cellHeight * scale).rounded()),
        )
    }

    func computeGrid() -> (cols: UInt16, rows: UInt16) {
        let w = max(1, Int(bounds.width / cellWidth))
        let h = max(1, Int(bounds.height / cellHeight))
        return (UInt16(min(w, Int(UInt16.max))), UInt16(min(h, Int(UInt16.max))))
    }

    private func recomputeCellMetrics() {
        // Monospaced advance from the canonical "M", falling back to maximumAdvancement.
        let attrs: [NSAttributedString.Key: Any] = [.font: font]
        let m = NSAttributedString(string: "M", attributes: attrs)
        cellWidth = ceil(m.size().width)
        if cellWidth <= 0 { cellWidth = ceil(font.maximumAdvancement.width) }
        ascent = ceil(font.ascender)
        let descent = ceil(abs(font.descender))
        let leading = ceil(font.leading)
        cellHeight = ascent + descent + leading
        if cellHeight <= 0 { cellHeight = 16 }
        reportCellPixels()
    }

    /// Display-link callback at vsync on the main runloop. Drains per-row damage via
    /// `takeDamageRows` so invalidation covers only changed rows, adds a 2 Hz single-cell repaint
    /// while a TUI cursor blinks, and drains the OSC 52 clipboard queue through the policy gate.
    @objc private func displayLinkFired(_: CADisplayLink) {
        guard let handle = termHandle else { return }
        drainClipboardRequests(handle)
        drainChildExit(handle)
        let damage = handle.takeDamageRows()
        let blinkPhaseChanged = advanceBlinkPhase()

        // GPU path: every frame is a full redraw, so row damage collapses into `gpuFrameDirty`,
        // which a non-damage invalidation (focus, theme, selection) may already have set.
        if metalRenderer != nil {
            let hasDamage: Bool = switch damage {
            case .full: true
            case let .rows(rows): !rows.isEmpty
            }
            if hasDamage || blinkPhaseChanged {
                snapshotCache = handle.snapshot()
                gpuFrameDirty = true
            }
            renderGPUFrameIfNeeded()
            return
        }

        // Even a no-op damage drain needs a fresh snapshot when repainting (a blink toggle needs
        // the latest cursor row); `takeDamageRows` already advanced the FFI's prev-state.
        switch damage {
        case .full:
            snapshotCache = handle.snapshot()
            needsDisplay = true
        case let .rows(rows):
            guard !rows.isEmpty || blinkPhaseChanged else { return }
            snapshotCache = handle.snapshot()
            // Always include the cursor row when the blink phase flipped: otherwise a blink tick
            // coinciding with unrelated row damage would skip the cursor cell and the blink
            // would appear to stop.
            var dirty = rows
            if blinkPhaseChanged, let cursor = snapshotCache?.cursor,
               !dirty.contains(cursor.row)
            {
                dirty.append(cursor.row)
            }
            invalidateRows(dirty)
        }
    }

    /// Encode and present one Metal frame when the dirty latch is set. The latch clears only on
    /// success: after a nil drawable (occluded window) the consumed damage will not fire again,
    /// so the next tick must retry.
    private func renderGPUFrameIfNeeded() {
        guard gpuFrameDirty,
              let renderer = metalRenderer,
              let gpuTheme,
              let snap = snapshotCache
        else { return }
        let state = MetalGridRenderer.FrameState(
            viewSizePoints: bounds.size,
            transparentMode: isTransparentBgActive,
            blinkVisible: blinkVisible,
            isKeyWindow: window?.isKeyWindow ?? false,
        )
        // The renderer splits its timing into render work vs `nextDrawable` wait. Only presented
        // frames are recorded, so a nil-drawable retry does not skew the distribution.
        if renderer.render(snap: snap, theme: gpuTheme, state: state, measure: renderMetricsEnabled) {
            gpuFrameDirty = false
            if renderMetricsEnabled {
                drawableWaitMetrics.record(renderer.lastDrawableWaitNanos)
                recordFrameMetric(renderer.lastEncodeNanos)
            }
        }
        // Keep the preedit overlay anchored to the cursor while composing.
        if markedText != nil {
            let cursor = snap.cursor
            imeOverlay?.updateComposition(
                markedText: markedText,
                selectedRange: markedSelectedRange,
                cursorRow: Int(cursor.row),
                cursorCol: Int(cursor.col),
            )
        }
    }

    /// Feed one frame duration into the recorder and report windowed stats to stderr every
    /// `metricsReportInterval` frames. Only reached when `renderMetricsEnabled`.
    private func recordFrameMetric(_ ns: UInt64) {
        frameMetrics.record(ns)
        metricsFrameCounter += 1
        guard metricsFrameCounter >= Self.metricsReportInterval else { return }
        metricsFrameCounter = 0
        guard let s = frameMetrics.stats() else { return }
        let painter = metalRenderer != nil ? "gpu" : "cpu"
        let (cols, rows) = computeGrid()
        let ms = { (v: UInt64) in Double(v) / 1_000_000.0 }
        var msg = String(
            format: "[copad-metrics] painter=%@ grid=%dx%d frames=%d work["
                + "mean=%.3f p50=%.3f p95=%.3f p99=%.3f max=%.3f]ms",
            painter, Int(cols), Int(rows), s.count,
            ms(s.meanNs), ms(s.p50Ns), ms(s.p95Ns), ms(s.p99Ns), ms(s.maxNs),
        )
        // GPU only: report vsync back-pressure separately from render cost.
        if metalRenderer != nil, let d = drawableWaitMetrics.stats() {
            msg += String(
                format: " drawableWait[p50=%.3f p95=%.3f max=%.3f]ms",
                ms(d.p50Ns), ms(d.p95Ns), ms(d.maxNs),
            )
        }
        msg += "\n"
        FileHandle.standardError.write(Data(msg.utf8))
    }

    /// Turn dirty viewport rows into `setNeedsDisplay(_:)` calls. AppKit unions the rects, and
    /// layer-backed views keep pixels outside the union, so per-row invalidation stays tight.
    private func invalidateRows(_ rows: [UInt16]) {
        guard metalRenderer == nil else {
            gpuFrameDirty = true
            return
        }
        guard cellHeight > 0 else {
            needsDisplay = true
            return
        }
        let viewWidth = bounds.width
        for row in rows {
            let y = CGFloat(row) * cellHeight
            setNeedsDisplay(CGRect(x: 0, y: y, width: viewWidth, height: cellHeight))
        }
    }

    /// Apply the OSC 52 policy to a pending clipboard write: `.allow` writes to
    /// NSPasteboard.general, `.deny` drops it with a stderr warning. Reads are never served
    /// under either policy: `copad_term_take_clipboard_request` only surfaces store requests.
    private func drainClipboardRequests(_ handle: CopadTermFFI.Handle) {
        guard let text = handle.takeClipboardRequest() else { return }
        switch osc52Policy {
        case .allow:
            let pb = NSPasteboard.general
            pb.declareTypes([.string], owner: nil)
            pb.setString(text, forType: .string)
        case .deny:
            let msg = "[copad] OSC 52 clipboard write blocked (\(text.utf8.count) bytes). "
                + "Set `[security] osc52 = \"allow\"` to opt in.\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }

    /// Drain the child-exit latch: broadcast `panel.exited` (so ContextService clears per-panel
    /// state), then call `onChildExited` so the VC can close the pane (`close_on_exit`). Same
    /// order as Linux's `connect_child_exited` -> `handle_panel_exit`. Fires at most once.
    private func drainChildExit(_ handle: CopadTermFFI.Handle) {
        guard handle.takeChildExit() else { return }
        eventBus?.broadcast(event: "panel.exited", data: ["panel_id": panelID])
        onChildExited?()
    }

    /// Toggle cursor visibility once per `blinkInterval` while the snapshot reports
    /// `cursor.blink == 1`; restores it to visible when a TUI hands back a steady cursor.
    private func advanceBlinkPhase() -> Bool {
        let cursorBlink = snapshotCache?.cursor.blink ?? 0
        if cursorBlink == 1 {
            let now = Date()
            if now.timeIntervalSince(lastBlinkToggle) >= blinkInterval {
                blinkVisible.toggle()
                lastBlinkToggle = now
                return true
            }
            return false
        }
        if !blinkVisible {
            blinkVisible = true
            return true
        }
        return false
    }

    override func draw(_ dirtyRect: NSRect) {
        // GPU mode renders via the display link; AppKit should not call this.
        guard metalRenderer == nil else { return }
        guard let snap = snapshotCache,
              let ctx = NSGraphicsContext.current?.cgContext
        else { return }
        ctx.setAllowsFontSmoothing(smoothFonts)
        ctx.setShouldSmoothFonts(smoothFonts)

        // Perf harness. This times a partial repaint (`dirtyRect` rows); full-screen workloads
        // (scroll, `yes`) dirty the whole view and compare directly with the GPU full frame.
        let metricsStart = renderMetricsEnabled ? DispatchTime.now().uptimeNanoseconds : 0
        defer {
            if renderMetricsEnabled {
                recordFrameMetric(DispatchTime.now().uptimeNanoseconds - metricsStart)
            }
        }

        // Fill the bounds with theme background unless transparent-default-bg is active. AppKit
        // clips the context to `dirtyRect`, so non-dirty rows keep their backing-store pixels.
        // Transparent modes: (a) `transparentDefaultBg && imageBackgroundActive`, the image shows
        // through; (b) `windowOpacity < 1.0` (Ghostty model), where skipping the opaque fill lets
        // the layer's alpha take effect. ANSI-bg and reverse-video cells still draw opaque in
        // `drawRow` under both.
        if !isTransparentBgActive {
            ctx.setFillColor(theme.background.nsColor.cgColor)
            ctx.fill(bounds)
        }

        // CTLineDraw is y-up but the view is flipped, so flip the textMatrix or glyphs render
        // upside-down and mirrored; saved/restored to keep the flip out of later drawing.
        ctx.saveGState()
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        defer { ctx.restoreGState() }

        let graphics = snap.graphics
        let snapRows = snap.rows
        let useDirtyClip = cellHeight > 0
        let drawRows = { (pass: RowPass) in
            for row in 0 ..< snapRows {
                if useDirtyClip {
                    let rowRect = CGRect(
                        x: 0,
                        y: CGFloat(row) * self.cellHeight,
                        width: self.bounds.width,
                        height: self.cellHeight,
                    )
                    if !rowRect.intersects(dirtyRect) { continue }
                }
                let runs = snap.rowRuns(row)
                let utf8 = snap.rowUtf8(row)
                guard runs.count > 0, utf8.count > 0 else { continue }
                self.drawRow(row: row, runs: runs, utf8: utf8, ctx: ctx, pass: pass)
            }
        }
        if graphics.contains(where: { $0.z < 0 }) {
            // Images under the text (z < 0) go over the cell backgrounds, as on the GPU path.
            drawRows(.backgrounds)
            paintImages(graphics, snap: snap, ctx: ctx, aboveText: false)
            drawRows(.text)
        } else {
            drawRows(.all)
        }

        // Cursor on top of the glyphs: nvim/htop paint the cursor cell with their own highlight,
        // which covered an early-drawn cursor. Block style re-renders the glyph in
        // theme.background so the character stays readable.
        drawCursor(snap: snap, ctx: ctx)

        paintImages(graphics, snap: snap, ctx: ctx, aboveText: true)
        trimImageCache(graphics, snap: snap)

        // Selection last, so it tints over the text instead of being covered by cell bg fills.
        paintSelection(snap.selection, ctx: ctx)

        // Find match highlight in theme.accent, distinct from the selection overlay (surface2).
        paintSearchMatch(snap.searchMatch, ctx: ctx)

        paintMarkedText(snap.cursor, ctx: ctx)
    }

    /// kitty-graphics runs for the CPU painter: each run's slice of the image, clipped to its
    /// rect (the GPU path does the same with texture coordinates).
    private func paintImages(
        _ graphics: [CopadGraphic],
        snap: CopadTermFFI.Snapshot,
        ctx: CGContext,
        aboveText: Bool,
    ) {
        for g in graphics where (g.z >= 0) == aboveText {
            guard let image = cgImage(g.image, snap: snap) else { continue }
            let (uw, uh) = (CGFloat(g.uv.2 - g.uv.0), CGFloat(g.uv.3 - g.uv.1))
            guard uw > 0, uh > 0 else { continue }
            let dest = CGRect(
                x: CGFloat(g.rect.0) * cellWidth,
                y: CGFloat(g.rect.1) * cellHeight,
                width: CGFloat(g.rect.2 - g.rect.0) * cellWidth,
                height: CGFloat(g.rect.3 - g.rect.1) * cellHeight,
            )
            // Where the whole image would land for this slice to fall on `dest`.
            let full = CGRect(
                x: dest.minX - CGFloat(g.uv.0) / uw * dest.width,
                y: dest.minY - CGFloat(g.uv.1) / uh * dest.height,
                width: dest.width / uw,
                height: dest.height / uh,
            )
            ctx.saveGState()
            ctx.clip(to: dest)
            // The view is flipped; CGContext.draw expects y-up.
            ctx.translateBy(x: 0, y: full.minY + full.maxY)
            ctx.scaleBy(x: 1, y: -1)
            ctx.draw(image, in: full)
            ctx.restoreGState()
        }
    }

    private func cgImage(_ index: UInt32, snap: CopadTermFFI.Snapshot) -> CGImage? {
        guard let info = snap.graphicImage(index), let pixels = info.pixels else { return nil }
        if let cached = imageCache[info.key] { return cached }
        let (w, h) = (Int(info.width), Int(info.height))
        let data = Data(bytes: pixels, count: w * h * 4)
        guard let provider = CGDataProvider(data: data as CFData),
              let image = CGImage(
                  width: w,
                  height: h,
                  bitsPerComponent: 8,
                  bitsPerPixel: 32,
                  bytesPerRow: w * 4,
                  space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                  provider: provider,
                  decode: nil,
                  shouldInterpolate: true,
                  intent: .defaultIntent,
              )
        else { return nil }
        imageCache[info.key] = image
        return image
    }

    private func trimImageCache(_ graphics: [CopadGraphic], snap: CopadTermFFI.Snapshot) {
        guard !imageCache.isEmpty else { return }
        let shown = Set(graphics.compactMap { snap.graphicImage($0.image)?.key })
        imageCache = imageCache.filter { shown.contains($0.key) }
    }

    /// Paint the find match: one rect for a single-row match; a multi-row match (pattern crossed
    /// an autowrap) paints start_col to the last column, full middle rows, then column 0 to
    /// end_col. theme.accent at about 0.45 alpha.
    private func paintSearchMatch(_ match: CopadSearchRange, ctx: CGContext) {
        guard match.present == 1, cellWidth > 0, cellHeight > 0 else { return }
        let color = theme.accent.nsColor.withAlphaComponent(0.45).cgColor
        ctx.setFillColor(color)

        let startRow = Int(match.start_row)
        let endRow = Int(match.end_row)
        let startCol = Int(match.start_col)
        let endCol = Int(match.end_col)
        let cols = max(1, Int(bounds.width / cellWidth))
        let lastCol = cols - 1

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
            let x = CGFloat(firstCol) * cellWidth
            let w = CGFloat(finalCol - firstCol + 1) * cellWidth
            let y = CGFloat(row) * cellHeight
            ctx.fill(CGRect(x: x, y: y, width: w, height: cellHeight))
        }
    }

    /// Paint the IME composition at the cursor cell: opaque theme.background underneath, then the
    /// marked string with a single underline and a thicker double one on the highlighted range.
    private func paintMarkedText(_ cursor: CopadCursor, ctx: CGContext) {
        guard let marked = markedText, !marked.isEmpty,
              cellWidth > 0, cellHeight > 0 else { return }

        let baseAttrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor(cgColor: theme.foreground.nsColor.cgColor) ?? .white,
            .underlineStyle: NSUnderlineStyle.single.rawValue,
            .underlineColor: NSColor(cgColor: theme.accent.nsColor.cgColor) ?? .yellow,
        ]
        let attr = NSMutableAttributedString(string: marked, attributes: baseAttrs)
        // Preedit is mostly CJK/Hangul, which the base face lacks. Walk composed character
        // sequences (a half-typed Hangul syllable stays one unit) and swap `.font` per cluster
        // when the cascade picks another face.
        var loc = 0
        marked.enumerateSubstrings(
            in: marked.startIndex ..< marked.endIndex,
            options: .byComposedCharacterSequences,
        ) { cluster, _, _, _ in
            guard let cluster else { return }
            let len = (cluster as NSString).length
            let resolved = self.resolveRunFont(cluster, base: self.font)
            if resolved !== self.font {
                attr.addAttribute(.font, value: resolved, range: NSRange(location: loc, length: len))
            }
            loc += len
        }
        // Thicker double underline on the IME-highlighted sub-range.
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
        // Typographic width gives the cell count; round up to a whole cell to align the bg fill.
        var ascentT: CGFloat = 0
        var descentT: CGFloat = 0
        var leadingT: CGFloat = 0
        let width = CGFloat(CTLineGetTypographicBounds(line, &ascentT, &descentT, &leadingT))
        let cellsCovered = max(1, Int(ceil(width / cellWidth)))
        let pxWidth = CGFloat(cellsCovered) * cellWidth

        let x = CGFloat(cursor.col) * cellWidth
        let y = CGFloat(cursor.row) * cellHeight
        ctx.setFillColor(theme.background.nsColor.cgColor)
        ctx.fill(CGRect(x: x, y: y, width: pxWidth, height: cellHeight))

        // Inside the row loop's scope, so its text matrix flip is still in effect.
        ctx.textPosition = CGPoint(x: x, y: y + ascent)
        CTLineDraw(line, ctx)
    }

    /// Paint a translucent `theme.surface2` overlay over the selection. `end_row`/`end_col` are
    /// inclusive (alacritty's `SelectionRange`), so the end row paints `end_col - start_col + 1`
    /// cells.
    private func paintSelection(_ sel: CopadSelectionRange, ctx: CGContext) {
        guard sel.present == 1, cellWidth > 0, cellHeight > 0 else { return }
        let color = theme.surface2.nsColor.withAlphaComponent(0.45).cgColor
        ctx.setFillColor(color)

        let startRow = Int(sel.start_row)
        let endRow = Int(sel.end_row)
        let startCol = Int(sel.start_col)
        let endCol = Int(sel.end_col)
        let cols = max(1, Int(bounds.width / cellWidth))
        let lastCol = cols - 1

        // Block (rectangular) selection (Option+drag, `is_block` from the snapshot): each row
        // paints the same column span, already normalized (start_col <= end_col).
        if sel.is_block == 1 {
            let firstCol = max(0, min(startCol, lastCol))
            let finalCol = max(0, min(endCol, lastCol))
            guard firstCol <= finalCol else { return }
            let x = CGFloat(firstCol) * cellWidth
            let w = CGFloat(finalCol - firstCol + 1) * cellWidth
            for row in startRow ... endRow {
                let y = CGFloat(row) * cellHeight
                ctx.fill(CGRect(x: x, y: y, width: w, height: cellHeight))
            }
            return
        }

        for row in startRow ... endRow {
            // Single row: start_col..=end_col. Multi-row: start_row covers start_col..=lastCol,
            // end_row 0..=end_col, middle rows the full width.
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
            let x = CGFloat(firstCol) * cellWidth
            let w = CGFloat(finalCol - firstCol + 1) * cellWidth
            let y = CGFloat(row) * cellHeight
            ctx.fill(CGRect(x: x, y: y, width: w, height: cellHeight))
        }
    }

    /// Cursor render. Style 0 = hidden. Block (1) fills the cell and re-renders its glyph in
    /// theme.background; beam (2) is a 2-px bar at the leading edge; underline (3) a 2-px bar at
    /// the bottom. When the window is not key, block draws hollow (as Terminal.app and iTerm2).
    /// With an image background every variant gets a 1-px theme.background outline, since the
    /// accent can blend into a low-contrast wallpaper pixel.
    private func drawCursor(snap: CopadTermFFI.Snapshot, ctx: CGContext) {
        let cursor = snap.cursor
        guard cursor.style != 0,
              cellWidth > 0, cellHeight > 0,
              // Skip the OFF phase of a TUI-requested blink; steady cursors (`blink == 0`) ignore
              // `blinkVisible`.
              cursor.blink == 0 || blinkVisible
        else { return }
        let x = CGFloat(cursor.col) * cellWidth
        let y = CGFloat(cursor.row) * cellHeight
        let cell = CGRect(x: x, y: y, width: cellWidth, height: cellHeight)
        let isKey = window?.isKeyWindow ?? false
        let color = theme.accent.nsColor.cgColor
        // The outline keeps the accent cursor legible against unknown pixels behind it (image or
        // window-opacity mode).
        let needsOutline = isTransparentBgActive
        let outlineColor = theme.background.nsColor.cgColor

        switch cursor.style {
        case 1: // block
            if isKey {
                ctx.setFillColor(color)
                ctx.fill(cell)
                if needsOutline {
                    ctx.setStrokeColor(outlineColor)
                    ctx.setLineWidth(1)
                    // The stroke straddles the path; inset so the frame lands inside the cell.
                    ctx.stroke(cell.insetBy(dx: 0.5, dy: 0.5))
                }
                redrawCursorGlyph(snap: snap, ctx: ctx)
            } else {
                ctx.setStrokeColor(color)
                ctx.setLineWidth(1)
                ctx.stroke(cell.insetBy(dx: 0.5, dy: 0.5))
            }
        case 2: // beam (bar)
            let barWidth: CGFloat = 2
            let rect = CGRect(x: x, y: y, width: barWidth, height: cellHeight)
            ctx.setFillColor(color)
            ctx.fill(rect)
            if needsOutline {
                ctx.setStrokeColor(outlineColor)
                ctx.setLineWidth(1)
                ctx.stroke(rect.insetBy(dx: 0.5, dy: 0.5))
            }
        case 3: // underline
            let barHeight: CGFloat = 2
            let rect = CGRect(x: x, y: y + cellHeight - barHeight, width: cellWidth, height: barHeight)
            ctx.setFillColor(color)
            ctx.fill(rect)
            if needsOutline {
                ctx.setStrokeColor(outlineColor)
                ctx.setLineWidth(1)
                ctx.stroke(rect.insetBy(dx: 0.5, dy: 0.5))
            }
        default:
            break
        }
    }

    /// Paint the cursor-cell glyph in theme.background against the accent block, keeping the
    /// run's bold/italic flags.
    private func redrawCursorGlyph(snap: CopadTermFFI.Snapshot, ctx: CGContext) {
        let cursor = snap.cursor
        let runs = snap.rowRuns(cursor.row)
        let utf8 = snap.rowUtf8(cursor.row)

        // Runs are per cell (or wide-cell pair), so the cursor is in exactly one; a wide char's
        // cursor is on the leading half, so start_col == cursor.col holds.
        var hit: CopadRun?
        for i in 0 ..< runs.count {
            let r = runs[i]
            if r.start_col <= cursor.col, cursor.col < r.end_col {
                hit = r
                break
            }
        }
        guard let run = hit else { return }

        let len = Int(run.utf8_len)
        let offset = Int(run.utf8_offset)
        guard offset + len <= utf8.count else { return }

        // Byte range and draw position, three shapes: (1) aggregated uniform-ASCII run: draw
        // exactly one byte at the cursor cell's x, since the full run would overpaint neighbours;
        // (2) wide char (WIDE_LEADING, 2 cells) and (3) single cell with combining marks: draw the
        // full utf8 at the run's start (= cursor.col).
        let flagBold: UInt16 = 1 << 0
        let flagItalic: UInt16 = 1 << 1
        let flagWideLeading: UInt16 = 1 << 7
        let runSpan = run.end_col - run.start_col
        let isWide = run.flags & flagWideLeading != 0
        let isAggregatedUniform = !isWide && runSpan > 1

        let drawBytes: UnsafeBufferPointer<UInt8>
        let drawX: CGFloat
        if isAggregatedUniform {
            drawBytes = UnsafeBufferPointer(rebasing: utf8[offset ..< offset + 1])
            drawX = CGFloat(cursor.col) * cellWidth
        } else {
            drawBytes = UnsafeBufferPointer(rebasing: utf8[offset ..< offset + len])
            drawX = CGFloat(run.start_col) * cellWidth
        }
        guard
            let str = String(bytes: drawBytes, encoding: .utf8),
            !str.isEmpty,
            str != " "
        else { return }

        let isBold = run.flags & flagBold != 0
        let isItalic = run.flags & flagItalic != 0
        let baseRunFont: NSFont = switch (isBold, isItalic) {
        case (true, true): boldItalicFont
        case (true, false): boldFont
        case (false, true): italicFont
        case (false, false): font
        }
        let runFont = resolveRunFont(str, base: baseRunFont)

        // Inverted color (fg = theme.background) for legibility on the accent block; no
        // underline or strike.
        let cursorFgRGBA = cgColorToRGBA32(theme.background.nsColor.cgColor)
        var styleBits: UInt16 = 0
        if isBold { styleBits |= Self.styleBitBold }
        if isItalic { styleBits |= Self.styleBitItalic }
        let line = ctLineFor(
            text: str,
            font: runFont,
            fgRGBA: cursorFgRGBA,
            styleBits: styleBits,
            underlineRGBA: 0,
            strikeRGBA: 0,
        )
        let baselineY = CGFloat(cursor.row) * cellHeight + ascent
        ctx.textPosition = CGPoint(x: drawX, y: baselineY)
        CTLineDraw(line, ctx)
    }

    /// Which part of a row `drawRow` paints: both, or the cell backgrounds and the text in
    /// separate passes when an image must go between them (kitty z < 0).
    private enum RowPass {
        case all, backgrounds, text
    }

    private func drawRow(
        row: UInt16,
        runs: UnsafeBufferPointer<CopadRun>,
        utf8: UnsafeBufferPointer<UInt8>,
        ctx: CGContext,
        pass: RowPass = .all,
    ) {
        let baselineY = CGFloat(row) * cellHeight + ascent
        let defaultFg = theme.foreground.nsColor.cgColor
        let defaultBg = theme.background.nsColor.cgColor

        // Flag bits mirror copad_term::flags (copad-term/src/lib.rs).
        let flagBold: UInt16 = 1 << 0
        let flagItalic: UInt16 = 1 << 1
        let flagInverse: UInt16 = 1 << 3
        let flagDim: UInt16 = 1 << 4
        let flagStrike: UInt16 = 1 << 5

        let transparentMode = isTransparentBgActive

        for i in 0 ..< runs.count {
            let run = runs[i]

            // Whether the bg came from the default sentinel (`run.bg_rgba == 0`), decided before
            // resolving: an explicit ANSI bg equal to theme.background must still paint, while a
            // default cell must not in transparent mode.
            let bgIsDefault = run.bg_rgba == 0
            let isInverse = run.flags & flagInverse != 0

            // Resolve colors, then apply the inverse swap. Default-bg materializes to
            // theme.background before the swap, or reverse-video over transparent bg is invisible.
            var fg = resolveColor(run.fg_rgba, defaultColor: defaultFg)
            var bg = resolveColor(run.bg_rgba, defaultColor: defaultBg)
            if isInverse {
                swap(&fg, &bg)
            }
            // ANSI leaves dim intensity unspecified; ~65% is the conventional value.
            if run.flags & flagDim != 0, let dimmed = fg.copy(alpha: 0.65) {
                fg = dimmed
            }

            let x = CGFloat(run.start_col) * cellWidth
            let cellsWide = CGFloat(run.end_col - run.start_col)
            let cellRect = CGRect(x: x, y: CGFloat(row) * cellHeight,
                                  width: cellsWide * cellWidth, height: cellHeight)

            // Per-cell bg fill overrides the bounds fill. Opaque mode skips when the resolved bg
            // equals theme.bg (already filled). Transparent mode skips only default-sentinel,
            // non-inverse cells; inverse + default-bg is opaque theme.fg after the swap and must
            // still paint.
            let skipFill = transparentMode
                ? (bgIsDefault && !isInverse)
                : cgColorsApproxEqual(bg, defaultBg)
            if !skipFill, pass != .text {
                ctx.setFillColor(bg)
                ctx.fill(cellRect)
            }
            if pass == .backgrounds { continue }

            let len = Int(run.utf8_len)
            let offset = Int(run.utf8_offset)
            guard offset + len <= utf8.count else { continue }
            guard
                let str = String(bytes: UnsafeBufferPointer(rebasing: utf8[offset ..< offset + len]), encoding: .utf8),
                !str.isEmpty
            else { continue }

            let isBold = run.flags & flagBold != 0
            let isItalic = run.flags & flagItalic != 0
            let baseRunFont: NSFont = switch (isBold, isItalic) {
            case (true, true): boldItalicFont
            case (true, false): boldFont
            case (false, true): italicFont
            case (false, false): font
            }
            let runFont = resolveRunFont(str, base: baseRunFont)
            let fgRGBA = cgColorToRGBA32(fg)
            // styleBits goes into the cache key. The underline byte is the raw FFI value (the
            // draw folds non-zero to single for now) so a future richer variant cannot reuse a
            // stale line.
            var styleBits = UInt16(run.underline_style)
            if isBold { styleBits |= Self.styleBitBold }
            if isItalic { styleBits |= Self.styleBitItalic }
            let isStrike = run.flags & flagStrike != 0
            if isStrike { styleBits |= Self.styleBitStrike }
            let ulRGBA: UInt32 = if run.underline_style != 0 {
                cgColorToRGBA32(
                    run.underline_color_rgba == 0
                        ? fg
                        : resolveColor(run.underline_color_rgba, defaultColor: fg),
                )
            } else { 0 }
            let strRGBA: UInt32 = isStrike ? fgRGBA : 0
            let line = ctLineFor(
                text: str,
                font: runFont,
                fgRGBA: fgRGBA,
                styleBits: styleBits,
                underlineRGBA: ulRGBA,
                strikeRGBA: strRGBA,
            )
            ctx.textPosition = CGPoint(x: x, y: baselineY)
            CTLineDraw(line, ctx)
            if run.underline_style != 0 {
                strokeUnderline(
                    ctx: ctx,
                    style: run.underline_style,
                    x: x,
                    width: cellsWide * cellWidth,
                    centerY: baselineY - runFont.underlinePosition,
                    thickness: max(1, runFont.underlineThickness),
                    color: ulRGBA,
                )
            }
        }
    }

    /// SGR 4:1..4:5 for the CPU painter, matching the GPU shader: lines snapped to device pixels,
    /// patterns phased on absolute x so neighbouring runs join up.
    private func strokeUnderline(
        ctx: CGContext,
        style: UInt8,
        x: CGFloat,
        width: CGFloat,
        centerY: CGFloat,
        thickness: CGFloat,
        color: UInt32,
    ) {
        if style == 3 {
            strokeCurlyUnderline(ctx: ctx, x: x, width: width, centerY: centerY, thickness: thickness, color: color)
            return
        }
        let scale = window?.backingScaleFactor ?? 2
        let t = max(1, (thickness * scale).rounded()) / scale
        let top = ((centerY - t / 2) * scale).rounded() / scale
        ctx.saveGState()
        ctx.clip(to: CGRect(x: x, y: top, width: width, height: t * 3))
        ctx.setFillColor(Self.cgColor(color))
        switch style {
        case 2:
            ctx.fill(CGRect(x: x, y: top, width: width, height: t))
            ctx.fill(CGRect(x: x, y: top + 2 * t, width: width, height: t))
        case 4, 5:
            // dotted: t on, t off; dashed: 3t on, 2t off.
            let (on, period) = style == 4 ? (t, 2 * t) : (3 * t, 5 * t)
            var px = (x / period).rounded(.down) * period
            while px < x + width {
                ctx.fill(CGRect(x: px, y: top, width: on, height: t))
                px += period
            }
        default:
            ctx.fill(CGRect(x: x, y: top, width: width, height: t))
        }
        ctx.restoreGState()
    }

    private static func cgColor(_ rgba: UInt32) -> CGColor {
        CGColor(
            red: CGFloat((rgba >> 24) & 0xFF) / 255,
            green: CGFloat((rgba >> 16) & 0xFF) / 255,
            blue: CGFloat((rgba >> 8) & 0xFF) / 255,
            alpha: CGFloat(rgba & 0xFF) / 255,
        )
    }

    private func strokeCurlyUnderline(
        ctx: CGContext,
        x: CGFloat,
        width: CGFloat,
        centerY: CGFloat,
        thickness: CGFloat,
        color: UInt32,
    ) {
        let amplitude = thickness * 1.2
        let half = max(2, amplitude * 4)
        let path = CGMutablePath()
        var px = (x / half).rounded(.down) * half
        var up = Int((px / half).rounded()) % 2 == 0
        path.move(to: CGPoint(x: px, y: centerY))
        while px < x + width {
            let mid = CGPoint(x: px + half / 2, y: centerY + (up ? -amplitude : amplitude) * 2)
            path.addQuadCurve(to: CGPoint(x: px + half, y: centerY), control: mid)
            px += half
            up.toggle()
        }
        ctx.saveGState()
        ctx.clip(to: CGRect(x: x, y: centerY - amplitude * 3, width: width, height: amplitude * 6))
        ctx.addPath(path)
        ctx.setStrokeColor(Self.cgColor(color))
        ctx.setLineWidth(thickness)
        ctx.strokePath()
        ctx.restoreGState()
    }

    /// Cheap component-wise equality for the "bg same as the bounds fill" early-out; falls back
    /// to ObjectIdentifier when components are not comparable (mixed color spaces).
    private func cgColorsApproxEqual(_ a: CGColor, _ b: CGColor) -> Bool {
        guard let ac = a.components, let bc = b.components, ac.count == bc.count else { return false }
        for (x, y) in zip(ac, bc) where abs(x - y) > 0.001 {
            return false
        }
        return true
    }

    // MARK: - Mouse selection

    /// Window-coord mouse event to a grid (row, col, side), clamping out-of-bounds drag positions
    /// (dragging past an edge is normal) so the FFI gets a valid `UInt16`.
    private func gridLocation(for event: NSEvent) -> (row: UInt16, col: UInt16, side: CopadTermFFI.Handle.CellSide)? {
        guard cellWidth > 0, cellHeight > 0 else { return nil }
        let local = convert(event.locationInWindow, from: nil)
        let maxCol = max(0, Int(bounds.width / cellWidth) - 1)
        let maxRow = max(0, Int(bounds.height / cellHeight) - 1)
        let col = min(maxCol, max(0, Int(local.x / cellWidth)))
        let row = min(maxRow, max(0, Int(local.y / cellHeight)))
        let xInCell = max(0, local.x - CGFloat(col) * cellWidth)
        let side: CopadTermFFI.Handle.CellSide = xInCell < cellWidth / 2 ? .left : .right
        return (UInt16(clamping: row), UInt16(clamping: col), side)
    }

    /// 1 click = simple drag selection, 2 = semantic (word), 3+ = lines. Option on a single click
    /// gives block selection, as in Terminal.app and iTerm2 (Cmd is reserved for URL click).
    private func selectionKind(for event: NSEvent) -> CopadTermFFI.Handle.SelectionKind {
        switch event.clickCount {
        case 2: .word
        case let n where n >= 3: .line
        default: event.modifierFlags.contains(.option) ? .block : .simple
        }
    }

    /// When a TUI has mouse reporting on (vim `set mouse=a`, less, htop, tmux mouse), clicks and
    /// drags go to the TUI; Shift overrides so the user can still select text. Forwarding
    /// (`forwardMouseEvent`) covers wheel, press/release, drag, and bare motion at the MOTION
    /// level (see `mouseMoved(with:)` and `updateTrackingAreas`).
    private func shouldHandleAsSelection(_ event: NSEvent) -> Bool {
        if event.modifierFlags.contains(.shift) { return true }
        return !(termHandle?.mouseModeActive ?? false)
    }

    // MARK: - Mouse event forwarding (mouse-mode TUIs)

    /// "press" emits SGR `M` (or the legacy press byte); "release" SGR `m` (legacy: button 3);
    /// "motion" SGR `M` with bit 32 set. Used only when a TUI has mouse reporting on and Shift is
    /// not held.
    private enum MouseForwardKind { case press, release, motion }

    /// Last cell forwarded by a motion event; -1 means the next motion emits unconditionally.
    /// Reset on every press/release so the first drag tick fires even within one cell.
    private var lastMotionCol: Int = -1
    private var lastMotionRow: Int = -1

    private func forwardMouseEvent(event: NSEvent, button: Int, kind: MouseForwardKind) {
        guard let h = termHandle else { return }
        let encoding = h.mouseEncoding
        guard encoding != .none else { return }
        let (col, row) = mouseCellCoord(for: event)
        if kind == .motion {
            // Cell-granularity throttle: the TUI does not need sub-cell precision.
            guard col != lastMotionCol || row != lastMotionRow else { return }
            lastMotionCol = col
            lastMotionRow = row
        } else {
            lastMotionCol = -1
            lastMotionRow = -1
        }
        let modBits = mouseModifierBits(event)
        let motionBit = (kind == .motion) ? 32 : 0
        // SGR keeps the button through release; legacy/UTF8 collapse releases to 3 (xterm).
        let buttonCode: Int = switch (encoding, kind) {
        case (.legacy, .release), (.utf8, .release):
            3 | modBits
        default:
            button | modBits | motionBit
        }
        sendMouseBytes(
            buttonCode: buttonCode,
            col: col, row: row, kind: kind,
            encoding: encoding, handle: h,
        )
    }

    /// 1-based grid coords clamped to the visible grid extent, not just UInt16: dragging past an
    /// edge yields out-of-bounds coordinates, and an unclamped report (row 65000) is ignored by
    /// the TUI instead of acting as drag-to-edge. Anchored at the mouse, like `forwardWheel`.
    private func mouseCellCoord(for event: NSEvent) -> (col: Int, row: Int) {
        guard cellWidth > 0, cellHeight > 0 else { return (1, 1) }
        let local = convert(event.locationInWindow, from: nil)
        let maxCol = max(1, Int(bounds.width / cellWidth))
        let maxRow = max(1, Int(bounds.height / cellHeight))
        let col = max(1, min(Int((local.x / cellWidth).rounded(.down)) + 1, maxCol))
        let row = max(1, min(Int((local.y / cellHeight).rounded(.down)) + 1, maxRow))
        return (col, row)
    }

    /// xterm mouse-mode modifier bits: 4=shift, 8=alt/meta, 16=ctrl. Shift is never set: held
    /// Shift bypasses forwarding entirely (`shouldHandleAsSelection`), so "Shift = host
    /// selection" stays clear to the TUI.
    private func mouseModifierBits(_ event: NSEvent) -> Int {
        var bits = 0
        if event.modifierFlags.contains(.option) { bits |= 8 }
        if event.modifierFlags.contains(.control) { bits |= 16 }
        return bits
    }

    private func sendMouseBytes(
        buttonCode: Int,
        col: Int, row: Int, kind: MouseForwardKind,
        encoding: CopadTermFFI.Handle.MouseEncoding,
        handle: CopadTermFFI.Handle,
    ) {
        switch encoding {
        case .sgr:
            let suffix = (kind == .release) ? "m" : "M"
            let s = "\u{1B}[<\(buttonCode);\(col);\(row)\(suffix)"
            handle.input(Array(s.utf8))
        case .legacy, .utf8:
            // Single-byte coords cap at 223 (mouse spec); +32 offset as in `forwardWheel`.
            let cb = UInt8(min(255, buttonCode + 32))
            let cc = UInt8(min(255, col + 32))
            let cr = UInt8(min(255, row + 32))
            handle.input([0x1B, 0x5B, 0x4D, cb, cc, cr])
        case .none:
            return
        }
    }

    /// Whether a drag forwards as a motion report: CLICK level wants press/release only.
    private func shouldForwardDragMotion() -> Bool {
        switch termHandle?.mouseReportLevel ?? .none {
        case .drag, .motion: true
        case .none, .click: false
        }
    }

    /// MOTION-level reporting (`\e[?1003h`) wants bare cursor moves too. AppKit does not deliver
    /// `mouseMoved` by default, so a tracking area is installed once and gated per event in the
    /// handler; `inVisibleRect` keeps it in sync with the bounds across resizes.
    private var motionTrackingArea: NSTrackingArea?

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let area = motionTrackingArea {
            removeTrackingArea(area)
        }
        let area = NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .activeInKeyWindow, .inVisibleRect],
            owner: self,
            userInfo: nil,
        )
        addTrackingArea(area)
        motionTrackingArea = area
    }

    override func mouseMoved(with event: NSEvent) {
        // Gated at handler time, not area install time, so a TUI flipping between `.motion` and
        // `.click` does not thrash the area. Bare motion reports only when the TUI subscribed via
        // `\e[?1003h`.
        if termHandle?.mouseReportLevel == .motion {
            forwardMouseEvent(event: event, button: 3, kind: .motion)
            return
        }
        super.mouseMoved(with: event)
    }

    override func mouseDown(with event: NSEvent) {
        // Always take first responder on click, even when mouse-mode TUI handling bails out;
        // otherwise Cmd+C and keyboard input have no responder target.
        window?.makeFirstResponder(self)

        // Cmd+click takes priority over selection: OSC 8 hyperlink first, then the plain-text
        // URL regex on the clicked row.
        if event.modifierFlags.contains(.command) {
            if openURLAtClick(event) {
                return
            }
            // No URL there: fall through so the click starts a selection.
        }

        if !shouldHandleAsSelection(event) {
            forwardMouseEvent(event: event, button: 0, kind: .press)
            return
        }
        guard let (row, col, side) = gridLocation(for: event), let h = termHandle else {
            super.mouseDown(with: event)
            return
        }
        h.selectionStart(row: row, col: col, side: side, kind: selectionKind(for: event))
        requestRepaint()
    }

    override func mouseUp(with event: NSEvent) {
        if !shouldHandleAsSelection(event) {
            forwardMouseEvent(event: event, button: 0, kind: .release)
            return
        }
        super.mouseUp(with: event)
    }

    override func rightMouseDown(with event: NSEvent) {
        if !shouldHandleAsSelection(event) {
            forwardMouseEvent(event: event, button: 2, kind: .press)
            return
        }
        super.rightMouseDown(with: event)
    }

    override func rightMouseUp(with event: NSEvent) {
        if !shouldHandleAsSelection(event) {
            forwardMouseEvent(event: event, button: 2, kind: .release)
            return
        }
        super.rightMouseUp(with: event)
    }

    override func rightMouseDragged(with event: NSEvent) {
        if !shouldHandleAsSelection(event), shouldForwardDragMotion() {
            forwardMouseEvent(event: event, button: 2, kind: .motion)
            return
        }
        super.rightMouseDragged(with: event)
    }

    override func otherMouseDown(with event: NSEvent) {
        if !shouldHandleAsSelection(event) {
            forwardMouseEvent(event: event, button: 1, kind: .press)
            return
        }
        super.otherMouseDown(with: event)
    }

    override func otherMouseUp(with event: NSEvent) {
        if !shouldHandleAsSelection(event) {
            forwardMouseEvent(event: event, button: 1, kind: .release)
            return
        }
        super.otherMouseUp(with: event)
    }

    override func otherMouseDragged(with event: NSEvent) {
        if !shouldHandleAsSelection(event), shouldForwardDragMotion() {
            forwardMouseEvent(event: event, button: 1, kind: .motion)
            return
        }
        super.otherMouseDragged(with: event)
    }

    /// Resolve the URL at a Cmd+click point and open it via NSWorkspace; true if one opened.
    /// Checks OSC 8 hyperlinks first, then URLClickHelper's plain-text regex.
    private func openURLAtClick(_ event: NSEvent) -> Bool {
        guard let snap = snapshotCache,
              let (row, col, _) = gridLocation(for: event)
        else { return false }

        // OSC 8: find a run with hyperlink_id != 0 whose `start_col..<end_col` covers the column.
        let runs = snap.rowRuns(row)
        for i in 0 ..< runs.count {
            let r = runs[i]
            if r.hyperlink_id != 0, col >= r.start_col, col < r.end_col,
               let uri = snap.hyperlinkURI(r.hyperlink_id),
               let url = URL(string: uri)
            {
                NSWorkspace.shared.open(url)
                return true
            }
        }

        // Plain text: find a URL regex match containing the clicked column. NSRegularExpression
        // works on UTF-16 units, which line up with columns for ASCII-dominant URLs; wide chars
        // upstream shift the offset (URLClickHelper makes the same trade-off).
        let utf8 = snap.rowUtf8(row)
        guard utf8.count > 0,
              let lineText = String(bytes: UnsafeBufferPointer(start: utf8.baseAddress, count: utf8.count), encoding: .utf8)
        else { return false }

        let ns = lineText as NSString
        let fullRange = NSRange(location: 0, length: ns.length)
        let matches = URLClickHelper.urlRegex.matches(in: lineText, options: [], range: fullRange)
        for match in matches where match.range.contains(Int(col)) {
            let candidate = ns.substring(with: match.range)
            let trimmed = URLClickHelper.trimURLTrailingPunctuation(candidate)
            if let url = URL(string: trimmed) {
                NSWorkspace.shared.open(url)
                return true
            }
        }
        return false
    }

    override func mouseDragged(with event: NSEvent) {
        if !shouldHandleAsSelection(event) {
            // The TUI owns the drag: forward it as motion with button 1 at DRAG/MOTION level.
            // CLICK-level subscribers see only press/release.
            if shouldForwardDragMotion() {
                forwardMouseEvent(event: event, button: 0, kind: .motion)
            }
            return
        }
        guard let (row, col, side) = gridLocation(for: event), let h = termHandle else {
            return
        }
        h.selectionUpdate(row: row, col: col, side: side)
        requestRepaint()
    }

    // MARK: - Scrolling

    /// Mouse wheel / trackpad scroll: maps `scrollingDeltaY` to a line count and shifts alacritty's
    /// `display_offset`. Positive deltaY brings older content into view (`scrollLines(positive)`).
    override func scrollWheel(with event: NSEvent) {
        guard let h = termHandle, cellHeight > 0 else {
            super.scrollWheel(with: event)
            return
        }
        let dy = event.scrollingDeltaY
        let lines: Int
        if event.hasPreciseScrollingDeltas {
            // Trackpad: accumulate fractional deltas so slow swipes do not round to zero.
            accumulatedScrollDelta += dy
            let whole = (accumulatedScrollDelta / cellHeight).rounded(.towardZero)
            lines = Int(whole)
            accumulatedScrollDelta -= whole * cellHeight
        } else {
            // Mouse wheel: `scrollingDeltaY` is about one per notch; no accumulator, rounding
            // toward zero.
            lines = Int(dy.rounded(.towardZero))
        }
        if lines == 0 { return }

        // With mouse reporting on and no Shift override, forward wheel events to the PTY so the
        // TUI's own scrollback (tmux copy mode, less, nvim) advances.
        let encoding = h.mouseEncoding
        if encoding != .none, !event.modifierFlags.contains(.shift) {
            forwardWheel(event: event, lines: lines, encoding: encoding, handle: h)
            return
        }

        h.scrollLines(Int32(lines))
        // No needsDisplay: the display link sees the state change within a tick and redraws with a
        // fresh snapshot. Marking dirty here drew the stale snapshotCache first, then again.
    }

    /// Send `\e[<64;col;rowM` (SGR wheel-up) or `\e[<65;col;rowm` (wheel-down) per line, as xterm
    /// would. Legacy/UTF8 pad coords by 32, capped at 223. One event per line, not coalesced, so
    /// the TUI's scroll rate follows the input rate.
    private func forwardWheel(
        event: NSEvent,
        lines: Int,
        encoding: CopadTermFFI.Handle.MouseEncoding,
        handle: CopadTermFFI.Handle,
    ) {
        // 1-based grid coords, anchored at the mouse position (as xterm) so tmux picks the right
        // pane.
        let local = convert(event.locationInWindow, from: nil)
        let col = max(1, min(Int((local.x / cellWidth).rounded(.down)) + 1, Int(UInt16.max)))
        let row = max(1, min(Int((local.y / cellHeight).rounded(.down)) + 1, Int(UInt16.max)))
        // SGR/X10 wheel buttons: 64 = up (older content), 65 = down; `lines > 0` is older content.
        let button = lines > 0 ? 64 : 65
        let count = abs(lines)
        for _ in 0 ..< count {
            switch encoding {
            case .sgr:
                let s = "\u{1B}[<\(button);\(col);\(row)M"
                handle.input(Array(s.utf8))
            case .legacy, .utf8:
                // Coord byte = value + 32, capped at 255. UTF8 mode allows 2-byte coords above
                // 223, which this path does not use.
                let cb = UInt8(min(255, button + 32))
                let cc = UInt8(min(255, col + 32))
                let cr = UInt8(min(255, row + 32))
                handle.input([0x1B, 0x5B, 0x4D, cb, cc, cr])
            case .none:
                return
            }
        }
    }

    /// Return to the live bottom before user input reaches the PTY, so typing is visible. PTY
    /// output leaves a scrolled-back view alone.
    private func scrollToBottomOnInput() {
        termHandle?.scrollToBottom()
    }

    // MARK: - Clipboard / Edit responder actions

    /// Cmd+C via the Edit menu key equivalent, which AppKit dispatches through the responder
    /// chain before keyDown. No selection is a no-op so the chain continues (as Terminal.app).
    /// `@objc` without `override`: NSResponder's cut/copy/paste are informal actions in Swift.
    @objc func copy(_: Any?) {
        guard let text = termHandle?.selectionString(), !text.isEmpty else { return }
        let pb = NSPasteboard.general
        pb.declareTypes([.string], owner: nil)
        pb.setString(text, forType: .string)
    }

    @objc func paste(_: Any?) {
        guard let text = NSPasteboard.general.string(forType: .string), !text.isEmpty else { return }
        sendPaste(text)
    }

    @objc override func selectAll(_: Any?) {
        termHandle?.selectionAll()
        requestRepaint()
    }

    /// Cmd+V. Wraps the bytes in bracketed-paste markers when the program enabled `\e[?2004h`.
    /// The `terminal.output` event carries the paste text without the wrapper, as VTE's `commit`
    /// does on Linux.
    private func sendPaste(_ text: String) {
        guard let h = termHandle else { return }
        scrollToBottomOnInput()
        let bytes = Array(text.utf8)
        publishTerminalOutput(bytes)
        if h.bracketedPasteActive {
            h.input([0x1B, 0x5B, 0x32, 0x30, 0x30, 0x7E]) // ESC [ 2 0 0 ~
            h.input(bytes)
            h.input([0x1B, 0x5B, 0x32, 0x30, 0x31, 0x7E]) // ESC [ 2 0 1 ~
        } else {
            h.input(bytes)
        }
    }

    // MARK: - Drag-drop

    /// Required for AppKit to emit `performDragOperation`; `.copy` as nothing is moved.
    override func draggingEntered(_: NSDraggingInfo) -> NSDragOperation {
        .copy
    }

    /// Turns the dropped pasteboard into one shell-quoted text payload and pastes it via
    /// `sendPaste` (bracketed paste + `terminal.output`). In priority order: file URLs (quoted
    /// absolute paths joined by spaces); raw image bytes (saved as PNG to
    /// `~/Library/Caches/copad/drops/`, path pasted); web URLs (bare text). Returns `false`
    /// when nothing was usable.
    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        let pb = sender.draggingPasteboard

        // Priority 1: file URLs. `urlReadingFileURLsOnly` leaves web URLs for priority 3;
        // Finder exports `.fileURL` alongside `.URL` and the local path is wanted.
        if let fileURLs = pb.readObjects(
            forClasses: [NSURL.self],
            options: [.urlReadingFileURLsOnly: true],
        ) as? [URL], !fileURLs.isEmpty {
            let quoted = fileURLs.map { Self.shellQuote($0.path) }.joined(separator: " ")
            sendPaste(quoted)
            return true
        }

        // Priority 2: raw image bytes, PNG first and TIFF as fallback (NSImage drags land as
        // `.tiff`); both are normalized to PNG on disk.
        let imageData = pb.data(forType: .png) ?? pb.data(forType: .tiff)
        if let data = imageData,
           let path = saveDroppedImage(data, prefersPng: pb.data(forType: .png) != nil)
        {
            sendPaste(Self.shellQuote(path))
            return true
        }

        // Priority 3: non-file URLs (web links), pasted as text for the receiving CLI to handle.
        if let urls = pb.readObjects(forClasses: [NSURL.self], options: nil) as? [URL],
           let url = urls.first
        {
            sendPaste(url.absoluteString)
            return true
        }

        return false
    }

    /// POSIX single-quote escape: wraps in `'...'` with every `'` becoming `'\''`; the empty
    /// string becomes `''`.
    private static func shellQuote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// Save a TIFF/PNG blob as PNG under `~/Library/Caches/copad/drops/<timestamp>-<n>.png`;
    /// returns the absolute path, or nil if the directory or re-encoding fails. PNG input is
    /// re-encoded too, so the file always matches its `.png` extension.
    private func saveDroppedImage(_ data: Data, prefersPng: Bool) -> String? {
        _ = prefersPng // routed through the same encoder either way
        guard let dir = Self.dropsCacheDir() else { return nil }
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy-MM-dd-HHmmss"
        let stamp = formatter.string(from: Date())
        // Random suffix so back-to-back drops in one second do not collide.
        let suffix = String(format: "%04x", UInt32.random(in: 0 ... 0xFFFF))
        let filename = "\(stamp)-\(suffix).png"
        let url = dir.appendingPathComponent(filename)

        guard let image = NSImage(data: data),
              let tiff = image.tiffRepresentation,
              let rep = NSBitmapImageRep(data: tiff),
              let png = rep.representation(using: .png, properties: [:])
        else {
            return nil
        }
        do {
            try png.write(to: url, options: .atomic)
            return url.path
        } catch {
            FileHandle.standardError.write(
                Data("[copad] drag-drop: failed to write \(url.path): \(error)\n".utf8),
            )
            return nil
        }
    }

    /// `~/Library/Caches/copad/drops/`, created lazily; nil (logged) if it cannot be created.
    private static func dropsCacheDir() -> URL? {
        let fm = FileManager.default
        guard let caches = fm.urls(for: .cachesDirectory, in: .userDomainMask).first else {
            return nil
        }
        let dir = caches.appendingPathComponent("copad/drops", isDirectory: true)
        do {
            try fm.createDirectory(at: dir, withIntermediateDirectories: true)
            return dir
        } catch {
            FileHandle.standardError.write(
                Data("[copad] drag-drop: failed to create \(dir.path): \(error)\n".utf8),
            )
            return nil
        }
    }

    // MARK: - PTY input dispatch

    /// Central PTY-input dispatch for keyboard paths (typed text, IME commits, control combos,
    /// shortcuts, special keys): writes to the PTY and publishes `terminal.output`, like
    /// `copad-linux/src/tabs.rs`'s VTE `connect_commit` hook. Mouse-mode wheel forwarding stays on
    /// raw `termHandle.input`, as VTE excludes mouse from `commit`.
    private func sendInput(_ bytes: [UInt8]) {
        termHandle?.input(bytes)
        publishTerminalOutput(bytes)
    }

    /// Broadcast a `terminal.output` event with the bytes decoded as utf8; non-utf8 input is
    /// dropped silently.
    private func publishTerminalOutput(_ bytes: [UInt8]) {
        guard let bus = eventBus, !bytes.isEmpty,
              let text = String(bytes: bytes, encoding: .utf8),
              !text.isEmpty
        else { return }
        bus.broadcast(event: "terminal.output", data: [
            "panel_id": panelID,
            "text": text,
        ])
    }

    // MARK: - Keyboard

    /// Routes keyDown through `interpretKeyEvents` so the system IME (Korean 2-Set, Japanese)
    /// delivers committed text; otherwise IME input is dropped. Ctrl-letter combinations bypass
    /// the IME since shells and TUIs need the raw control bytes (Ctrl+C = 0x03). Cmd-modified keys
    /// go to the responder chain via super.
    override func keyDown(with event: NSEvent) {
        let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        // Cmd+Up/Down (line), Cmd+Home/End (top/bottom), Shift+PageUp/Down (page): host-side
        // viewport controls, not forwarded to the PTY.
        if handleScrollKey(event, mods: mods) {
            return
        }
        if mods.contains(.command) {
            // Menu key equivalents fired in performKeyEquivalent already. Map the readline-style
            // line-edit combos (Cmd+arrows/backspace/delete) here; super.keyDown would beep.
            if let bytes = commandKeyBytes(forKeyCode: event.keyCode) {
                scrollToBottomOnInput()
                sendInput(bytes)
                return
            }
            super.keyDown(with: event)
            return
        }
        // A program that negotiated the kitty keyboard protocol gets every key in its form
        // (Shift+Enter as `CSI 13;2u`); text still goes through the IME path below.
        if kittyKeyDown(event) {
            return
        }
        if mods == .control, let bytes = controlBytes(for: event) {
            // Jump back to the bottom so the keypress lands visibly.
            scrollToBottomOnInput()
            sendInput(bytes)
            return
        }
        // Force-Meta keys: Option + Return / Escape (the subset in `force_meta_keys`) send
        // `ESC + <byte>` regardless of `optionAsAlt`, which drops control-char keys. The default
        // includes Return for newline-in-prompt in Claude Code and Python REPLs.
        if mods.subtracting(.shift) == .option,
           forceMetaKeyCodes.contains(event.keyCode),
           let bytes = forceMetaBytes(forKeyCode: event.keyCode)
        {
            scrollToBottomOnInput()
            sendInput(bytes)
            return
        }
        // Option-as-Alt: send Option+key as `ESC + base_char` so tmux/zsh/readline see the Meta
        // prefix instead of the IME dead-key (Option+1 giving `¡`). Shift may coexist.
        // Non-printable keys fall through to interpretKeyEvents so moveWordLeft/Right and
        // deleteWordBackward keep working.
        if optionAsAlt,
           mods.subtracting(.shift) == .option,
           let bytes = optionAltBytes(for: event)
        {
            scrollToBottomOnInput()
            sendInput(bytes)
            return
        }
        interpretKeyEvents([event])
    }

    // MARK: - kitty keyboard protocol (decisions #164)

    /// keyCodes whose press went out encoded, so their release (flag 2) does too; a key a menu or
    /// the IME took never reports a release.
    private var kittyPressed: Set<UInt16> = []
    /// The event `interpretKeyEvents` is processing while the IME owns it.
    private var kittyPending: NSEvent?

    private var kittyFlags: UInt8 {
        termHandle?.keyboardFlags ?? 0
    }

    /// True when the active input source is an input method (Korean, Japanese) rather than a
    /// plain layout.
    private var inputMethodActive: Bool {
        guard let id = inputContext?.selectedKeyboardInputSource else { return false }
        return !id.contains(".keylayout.")
    }

    private static func kittyMods(_ flags: NSEvent.ModifierFlags) -> UInt8 {
        var m: UInt8 = 0
        if flags.contains(.shift) { m |= 1 }
        if flags.contains(.option) { m |= 2 }
        if flags.contains(.control) { m |= 4 }
        if flags.contains(.command) { m |= 8 }
        if flags.contains(.capsLock) { m |= 64 }
        return m
    }

    /// The text a key event types on its own: none for a Ctrl chord or an Option-as-Alt chord
    /// (with Option as a compose key, `Option+e` types its accent and stays text).
    private func kittyText(_ event: NSEvent) -> String {
        let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        if mods.contains(.control) || mods.contains(.command) { return "" }
        if mods.contains(.option), optionAsAlt { return "" }
        return event.characters ?? ""
    }

    private func kittyEncode(_ event: NSEvent, action: UInt8) -> CopadTermFFI.Handle.KeyEncoding {
        guard let h = termHandle else { return .text }
        let isKey = event.type == .keyDown || event.type == .keyUp
        return h.encodeKey(
            keyCode: event.keyCode,
            mods: Self.kittyMods(event.modifierFlags),
            action: action,
            unmodified: isKey ? event.characters(byApplyingModifiers: []) ?? "" : "",
            shifted: isKey ? event.characters(byApplyingModifiers: .shift) ?? "" : "",
            text: isKey ? kittyText(event) : ""
        )
    }

    /// Encode and send one event. False = it types text, which the caller delivers.
    private func sendKittyKey(_ event: NSEvent, action: UInt8) -> Bool {
        switch kittyEncode(event, action: action) {
        case let .bytes(bytes):
            if action != 2 {
                scrollToBottomOnInput()
                kittyPressed.insert(event.keyCode)
            }
            sendInput(bytes)
            return true
        case .nothing:
            return true
        case .text:
            return false
        }
    }

    /// keyDown under the kitty keyboard protocol. False means not handled here (protocol off, or
    /// the key types text).
    private func kittyKeyDown(_ event: NSEvent) -> Bool {
        let flags = kittyFlags
        guard flags != 0 else { return false }
        if hasMarkedText() {
            // Mid-composition the IME sees the key first; if it passes it on, doCommand sends it
            // encoded, so Shift+Enter after a Hangul syllable commits the syllable, then newline.
            kittyPending = event
            interpretKeyEvents([event])
            kittyPending = nil
            return true
        }
        // "Report all keys" would turn text keys into escape codes; with an input method
        // active they still compose through it (kitty sends input-method text as text too).
        if flags & 8 != 0, inputMethodActive, !kittyText(event).isEmpty {
            return false
        }
        return sendKittyKey(event, action: event.isARepeat ? 1 : 0)
    }

    override func keyUp(with event: NSEvent) {
        if kittyPressed.remove(event.keyCode) != nil {
            _ = sendKittyKey(event, action: 2)
            return
        }
        super.keyUp(with: event)
    }

    /// Modifier keys report press/release only under "report all keys" (flag 8).
    override func flagsChanged(with event: NSEvent) {
        super.flagsChanged(with: event)
        guard kittyFlags & 8 != 0, !hasMarkedText() else { return }
        // Device-dependent bits (NX_DEVICE*KEYMASK) tell left from right, so releasing one Shift
        // while the other is held is still a release.
        let raw = event.modifierFlags.rawValue
        let down: Bool
        switch event.keyCode {
        case 56: down = raw & 0x02 != 0 // left shift
        case 60: down = raw & 0x04 != 0 // right shift
        case 59: down = raw & 0x01 != 0 // left control
        case 62: down = raw & 0x2000 != 0 // right control
        case 58: down = raw & 0x20 != 0 // left option
        case 61: down = raw & 0x40 != 0 // right option
        case 55: down = raw & 0x08 != 0 // left command
        case 54: down = raw & 0x10 != 0 // right command
        case 57: down = true // caps lock: one event per press
        default: return
        }
        if case let .bytes(bytes) = kittyEncode(event, action: down ? 0 : 2) {
            sendInput(bytes)
        }
    }

    override func resignFirstResponder() -> Bool {
        kittyPressed.removeAll()
        kittyPending = nil
        let resigned = super.resignFirstResponder()
        if resigned {
            reportFocus(false)
        }
        return resigned
    }

    /// Build `ESC + utf8(base_char)` for an Option-modified key. `charactersIgnoringModifiers`
    /// applies Shift but not Option (Option+1 gives "1", Option+Shift+1 gives "!"). Nil for
    /// non-printable keys, which take the IME / doCommand path.
    private func optionAltBytes(for event: NSEvent) -> [UInt8]? {
        guard
            let chars = event.charactersIgnoringModifiers,
            let scalar = chars.unicodeScalars.first
        else { return nil }
        // Skip the ASCII control range and the NSFunctionKey block (0xF700+).
        let v = scalar.value
        if v < 0x20 || v == 0x7F || v >= 0xF700 { return nil }
        var out: [UInt8] = [0x1B]
        out.append(contentsOf: Array(chars.utf8))
        return out
    }

    /// Virtual key codes for the scroll shortcuts. keyCode, not `characters`, so IME-active
    /// keystrokes (Caps-Lock toggle, Eisu) do not shadow them.
    private enum KeyCode {
        static let up: UInt16 = 126
        static let down: UInt16 = 125
        static let left: UInt16 = 123
        static let right: UInt16 = 124
        static let home: UInt16 = 115
        static let end: UInt16 = 119
        static let pageUp: UInt16 = 116
        static let pageDown: UInt16 = 121
        static let delete: UInt16 = 51
        static let forwardDelete: UInt16 = 117
        static let returnKey: UInt16 = 36
        static let escape: UInt16 = 53
    }

    /// Translate a `force_meta_keys` keyCode to its `ESC + <byte>` sequence. The keyDown caller
    /// filters to `forceMetaKeyCodes` first, so add a case here when extending
    /// `parseForceMetaKeyCodes`.
    private func forceMetaBytes(forKeyCode kc: UInt16) -> [UInt8]? {
        switch kc {
        case KeyCode.returnKey: [0x1B, 0x0D] // ESC + CR
        case KeyCode.escape: [0x1B, 0x1B] // ESC + ESC
        default: nil
        }
    }

    /// Map `force_meta_keys` names to macOS key codes, case-insensitively. Unknown entries print
    /// one stderr line and are dropped. A Set keeps the keyDown check O(1).
    private static func parseForceMetaKeyCodes(_ names: [String]) -> Set<UInt16> {
        var codes: Set<UInt16> = []
        for name in names {
            switch name.lowercased() {
            case "return", "enter":
                codes.insert(KeyCode.returnKey)
            case "escape", "esc":
                codes.insert(KeyCode.escape)
            default:
                let msg = "[copad] [terminal] force_meta_keys: unknown key '\(name)' " +
                    "(supported: Return, Escape)\n"
                FileHandle.standardError.write(Data(msg.utf8))
            }
        }
        return codes
    }

    /// Intercept Cmd/Shift scroll keys before the PTY; true when consumed.
    private func handleScrollKey(_ event: NSEvent, mods: NSEvent.ModifierFlags) -> Bool {
        guard let h = termHandle else { return false }
        let kc = event.keyCode
        // As in scrollWheel, skip needsDisplay: the display link redraws with a fresh snapshot
        // within a frame, and marking dirty here caused a stale draw first.
        if mods.contains(.command) {
            switch kc {
            case KeyCode.up: h.scrollLines(1); return true
            case KeyCode.down: h.scrollLines(-1); return true
            case KeyCode.home: h.scrollToTop(); return true
            case KeyCode.end: h.scrollToBottom(); return true
            default: break
            }
        }
        if mods.contains(.shift) {
            switch kc {
            case KeyCode.pageUp: h.scrollPageUp(); return true
            case KeyCode.pageDown: h.scrollPageDown(); return true
            default: break
            }
        }
        return false
    }

    /// Cmd+arrow / Cmd+delete line-edit shortcuts (iTerm2/Terminal.app convention) mapped to
    /// readline bytes. Called from keyDown, since Cmd-modified events go to super for menu
    /// dispatch and never reach interpretKeyEvents.
    private func commandKeyBytes(forKeyCode kc: UInt16) -> [UInt8]? {
        switch kc {
        case KeyCode.left: [0x01] // Cmd+← → Ctrl+A (beginning-of-line)
        case KeyCode.right: [0x05] // Cmd+→ → Ctrl+E (end-of-line)
        case KeyCode.delete: [0x15] // Cmd+⌫ → Ctrl+U (unix-line-discard)
        case KeyCode.forwardDelete: [0x0B] // Cmd+⌦ → Ctrl+K (kill-line)
        default: nil
        }
    }

    /// Map Ctrl+letter / @ [ \ ] ^ _ / Space to control bytes (0x00-0x1f, 0x7f); nil for
    /// combinations outside the ASCII control set so the responder chain handles them.
    private func controlBytes(for event: NSEvent) -> [UInt8]? {
        guard
            let chars = event.charactersIgnoringModifiers?.lowercased(),
            let scalar = chars.unicodeScalars.first
        else { return nil }
        let v = scalar.value
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

    // NSTextInputClient: the IME routes commits through `insertText` and special keys through
    // `doCommand`.

    func insertText(_ string: Any, replacementRange _: NSRange) {
        let text: String
        if let s = string as? String { text = s }
        else if let a = string as? NSAttributedString { text = a.string }
        else { return }
        // Some IMEs skip the unmarkText() that normally precedes a commit; clear here too so the
        // preedit overlay does not linger.
        if markedText != nil {
            markedText = nil
            syncIMEOverlay()
            requestRepaint()
        }
        guard !text.isEmpty else { return }
        scrollToBottomOnInput()
        sendInput(Array(text.utf8))
    }

    override func doCommand(by selector: Selector) {
        // The IME owned this key (composing) and passed it on: send the key in the negotiated
        // encoding after whatever the IME committed, once per event.
        if let pending = kittyPending {
            kittyPending = nil
            if sendKittyKey(pending, action: pending.isARepeat ? 1 : 0) {
                return
            }
        }
        if let bytes = commandBytes(for: selector) {
            scrollToBottomOnInput()
            sendInput(bytes)
        }
        // Unmapped selectors are ignored; super would treat them as text editing on a view with
        // no document model.
    }

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
        case #selector(NSStandardKeyBindingResponding.moveWordLeft(_:)):
            [0x1B, 0x62] // ESC b
        case #selector(NSStandardKeyBindingResponding.moveWordRight(_:)):
            [0x1B, 0x66] // ESC f
        // Option+backspace/delete: ESC+DEL is the meta-backspace bash/zsh bind by default; raw
        // Ctrl+W would ignore the readline word-boundary config.
        case #selector(NSStandardKeyBindingResponding.deleteWordBackward(_:)):
            [0x1B, 0x7F]
        case #selector(NSStandardKeyBindingResponding.deleteWordForward(_:)):
            [0x1B, 0x64] // ESC d
        // Line-edit selectors a custom DefaultKeyBinding.dict or text-input plugin might
        // synthesize; normally `commandKeyBytes()` in keyDown handles them first.
        case #selector(NSStandardKeyBindingResponding.moveToBeginningOfLine(_:)):
            [0x01]
        case #selector(NSStandardKeyBindingResponding.moveToEndOfLine(_:)):
            [0x05]
        case #selector(NSStandardKeyBindingResponding.deleteToBeginningOfLine(_:)):
            [0x15]
        case #selector(NSStandardKeyBindingResponding.deleteToEndOfLine(_:)):
            [0x0B]
        default:
            nil
        }
    }

    // IME preedit: setMarkedText stores the composition, painted as an overlay at the cursor in
    // `draw(_:)`; nothing reaches the PTY until `insertText` commits.

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange _: NSRange) {
        let text: String = if let s = string as? String { s }
        else if let a = string as? NSAttributedString { a.string }
        else { "" }
        if text.isEmpty {
            markedText = nil
        } else {
            markedText = text
            // Clamp the highlighted sub-range: some IMEs and dictation send ranges past the
            // marked string, and an out-of-range index would crash CoreText.
            let utf16Count = (text as NSString).length
            let loc = max(0, min(selectedRange.location, utf16Count))
            let len = max(0, min(selectedRange.length, utf16Count - loc))
            markedSelectedRange = NSRange(location: loc, length: len)
        }
        syncIMEOverlay()
        requestRepaint()
    }

    func unmarkText() {
        guard markedText != nil else { return }
        markedText = nil
        markedSelectedRange = NSRange(location: 0, length: 0)
        syncIMEOverlay()
        requestRepaint()
    }

    /// Push composition state into the GPU-mode overlay (no-op in CPU mode, where `draw(_:)`
    /// paints the preedit).
    private func syncIMEOverlay() {
        guard let overlay = imeOverlay else { return }
        let cursor = snapshotCache?.cursor
        overlay.updateComposition(
            markedText: markedText,
            selectedRange: markedSelectedRange,
            cursorRow: Int(cursor?.row ?? 0),
            cursorCol: Int(cursor?.col ?? 0),
        )
    }

    /// Caret position for IMEs. There is no document buffer, so report a zero-length range at the
    /// start; Korean/Japanese IMEs key off `markedRange` + `firstRect`, and NSNotFound breaks
    /// several IMEs.
    func selectedRange() -> NSRange {
        NSRange(location: 0, length: 0)
    }

    func markedRange() -> NSRange {
        guard let text = markedText else {
            return NSRange(location: NSNotFound, length: 0)
        }
        return NSRange(location: 0, length: (text as NSString).length)
    }

    func hasMarkedText() -> Bool {
        markedText != nil
    }

    /// The terminal buffer is not exposed to the IME; nil degrades gracefully for accessibility
    /// and dictation.
    func attributedSubstring(forProposedRange _: NSRange, actualRange _: NSRangePointer?) -> NSAttributedString? {
        nil
    }

    /// Attribute keys the IME may put in marked text; underline is painted by us and segment
    /// styles are ignored.
    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        [.underlineStyle, .underlineColor]
    }

    /// Where the IME anchors its candidate window: the cursor cell's rect in screen coordinates,
    /// which the IME pipeline expects (else the popup floats at (0, 0)). `convert(_:to:)` already
    /// handles the flipped view, so pass local coords; inverting y again anchors the mirror row.
    func firstRect(forCharacterRange _: NSRange, actualRange _: NSRangePointer?) -> NSRect {
        guard let snap = snapshotCache, cellWidth > 0, cellHeight > 0 else { return .zero }
        let cursor = snap.cursor
        let cellRect = NSRect(
            x: CGFloat(cursor.col) * cellWidth,
            y: CGFloat(cursor.row) * cellHeight,
            width: cellWidth,
            height: cellHeight,
        )
        guard let win = window else { return .zero }
        let windowRect = convert(cellRect, to: nil)
        return win.convertToScreen(windowRect)
    }

    /// Preedit hit-testing is unsupported; NSNotFound keeps the IME from probing further.
    func characterIndex(for _: NSPoint) -> Int {
        NSNotFound
    }
}

/// In-terminal find UI mirroring `copad-linux/src/search.rs`'s SearchBar (text field, prev/next,
/// case toggle, close), at the bottom of the pane. It holds no search logic and reports actions
/// to the controller through `Action`.
private final class FindBar: NSView, NSTextFieldDelegate {
    enum Action {
        case queryChanged
        case next
        case prev
        case caseToggle
        case close
    }

    private let textField: NSTextField
    private let prevButton: NSButton
    private let nextButton: NSButton
    private let caseButton: NSButton
    private let closeButton: NSButton
    private let onAction: (Action) -> Void

    var currentPattern: String {
        textField.stringValue
    }

    /// Authoritative case-sensitive state, tracked apart from `NSButton.state` because the
    /// borderless `.accessoryBar` button gives no visible pressed feedback;
    /// `updateCaseButtonAppearance` colors the title instead.
    private var caseSensitiveState: Bool = false
    var caseSensitive: Bool {
        caseSensitiveState
    }

    init(theme: CopadTheme, onAction: @escaping (Action) -> Void) {
        self.onAction = onAction
        textField = NSTextField(frame: .zero)
        prevButton = NSButton(title: "↑", target: nil, action: nil)
        nextButton = NSButton(title: "↓", target: nil, action: nil)
        caseButton = NSButton(title: "Aa", target: nil, action: nil)
        closeButton = NSButton(title: "✕", target: nil, action: nil)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 6
        applyTheme(theme)

        textField.placeholderString = "Search…"
        textField.bezelStyle = .roundedBezel
        textField.delegate = self
        textField.target = self
        textField.action = #selector(textFieldEnter(_:))
        textField.refusesFirstResponder = false

        for btn in [prevButton, nextButton, caseButton, closeButton] {
            btn.bezelStyle = .accessoryBar
            btn.isBordered = false
            btn.font = NSFont.systemFont(ofSize: 12)
            btn.target = self
        }
        caseButton.toolTip = "Case sensitive (Aa)"
        updateCaseButtonAppearance()
        prevButton.toolTip = "Previous match (Shift+Enter)"
        nextButton.toolTip = "Next match (Enter)"
        closeButton.toolTip = "Close (Esc)"
        prevButton.action = #selector(prevTapped)
        nextButton.action = #selector(nextTapped)
        caseButton.action = #selector(caseTapped)
        closeButton.action = #selector(closeTapped)

        let stack = NSStackView(views: [textField, prevButton, nextButton, caseButton, closeButton])
        stack.orientation = .horizontal
        stack.alignment = .centerY
        stack.spacing = 6
        stack.edgeInsets = NSEdgeInsets(top: 4, left: 8, bottom: 4, right: 8)
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor),
            textField.widthAnchor.constraint(greaterThanOrEqualToConstant: 180),
        ])
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    func applyTheme(_ theme: CopadTheme) {
        // Slightly lighter than the terminal background to separate the bar from the grid.
        layer?.backgroundColor = theme.surface2.nsColor
            .withAlphaComponent(0.92).cgColor
    }

    func focusSearchField() {
        window?.makeFirstResponder(textField)
        // Select all so a fresh Cmd+F over an existing query types over it.
        textField.currentEditor()?.selectAll(nil)
    }

    @objc private func textFieldEnter(_: Any?) {
        onAction(.next)
    }

    @objc private func prevTapped() {
        onAction(.prev)
    }

    @objc private func nextTapped() {
        onAction(.next)
    }

    @objc private func caseTapped() {
        caseSensitiveState.toggle()
        updateCaseButtonAppearance()
        onAction(.caseToggle)
    }

    /// Repaint the "Aa" title in the accent color when active, since a borderless button shows no
    /// pressed indicator of its own.
    private func updateCaseButtonAppearance() {
        let color: NSColor = caseSensitiveState
            ? .controlAccentColor
            : .secondaryLabelColor
        let attrs: [NSAttributedString.Key: Any] = [
            .foregroundColor: color,
            .font: NSFont.boldSystemFont(ofSize: 12),
        ]
        caseButton.attributedTitle = NSAttributedString(string: "Aa", attributes: attrs)
    }

    @objc private func closeTapped() {
        onAction(.close)
    }

    func controlTextDidChange(_: Notification) {
        onAction(.queryChanged)
    }

    /// Esc closes and Shift+Enter goes to the previous match (plain Enter is the text field's
    /// action, for next).
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53 {
            onAction(.close)
            return
        }
        if event.keyCode == 36, event.modifierFlags.contains(.shift) { // Shift+Enter
            onAction(.prev)
            return
        }
        super.keyDown(with: event)
    }

    /// Re-routes Esc, which the field editor would otherwise swallow as "cancel editing".
    func control(_: NSControl, textView _: NSTextView, doCommandBy selector: Selector) -> Bool {
        if selector == #selector(NSStandardKeyBindingResponding.cancelOperation(_:)) {
            onAction(.close)
            return true
        }
        if selector == #selector(NSStandardKeyBindingResponding.insertBacktab(_:)) {
            // Shift+Tab also goes to the previous match.
            onAction(.prev)
            return true
        }
        return false
    }
}
