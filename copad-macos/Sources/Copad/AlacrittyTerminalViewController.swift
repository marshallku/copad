import AppKit
import CCopadTerm
import Foundation

/// Phase 3.2 — `copad-term` (alacritty_terminal-backed) pane with
/// CoreText cell rendering. Conforms to `CopadPanel` so PaneManager
/// / SplitNode / socket commands treat it identically to
/// `TerminalViewController`. See
/// docs/macos-renderer-migration-plan.md for the staged scope.
///
/// What ships in this slice:
///
/// - PTY spawn (`CopadTermFFI.Handle`) — already lands in 3.1
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
final class AlacrittyTerminalViewController: NSViewController, CopadPanel, Zoomable, TerminalCapable {
    let panelID: String = UUID().uuidString
    private(set) var currentTitle: String = "Terminal (alacritty)"
    /// `tab.rename`-set title override. When non-nil, OSC 0/2 updates
    /// from the running program (shell prompt setting window title)
    /// are ignored — the user's chosen name wins. Mirrors SwiftTerm
    /// path's customTitle semantics so the socket contract is the
    /// same across (historical) backends.
    private(set) var customTitle: String?

    /// Set by `PaneManager.assignEventBus` after the EventBus is created.
    /// Used to publish `terminal.output` on keyboard / paste input —
    /// mirror of `copad-linux/src/tabs.rs` `connect_commit` hook.
    /// `nil` ⇒ events are silently dropped (test panels, panels created
    /// before the bus is attached). The setter forwards to the render
    /// view (which is the actual input first-responder, where the
    /// keyboard / paste call sites live).
    weak var eventBus: EventBus? {
        didSet { renderView?.eventBus = eventBus }
    }

    /// v1 cwd snapshot for session persistence. Only reflects the
    /// startup cwd passed at construction — the alacritty backend
    /// doesn't surface OSC 7 / cwd_changed yet (alacritty_terminal as
    /// a library has no cwd state, no `CurrentDirectoryUrl` event in
    /// Most recent cwd reported by the in-shell `copad-cwd` hook
    /// (`coctl call panel.report_cwd`). Updated synchronously via
    /// `setReportedCwd(_:)` on every `chpwd` notification. Preferred
    /// over the `proc_pidinfo` fallback because shell hooks see the
    /// authoritative cwd without needing macOS entitlements.
    private(set) var reportedCwd: String?

    /// Tracked cwd. Priority: shell-reported (via `panel.report_cwd`)
    /// → `proc_pidinfo` syscall (EPERM on un-entitled macOS dev
    /// builds) → spawn-time `initialCwd`. Mirrors the 3-layer
    /// fallback Linux gets via VTE + `/proc/<pid>/cwd` + last_cwd.
    var currentCwd: String? {
        reportedCwd ?? termHandle?.childCwd() ?? initialCwd
    }

    /// Called from the `panel.report_cwd` registry handler when the
    /// in-shell hook reports a new cwd. Idempotent — same value just
    /// overwrites.
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

    /// Active font size for Cmd+= / Cmd+- / Cmd+0 zoom. Mirror of
    /// `TerminalViewController.currentFontSize`. Decoupled from
    /// `configFontSize` so a live zoom-in survives a config hot-
    /// reload (matches SwiftTerm path: applyFont updates the
    /// baseline but the user's current zoom level stays).
    private var currentFontSize: CGFloat
    private var configFontSize: CGFloat
    private var currentFontFamily: String

    /// Focus target for `panel.focusTarget` — callers like PaneManager
    /// that activate a pane (`makeFirstResponder`) need the renderView,
    /// not the layout container.
    var focusTarget: NSView {
        renderView ?? view
    }

    init(config: CopadConfig, theme: CopadTheme, cwd: String? = nil, initialInput: String? = nil) {
        self.config = config
        self.theme = theme
        initialCwd = cwd
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

    /// View hierarchy:
    ///   container (plain NSView)
    ///   └─ renderView (AlacrittyRenderView, transparent layer when image active)
    ///
    /// Background image + tint moved to `TabViewController.contentArea`
    /// in Phase 10b follow-up so splits share one image. Pane's
    /// `setImageBackgroundActive` flips to true so default-bg cells
    /// stay transparent and the window-level image shows through.
    ///
    /// Focus contract: external callers that target `panel.view` (the
    /// container) get a silent no-op because the container's default
    /// `acceptsFirstResponder` is false. The render view becomes
    /// first responder via `startIfNeeded`'s explicit
    /// `makeFirstResponder(render)` call, via user mouse clicks (the
    /// `mouseDown` override re-asserts focus), and via the
    /// activate-on-tab-switch path going through PaneManager. This
    /// mirrors what SwiftTerm's `TerminalViewController` does — the
    /// container is just a layout host, not a focus participant.
    override func loadView() {
        let frame = NSRect(x: 0, y: 0, width: 1200, height: 800)
        let container = NSView(frame: frame)
        container.wantsLayer = true

        let render = AlacrittyRenderView(
            theme: theme,
            font: resolveFont(family: config.fontFamily, size: CGFloat(config.fontSize)),
            transparentDefaultBg: config.transparentDefaultBg,
            windowOpacity: config.windowOpacity,
            osc52Policy: config.osc52,
            optionAsAlt: config.optionAsAlt,
            forceMetaKeys: config.forceMetaKeys,
        )
        render.frame = container.bounds
        render.autoresizingMask = [.width, .height]
        container.addSubview(render)
        // Bind the panel identity before the render view starts handling
        // input — `terminal.output` events emitted via `sendInput` need
        // to carry the right panel_id from the very first keystroke.
        // `eventBus` is propagated later (PaneManager.assignEventBus
        // runs after all panels are constructed); the didSet on the VC
        // forwards subsequent assignments.
        render.panelID = panelID
        render.eventBus = eventBus
        renderView = render

        // Find bar — hidden by default. Anchored at the bottom of the
        // pane with manual frame math (auto-resize on container
        // resize). Toggled by Cmd+F via AppDelegate; the bar handles
        // its own next/prev/close shortcuts when first responder.
        let bar = FindBar(theme: theme) { [weak self] action in
            self?.handleFindAction(action)
        }
        bar.isHidden = true
        // Use Auto Layout for the bar's outer placement so the
        // bottom-anchor is unambiguous regardless of the parent's
        // `isFlipped` setting. Manual frame + autoresizing was
        // unreliable: depending on the autoresizing/flip
        // combination, the bar could end up at the wrong edge.
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

        // Apply background from config if set. Runs before viewDidAppear,
        // which is fine: NSImageView accepts an image even off-screen, and
        // we re-snap layer state in applyBackground itself.
        if let path = config.backgroundPath {
            applyBackground(path: path, tint: config.backgroundTint, opacity: config.backgroundOpacity)
        }
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
        // SwiftTerm path uses the legacy `/tmp/copad-<pid>.sock`
        // pattern for its per-instance GUI socket — match it here so
        // `coctl call` from the in-shell hook hits this exact GUI
        // (not the well-known daemon at `~/Library/Caches/copad/socket`).
        let socketPath = "/tmp/copad-\(ProcessInfo.processInfo.processIdentifier).sock"
        termHandle = CopadTermFFI.Handle(
            cols: cols,
            rows: rows,
            shell: initialCwd != nil ? config.shell : nil,
            cwd: initialCwd,
            panelID: panelID,
            socketPath: socketPath,
        )
        // Push the active palette so the first OSC 4 / 10 / 11 / 12
        // query (often part of nvim / fish prompt init) gets the color
        // we actually render. applyTheme reapplies on every theme
        // hot-reload — same code path keeps the two in sync.
        termHandle?.applyPaletteFromTheme(theme)
        if let initialInput {
            termHandle?.input(Array(initialInput.utf8))
        }
        renderView?.bind(handle: termHandle)
        // Target the render view explicitly. The container forwards too
        // (belt and braces for callers that already have a reference to
        // `panel.view`), but going direct skips the second hop.
        if let render = renderView {
            view.window?.makeFirstResponder(render)
        }
    }

    /// Config hot-reload: flip the OSC 52 policy on the live render
    /// view so already-open alacritty panes start honoring the new
    /// `[security] osc52` setting without needing to be recreated.
    func applyOSC52Policy(_ policy: OSC52Policy) {
        renderView?.setOSC52Policy(policy)
    }

    /// Config hot-reload: `[window] opacity`. Refreshes the layer bg
    /// and the per-cell skip gate. The window-level isOpaque /
    /// NSVisualEffectView swap is handled by `AppDelegate.
    /// applyWindowTransparency` — this is the cell-renderer half.
    /// Image + tint alpha scaling now happens at the window level
    /// (TabViewController owns the bg/tint views).
    func applyWindowOpacity(_ opacity: Double) {
        windowOpacity = opacity
        renderView?.setWindowOpacity(opacity)
    }

    /// Config hot-reload: swap the theme on a running pane. Mirrors
    /// `TerminalViewController.applyTheme` so `PaneManager.applyConfig`
    /// can fan out theme changes uniformly across both backends.
    func applyTheme(_ newTheme: CopadTheme) {
        theme = newTheme
        renderView?.setTheme(newTheme)
        termHandle?.applyPaletteFromTheme(newTheme)
        findBar?.applyTheme(newTheme)
    }

    // MARK: - Find

    /// Show / hide the find bar. Cmd+F handler routes here via
    /// `AppDelegate.performFindPanelAction`. Toggle behavior matches
    /// the Linux SearchBar: first invocation shows + focuses, second
    /// invocation hides + clears search state.
    func toggleFindBar() {
        guard let bar = findBar else { return }
        if bar.isHidden {
            bar.isHidden = false
            bar.focusSearchField()
            // Re-apply the existing query if user re-opens the bar
            // (keeps the highlight on what they were last finding).
            performFindIfPattern(forward: true)
        } else {
            closeFindBar()
        }
    }

    /// Esc / close-button path. Hides the bar, clears the Rust search
    /// state, and returns focus to the render view so the user can
    /// keep typing into the shell.
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

    /// Cmd+G / Cmd+Shift+G — only acts when the bar is visible.
    /// Otherwise the menu's keyEquivalent silently no-ops, which
    /// matches iTerm2 / Terminal.app behavior.
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
        // searchNext mutates Rust-side state only — no grid damage —
        // so the per-tick damage gate won't refresh the snapshot for
        // us. Force a refresh so the next `draw(_:)` sees the new
        // `search_match` from the snapshot.
        renderView?.refreshSnapshotForSearchChange(h)
    }

    private func performFindIfPattern(forward: Bool) {
        guard let bar = findBar else { return }
        let pattern = bar.currentPattern
        guard let h = termHandle else { return }
        if pattern.isEmpty {
            // User deleted the query — drop the cached match so the
            // highlight vanishes on the next frame instead of pinning
            // on the last hit.
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

    /// Config hot-reload: swap the font family/size on a running pane.
    /// Updates the baseline `configFontSize` for `zoomReset`, but
    /// leaves `currentFontSize` alone if the user has an active zoom
    /// in flight — matches `TerminalViewController.applyFont` so a
    /// live Cmd++ doesn't get clobbered by saving config.toml.
    func applyFont(family: String, baseSize: CGFloat) {
        configFontSize = baseSize
        currentFontFamily = family
        // Re-apply at the *current* (possibly zoomed) size so the
        // user's zoom level survives the config reload.
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

    /// `tab.rename` entry. Saves the user-chosen title, marks it as
    /// the current title, and notifies the tab bar to repaint.
    /// Subsequent OSC 0/2 from the running program are ignored as
    /// long as `customTitle` is set.
    func setCustomTitle(_ title: String) {
        customTitle = title
        currentTitle = title
        NotificationCenter.default.post(name: .terminalTitleChanged, object: self)
    }

    /// Raw text → PTY. UTF-8 encoded; bytes are kernel-buffered so
    /// even pre-`startIfNeeded` feeds reach the child once it's up.
    func feedText(_ text: String) {
        termHandle?.input(Array(text.utf8))
    }

    /// Convenience: feed + trailing newline. SwiftTerm uses `"\n"` here
    /// (LF only); we match so scripts that rely on the bare LF (no CR)
    /// behave identically across backends.
    func execCommand(_ command: String) {
        feedText(command + "\n")
    }

    /// `terminal.state` — grid metrics + cursor + window title.
    /// Reads via snapshot (cheap; alacritty snapshot is a Rust-side
    /// copy of the current grid). Cursor is reported as
    /// `[row, col]` to match SwiftTerm's shape.
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

    /// `terminal.read` — visible viewport rendered as plain text.
    /// One line per grid row, joined by '\n'. Decoder copies bytes
    /// out of the borrowed `rowUtf8` buffer before the snapshot is
    /// dropped, so the returned string outlives the FFI lifetime.
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

    /// `terminal.history` — last `lines` rows of scrollback above the
    /// viewport top. Routes through `copad_term_history` so output
    /// matches SwiftTerm's `Terminal.getLine(row: -N..0)` walk. NUL
    /// cells render as space; `\n` between rows; no trailing newline.
    ///
    /// `rows` / `cols` reflect viewport metrics (not the history
    /// dimensions) so the shape matches SwiftTerm and callers can
    /// pair the text with a follow-up `terminal.state`.
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

    /// Zoom in / out / reset. Same step + clamp values as the
    /// SwiftTerm path so users get identical behavior across the two
    /// backends. `currentFontSize` is the source of truth; on every
    /// step we re-call `applyFontInternal` which recomputes cell
    /// metrics and resizes the PTY grid (smaller font → more cells fit
    /// → SIGWINCH so shells can re-wrap).
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

    /// Monotonic token bumped on every `applyBackground` / `clearBackground`
    /// so an async image decode finishing late (slow Gatekeeper scan,
    /// Mirror of the render view's `windowOpacity` value. Initialized
    /// from `config.windowOpacity` at construction; updated by
    /// `applyWindowOpacity` hot-reload. The render view keeps its own
    /// copy because the draw loop reads it on every frame.
    private var windowOpacity: Double = 1.0

    /// Wire an image background + tint overlay. The render view's layer
    /// goes transparent so the image layer underneath composites
    /// through. `transparent_default_bg` config decides whether default
    /// cells fill opaquely on top (image hidden behind text area, cursor
    /// always visible) or stay transparent (image visible through blank
    /// cells, cursor visibility depends on accent vs image contrast).
    ///
    /// Wire an image background + tint overlay. `NSImage(contentsOfFile:)`
    /// can stall the main thread for tens to hundreds of ms during the
    /// first Gatekeeper / XProtect scan of a newly-seen wallpaper file;
    /// Per-pane bg/tint ownership moved to TabViewController.contentArea
    /// (one image spans the whole window — splits no longer duplicate
    /// the wallpaper). This method now only updates renderer state so
    /// the alacritty draw path knows to skip the opaque default-bg
    /// fill (`isTransparentBgActive` gate). `path` / `tint` / `opacity`
    /// are consumed at the window level; AlacrittyTerminalViewController
    /// doesn't need them. Kept in the signature for `CopadPanel`
    /// protocol conformance.
    func applyBackground(path _: String, tint _: Double, opacity _: Double) {
        renderView?.setImageBackgroundActive(true)
        renderView?.needsDisplay = true
    }

    func clearBackground() {
        renderView?.setImageBackgroundActive(false)
        renderView?.needsDisplay = true
    }

    func setTint(_: Double) {
        // No-op on the panel — tint is rendered at the window level
        // (TabViewController owns the tint overlay). Kept for
        // CopadPanel protocol conformance.
    }

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
/// are taken under the `copad-term` handle's `FairMutex`; the lock is
/// dropped before `setNeedsDisplay` so AppKit's redraw doesn't block
/// the PTY reader thread.
///
/// Coordinate system is **flipped** (origin top-left, y down) so row 0
/// renders at the top of the view — matching the terminal convention
/// and keeping cell math straightforward.
@MainActor
private final class AlacrittyRenderView: NSView, @preconcurrency NSTextInputClient {
    /// `var` so `setTheme(_:)` can hot-swap on config reload. Draw
    /// paths read it directly each frame, so a swap takes effect on
    /// the next paint without further plumbing.
    private var theme: CopadTheme
    private var font: NSFont
    private var boldFont: NSFont
    private var italicFont: NSFont
    private var boldItalicFont: NSFont
    private(set) var cellWidth: CGFloat = 0
    private(set) var cellHeight: CGFloat = 0
    private var ascent: CGFloat = 0

    /// Cached CGColor for the 16-color ANSI palette + xterm 256
    /// extension. Indices 0-15 from `theme.palette` (so theme changes
    /// reflect the right color); 16-231 from the 6×6×6 cube; 232-255
    /// from the grayscale ramp. `var` so `setTheme(_:)` can rebuild it
    /// on hot-reload — the 256-entry rebuild is cheap.
    private var paletteCache: [CGColor]

    private weak var termHandle: CopadTermFFI.Handle?
    /// CADisplayLink fires once per display refresh (typically 60 Hz,
    /// up to ProMotion's 120 Hz). Replaces the Timer-driven 30 Hz
    /// poll: aligned to vsync (no tearing, no half-frame draws), and
    /// the per-tick `takeDamage` gate means an idle terminal does
    /// zero work between key presses or PTY output bursts.
    ///
    /// `nonisolated(unsafe)` so deinit (Swift 6 nonisolated) can
    /// invalidate without crossing the main-actor barrier — same
    /// pattern as the previous `refreshTimer`.
    private nonisolated(unsafe) var vsyncLink: CADisplayLink?

    /// Cached snapshot for the most recent paint. Refreshed only when
    /// `copad_term_take_damage` reports the grid changed.
    private var snapshotCache: CopadTermFFI.Snapshot?

    /// Panel identity stamped into every `terminal.output` event so
    /// consumers (AI agents, trigger conditions) can filter per pane.
    /// Set by the controlling VC right after this view is constructed;
    /// stays empty for test-only `AlacrittyRenderView` instances.
    var panelID: String = ""

    /// Bus reference for publishing `terminal.output` on keyboard /
    /// paste input. Mirror of the VC-level property; the VC's setter
    /// forwards subsequent assignments so PaneManager's late wiring
    /// reaches the render view too.
    weak var eventBus: EventBus?

    /// User opt-in: when true AND an image background is active, default
    /// (sentinel-zero) cells render without a bg fill so the image shows
    /// through. Independent of the controller's bg state because the
    /// flag is set at init from the live config; the
    /// `imageBackgroundActive` runtime flag (set/cleared as the user
    /// applies or clears the background) AND-gates the actual behavior
    /// — no image, no transparency, regardless of the user pref.
    private let transparentDefaultBg: Bool
    private var imageBackgroundActive = false

    /// `[window] opacity` — when < 1.0, the window is non-opaque and
    /// the renderer skips default-bg cells (same path as
    /// `transparentDefaultBg && imageBackgroundActive`) so the
    /// alpha-tinted layer bg / blurred desktop bleeds through. ANSI bg
    /// + reverse-video cells still paint opaque (Ghostty / Zed pattern).
    /// `var` for hot-reload via `setWindowOpacity`.
    private var windowOpacity: Double

    /// Single gate for "skip default-bg cell fills". Either
    /// `[window] opacity < 1.0` (Ghostty) or
    /// `transparentDefaultBg && imageBackgroundActive` (wallpaper
    /// passthrough). Used by the bounds fill, per-cell skip, and the
    /// cursor-outline heuristic — read it everywhere instead of
    /// duplicating the OR.
    private var isTransparentBgActive: Bool {
        windowOpacity < 1.0 || (transparentDefaultBg && imageBackgroundActive)
    }

    /// OSC 52 policy from config. `.deny` (default) drops the request
    /// with a stderr warning; `.allow` writes to NSPasteboard.general.
    /// `var` so config hot-reload can flip it without re-creating the
    /// pane — matches `TerminalViewController.applyOSC52Policy`.
    private var osc52Policy: OSC52Policy

    /// Setter for the controller to forward `applyConfig` updates.
    func setOSC52Policy(_ policy: OSC52Policy) {
        osc52Policy = policy
    }

    /// When true, Option+key bypasses the IME path and writes
    /// `ESC + base_char` to the PTY so tmux/zsh/readline Meta bindings
    /// fire. Off → the system delivers `¡™£¢`-style chars to `insertText`
    /// and Alt-bindings never see the keystroke.
    private let optionAsAlt: Bool

    /// Set of macOS virtual keyCodes that should send `ESC + <byte>` on
    /// Option, even though they're control characters that
    /// `optionAsAlt`'s printable-only filter normally drops. Built from
    /// `config.forceMetaKeys`. Fires independently of `optionAsAlt` so
    /// users who keep Option=diacritics globally can still get
    /// newline-in-prompt for Claude Code / Python REPL / ipython.
    private let forceMetaKeyCodes: Set<UInt16>

    /// Cursor-blink state. Honored only when the TUI/shell actually
    /// asks for it via DECSCUSR (`cursor.blink == 1` on the snapshot).
    /// When idle with blink on, the display-link callback forces a
    /// redraw every `blinkInterval` even when `takeDamage` says
    /// nothing changed — that's 2 redraws/sec, acceptable cost.
    private var blinkVisible = true
    private var lastBlinkToggle = Date.distantPast
    private let blinkInterval: TimeInterval = 0.5

    /// Trackpad pixel deltas accumulate here between `scrollWheel`
    /// events so a slow swipe (each tick fractional sub-cell) still
    /// eventually produces a whole-cell scroll. Mouse-wheel devices
    /// (`hasPreciseScrollingDeltas == false`) bypass this accumulator
    /// — their per-notch delta is already line-count-shaped.
    private var accumulatedScrollDelta: CGFloat = 0

    /// IME composition state. While the user is composing (Korean
    /// 2-Set, Japanese kana → kanji, Pinyin, …) the system delivers
    /// `setMarkedText` with the in-progress string; nothing flows to
    /// the PTY until the IME commits via `insertText`. We paint the
    /// marked text as an overlay at the cursor cell so the user can
    /// see what they're composing without it ever touching the
    /// terminal buffer.
    ///
    /// `markedSelectedRange` is the IME-highlighted sub-range inside
    /// the marked text (e.g. the active syllable on a multi-syllable
    /// composition). Drawn with a stronger underline.
    private var markedText: String?
    private var markedSelectedRange: NSRange = .init(location: 0, length: 0)

    /// Codepoint → fallback NSFont cache. Populated lazily by
    /// `resolveRunFont` whenever a run contains a non-ASCII scalar; the
    /// all-ASCII fast path skips the lookup entirely so the hot
    /// rendering loop stays unchanged for the common case. Keyed by
    /// the run's first non-ASCII scalar — runs come from
    /// `copad-term::walk_row` which already groups by attribute, and
    /// a run with both Hangul and Powerline glyphs is unreachable in
    /// practice. Invalidated on `setFont` since a new base font means
    /// a new cascade list.
    private var fallbackCache: [UInt32: NSFont] = [:]

    /// (text, font, fg, decoration) → retained CTLine cache. Reuses
    /// the CoreText shaping work across frames so the hot redraw loop
    /// stops paying ~19k `NSAttributedString` + `CTLineCreate` allocs
    /// per second (80×24 grid × ~4 runs × 60 Hz). Per-render-view —
    /// each pane has its own font and zoom level, so a global cache
    /// would key off NSFont identity but evict cross-pane more
    /// aggressively than helps.
    ///
    /// Key includes the full text as a `String`: Swift Dictionary
    /// Equatable handles collisions natively, so we get the
    /// codex-flagged "verify on hit" semantics without a UInt64 hash
    /// + manual compare step.
    ///
    /// LRU via tick counter + scan-on-evict: the eviction scan is
    /// O(n) but only fires when the cache is full. Steady-state
    /// terminal output settles into a working set well under the cap
    /// so eviction is rare; the hot lookup path stays O(1).
    ///
    /// Flushed on `setFont` (cascade changes) and `setTheme` (every
    /// `fgRGBA` is potentially stale). `setImageBackgroundActive` /
    /// `setWindowOpacity` deliberately don't flush: only the bg fill
    /// path reads those, the cached CTLine paints text only.
    private struct CTLineCacheKey: Hashable {
        let text: String
        let fontId: ObjectIdentifier
        let fgRGBA: UInt32
        /// bit 0-7: underline_style (raw u8 from `CopadRun`); bits
        /// 8-10: bold, italic, strike. dim/inverse are NOT bits —
        /// their visual effect is already baked into `fgRGBA` by the
        /// caller before lookup.
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

    init(
        theme: CopadTheme,
        font: NSFont,
        transparentDefaultBg: Bool,
        windowOpacity: Double,
        osc52Policy: OSC52Policy,
        optionAsAlt: Bool,
        forceMetaKeys: [String],
    ) {
        self.theme = theme
        self.font = font
        boldFont = Self.deriveTrait(font, mask: .boldFontMask)
        italicFont = Self.deriveTrait(font, mask: .italicFontMask)
        boldItalicFont = Self.deriveTrait(font, mask: [.boldFontMask, .italicFontMask])
        paletteCache = Self.buildPalette(theme: theme)
        self.transparentDefaultBg = transparentDefaultBg
        self.windowOpacity = windowOpacity
        self.osc52Policy = osc52Policy
        self.optionAsAlt = optionAsAlt
        forceMetaKeyCodes = Self.parseForceMetaKeyCodes(forceMetaKeys)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = Self.layerBg(theme: theme, opacity: windowOpacity, imageActive: false)
        recomputeCellMetrics()
        // Accept the three drop shapes a terminal cares about:
        //   - .fileURL — Finder, any app exporting a file URL. Single
        //     or multi-select both arrive as one drop with N URLs.
        //   - .png/.tiff — raw image bytes (browser drag, screenshot
        //     buffer, NSImage drag). Materialized to a temp file so the
        //     paste delivers a path the running CLI can read.
        //   - .URL — non-file URLs (web links). Pasted as text.
        registerForDraggedTypes([.fileURL, .png, .tiff, .URL])
        // CADisplayLink can't be created until the view has a window
        // (the link binds to the display showing the view). Hooked up
        // in `viewDidMoveToWindow`.
    }

    /// Hot-reload: swap the theme and rebuild the palette cache.
    /// Draw paths read `theme` and `paletteCache` directly each frame,
    /// so the next paint picks up the new colors. Also touches the
    /// layer bg so a theme change while no image is active flips the
    /// underlying clear-vs-themed layer (matches what
    /// `setImageBackgroundActive` does for the image-on path).
    func setTheme(_ newTheme: CopadTheme) {
        theme = newTheme
        paletteCache = Self.buildPalette(theme: newTheme)
        flushCTLineCache()
        if !imageBackgroundActive {
            layer?.backgroundColor = Self.layerBg(
                theme: newTheme,
                opacity: windowOpacity,
                imageActive: false,
            )
        }
        needsDisplay = true
    }

    /// Hot-reload entry for `[window] opacity`. Updates the cached
    /// alpha and refreshes the layer bg (when no image is active —
    /// the image path uses `.clear` and is driven by
    /// `setImageBackgroundActive`).
    func setWindowOpacity(_ newOpacity: Double) {
        windowOpacity = newOpacity
        if !imageBackgroundActive {
            layer?.backgroundColor = Self.layerBg(
                theme: theme,
                opacity: newOpacity,
                imageActive: false,
            )
        }
        needsDisplay = true
    }

    /// Resolve the layer background CGColor for the current state.
    /// Three cases:
    /// - image active → `.clear` (image view + tint paint the bg)
    /// - opacity = 1.0 → theme.background opaque
    /// - opacity < 1.0 → theme.background with alpha = opacity, so the
    ///   non-opaque window's blurred / desktop layer behind shows through
    private static func layerBg(theme: CopadTheme, opacity: Double, imageActive: Bool) -> CGColor {
        if imageActive { return NSColor.clear.cgColor }
        let nsColor = theme.background.nsColor
        return opacity < 1.0
            ? nsColor.withAlphaComponent(CGFloat(opacity)).cgColor
            : nsColor.cgColor
    }

    /// Hot-reload: swap the font (regular face) and rebuild the bold /
    /// italic / bold-italic derivatives and cell metrics. Returns true
    /// when the cell size actually changed so the caller can resize
    /// the term grid to match — without that, the PTY keeps sending
    /// content sized to the old grid.
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
        needsDisplay = true
        return cellWidth != oldW || cellHeight != oldH
    }

    /// Called by the controller when `applyBackground` / `clearBackground`
    /// flips the layered-view state. Toggles the layer-bg clear/opaque
    /// AND the bounds-fill skip — both are needed: layer-bg covers the
    /// image even without per-cell draw, and the bounds fill would
    /// re-cover it inside `draw(_:)`.
    func setImageBackgroundActive(_ active: Bool) {
        imageBackgroundActive = active
        layer?.backgroundColor = Self.layerBg(
            theme: theme,
            opacity: windowOpacity,
            imageActive: active,
        )
        needsDisplay = true
    }

    /// Refresh `snapshotCache` immediately, bypassing the per-tick
    /// damage gate. Required after a find operation: the search
    /// match lives inside the snapshot, and `searchNext` doesn't
    /// generate any grid damage (it mutates Rust-side search state
    /// only), so without this the next `draw(_:)` would paint the
    /// stale highlight from the previous frame's snapshot.
    func refreshSnapshotForSearchChange(_ handle: CopadTermFFI.Handle) {
        snapshotCache = handle.snapshot()
        needsDisplay = true
    }

    /// Apply font traits via NSFontManager, falling back to the regular
    /// face if the family doesn't ship the requested variant (common
    /// for monospace fonts that lack an italic — synthesized italics
    /// are visually awkward, so we just don't slant).
    private static func deriveTrait(_ regular: NSFont, mask: NSFontTraitMask) -> NSFont {
        let mgr = NSFontManager.shared
        if let variant = mgr.convert(regular, toHaveTrait: mask) as NSFont? {
            return variant
        }
        return regular
    }

    /// Resolve the actual font for a single run, with system cascade
    /// fallback for codepoints the base font doesn't cover. Without this
    /// `CTLineCreateWithAttributedString` would draw `.notdef` glyphs
    /// (boxes / `_`) for anything outside the user's monospace face —
    /// CJK, Nerd Font Powerline glyphs (PUA U+E000-F8FF), emoji, …
    /// — because `.font` attribute pins the run to one font with no
    /// implicit cascade. SwiftTerm got this for free; the alacritty
    /// custom renderer has to do it explicitly.
    ///
    /// ASCII fast path: skip when *every* scalar in the run is ASCII
    /// (covered by any monospace base). We can't bail on just the first
    /// scalar because a run can mix ASCII with non-ASCII combining
    /// marks (e.g. `a` + U+0301) — `walk_row` groups by attribute, not
    /// by script. Cache key is the first non-ASCII scalar in the run:
    /// it's the one that drives cascade selection in
    /// `CTFontCreateForString`, and runs in practice are script-
    /// homogenous beyond the ASCII portion.
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

    /// Pack a `CGColor` into RGBA8888 for use as a cache key. Returns
    /// 0 on color spaces we don't expect to see in the draw path
    /// (gray scale CGColors should be rare — palette + theme entries
    /// come through `nsColor.cgColor` which is RGB). A 0 sentinel is
    /// safe: it bins all unexpected colors into the same key, which
    /// at worst causes cache misses, never wrong-glyph hits (Swift
    /// Dict's String equality on the `text` field rules that out).
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

    /// LRU lookup / build. On hit, bumps the entry's tick and returns
    /// the retained `CTLine`. On miss, builds via
    /// `CTLineCreateWithAttributedString`, stores, evicts the
    /// lowest-tick entry if over `ctLineCacheMax`.
    ///
    /// `underlineColor` / `strikeColor` may be 0 — caller passes 0
    /// when the attr isn't applied to this run. They're key
    /// components either way so toggling underline on/off doesn't
    /// reuse the wrong line.
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

        // Miss — build the attributed string. The bits in
        // `styleBits` decide which decoration attrs participate;
        // `font` already has the right bold/italic face baked in
        // (caller resolved via `boldFont`/`italicFont`/etc. before
        // calling).
        var attrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor(red: CGFloat((fgRGBA >> 24) & 0xFF) / 255.0,
                                      green: CGFloat((fgRGBA >> 16) & 0xFF) / 255.0,
                                      blue: CGFloat((fgRGBA >> 8) & 0xFF) / 255.0,
                                      alpha: CGFloat(fgRGBA & 0xFF) / 255.0),
        ]
        let rawUnderline = UInt8(styleBits & 0xFF)
        if rawUnderline != 0 {
            attrs[.underlineStyle] = NSUnderlineStyle.single.rawValue
            attrs[.underlineColor] = NSColor(
                red: CGFloat((underlineRGBA >> 24) & 0xFF) / 255.0,
                green: CGFloat((underlineRGBA >> 16) & 0xFF) / 255.0,
                blue: CGFloat((underlineRGBA >> 8) & 0xFF) / 255.0,
                alpha: CGFloat(underlineRGBA & 0xFF) / 255.0,
            )
        }
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

        // Evict the lowest-tick entry when full. Scan is O(n) but
        // only runs when the cache is at capacity — at steady state
        // the working set stabilizes well below the cap so this is
        // effectively never hit.
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

    /// 256-color ANSI table, computed once at view init. Indices 0-15
    /// follow theme.palette so a theme change re-derives the right
    /// brand colors; 16-231 are the canonical xterm 6×6×6 cube;
    /// 232-255 are the 24-step grayscale ramp.
    private static func buildPalette(theme: CopadTheme) -> [CGColor] {
        var out: [CGColor] = []
        out.reserveCapacity(256)
        for c in theme.palette {
            out.append(c.nsColor.cgColor)
        }
        // Defensive padding if a theme ships fewer than 16 palette
        // entries — black for the missing slots so a stray index
        // doesn't crash.
        while out.count < 16 {
            out.append(CGColor(red: 0, green: 0, blue: 0, alpha: 1))
        }
        // 6×6×6 RGB cube (216 colors).
        let cubeLevels: [CGFloat] = [0, 95, 135, 175, 215, 255].map { $0 / 255.0 }
        for r in 0 ..< 6 {
            for g in 0 ..< 6 {
                for b in 0 ..< 6 {
                    out.append(CGColor(red: cubeLevels[r], green: cubeLevels[g], blue: cubeLevels[b], alpha: 1))
                }
            }
        }
        // 24-step grayscale.
        for i in 0 ..< 24 {
            let v = CGFloat(8 + i * 10) / 255.0
            out.append(CGColor(red: v, green: v, blue: v, alpha: 1))
        }
        return out
    }

    /// Decode the fg/bg encoding from `copad-term::color_to_rgba`.
    /// High byte is a tag: 0x00=default, 0x01=indexed (low byte holds
    /// the index), 0xFF=direct RGB in the low 24 bits. Tagged because
    /// the old "alpha byte = 0 means indexed" trick collided with RGB
    /// colors that have R=0 (skyblue, pure green) — those silently
    /// fell into the indexed path and rendered as grayscale.
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

    /// Bind the display link once the view has a window. AppKit calls
    /// this with `nil` window when the view is removed, so we tear
    /// down too — no leaked link firing into a detached view. We also
    /// observe key-window transitions so the cursor block can flip
    /// between filled (focused) and hollow (blurred) without waiting
    /// for unrelated terminal damage.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        let center = NotificationCenter.default
        guard let win = window else {
            vsyncLink?.invalidate()
            vsyncLink = nil
            center.removeObserver(self)
            return
        }
        if vsyncLink == nil {
            // `displayLink(target:selector:)` is NSView's vsync-link
            // factory (macOS 14+). Property name above is `vsyncLink`
            // to avoid shadowing the method with our stored link.
            let link = displayLink(target: self, selector: #selector(displayLinkFired(_:)))
            link.add(to: .current, forMode: .common)
            vsyncLink = link
        }
        center.removeObserver(self)
        center.addObserver(self, selector: #selector(windowFocusChanged(_:)),
                           name: NSWindow.didBecomeKeyNotification, object: win)
        center.addObserver(self, selector: #selector(windowFocusChanged(_:)),
                           name: NSWindow.didResignKeyNotification, object: win)
    }

    @objc private func windowFocusChanged(_: Notification) {
        // Cursor draw depends on `window?.isKeyWindow`; force the next
        // paint to pick the new focus state up. The damage gate stays
        // safe (no snapshot churn) — we just invalidate the cached
        // bitmap so AppKit re-runs `draw(_:)` with the cached snapshot.
        needsDisplay = true
    }

    override var isFlipped: Bool {
        true
    }

    override var acceptsFirstResponder: Bool {
        true
    }

    func bind(handle: CopadTermFFI.Handle?) {
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

    /// Display-link callback. Runs on the main runloop at vsync.
    /// Drains alacritty's per-row damage via `takeDamageRows` so the
    /// view's invalidation is scoped to the rows that actually
    /// changed — cursor blink no longer triggers an 80×24 redraw.
    /// When a TUI-driven blinking cursor is active, an additional
    /// 2 Hz tick forces a single-cell repaint to advance the blink
    /// phase. Also drains the OSC 52 clipboard-request queue so
    /// paste requests from inside the terminal flow through the
    /// policy gate.
    @objc private func displayLinkFired(_: CADisplayLink) {
        guard let handle = termHandle else { return }
        drainClipboardRequests(handle)
        drainChildExit(handle)
        let damage = handle.takeDamageRows()
        let blinkPhaseChanged = advanceBlinkPhase()

        // Even a no-op damage drain must capture a fresh snapshot when
        // we're going to repaint (blink toggle still needs the latest
        // cursor row). `takeDamageRows` already advanced the FFI's
        // internal prev-state — recovering the snapshot here is the
        // only side-effect we still owe.
        switch damage {
        case .full:
            snapshotCache = handle.snapshot()
            needsDisplay = true
        case let .rows(rows):
            guard !rows.isEmpty || blinkPhaseChanged else { return }
            snapshotCache = handle.snapshot()
            // Always include the cursor row when the blink phase
            // flipped this tick. Without this, a blink tick that
            // happens to coincide with unrelated row damage would
            // skip the cursor cell — the user-visible "blink
            // stopped" symptom codex C1 flagged.
            var dirty = rows
            if blinkPhaseChanged, let cursor = snapshotCache?.cursor,
               !dirty.contains(cursor.row)
            {
                dirty.append(cursor.row)
            }
            invalidateRows(dirty)
        }
    }

    /// Convert a list of dirty viewport row indices into AppKit
    /// `setNeedsDisplay(_:)` calls. AppKit unions the resulting dirty
    /// rectangles automatically before the next `draw(_:)`, so
    /// per-row calls compose into a single (possibly tight) repaint
    /// region without explicit rect-merging on our side. Layer-backed
    /// views preserve pixels outside the union from the backing
    /// store — that's what makes the per-row invalidation a win.
    private func invalidateRows(_ rows: [UInt16]) {
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

    /// Apply the user's OSC 52 policy to any pending clipboard write
    /// request. `.allow` writes through to NSPasteboard.general;
    /// `.deny` (the secure default) drops with a stderr warning so a
    /// rogue program in the terminal can't silently overwrite the
    /// user's clipboard. Matches the SwiftTerm path's behavior.
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

    /// Drain alacritty's child-exit latch — when the PTY child
    /// (shell) terminates, broadcast `panel.exited` so copad-core's
    /// ContextService clears per-panel cwd/active state. Matches the
    /// cross-platform contract the SwiftTerm path used to honor via
    /// `processTerminated`. Fires at most once per Term lifetime;
    /// repeated polls after the first return false.
    private func drainChildExit(_ handle: CopadTermFFI.Handle) {
        guard handle.takeChildExit() else { return }
        eventBus?.broadcast(event: "panel.exited", data: ["panel_id": panelID])
    }

    /// Toggle the cursor visibility once per `blinkInterval` whenever
    /// the most recent snapshot reports `cursor.blink == 1`. Restores
    /// the cursor to visible if a previously-blinking TUI handed back
    /// to a steady cursor — otherwise the cursor could stick off.
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
        guard let snap = snapshotCache,
              let ctx = NSGraphicsContext.current?.cgContext
        else { return }

        // Fill the bounds with theme background unless we're in a
        // transparent-default-bg mode. AppKit clips this context to
        // `dirtyRect`, so `ctx.fill(bounds)` here only actually paints
        // the dirty area — non-dirty rows keep their backing-store
        // pixels intact (the win of layer-backed partial repaint).
        //
        // Two modes activate transparent bg:
        //   (a) `transparentDefaultBg && imageBackgroundActive` — image
        //       layer underneath shows through blank cells; or
        //   (b) `windowOpacity < 1.0` — Ghostty model: layer bg is
        //       `theme.background.alpha=opacity`, blurred desktop /
        //       wallpaper underneath bleeds through. Skipping the
        //       opaque bounds fill is what lets that alpha take effect.
        // Cells with explicit ANSI bg or reverse-video still materialize
        // opaque in `drawRow` (Zed pattern) under both modes, so
        // colored content stays legible.
        if !isTransparentBgActive {
            ctx.setFillColor(theme.background.nsColor.cgColor)
            ctx.fill(bounds)
        }

        // CTLineDraw uses CoreGraphics-native y-up glyph orientation.
        // Our view is `isFlipped = true` (so row 0 is at the top
        // visually) — without this textMatrix flip the glyphs render
        // upside-down + mirrored against the flipped CTM. Save/restore
        // the prior state so we don't leak the flip into non-text
        // drawing later.
        ctx.saveGState()
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        defer { ctx.restoreGState() }

        // Per-row dirty intersect: skip rows whose y-band doesn't
        // overlap the AppKit-supplied dirty rect. `cellHeight > 0`
        // guarded so the very first paint (pre-layout) still draws
        // every row.
        let snapRows = snap.rows
        let useDirtyClip = cellHeight > 0
        for row in 0 ..< snapRows {
            if useDirtyClip {
                let rowRect = CGRect(
                    x: 0,
                    y: CGFloat(row) * cellHeight,
                    width: bounds.width,
                    height: cellHeight,
                )
                if !rowRect.intersects(dirtyRect) { continue }
            }
            let runs = snap.rowRuns(row)
            let utf8 = snap.rowUtf8(row)
            guard runs.count > 0, utf8.count > 0 else { continue }
            drawRow(row: row, runs: runs, utf8: utf8, ctx: ctx)
        }

        // Cursor on top of the glyph layer: nvim/htop/etc. paint the
        // cursor cell with their own highlight group (CursorLine,
        // Cursor) which previously covered an early-drawn cursor. For
        // block style we then re-render the cell glyph in
        // theme.background so the character under the cursor stays
        // readable (xterm/iTerm2/Terminal.app convention).
        drawCursor(snap: snap, ctx: ctx)

        // Selection highlight last so it tints OVER the text instead
        // of getting covered by per-cell bg fills. theme.surface2 at
        // ~0.4 alpha keeps the underlying text legible while clearly
        // marking the range.
        paintSelection(snap.selection, ctx: ctx)

        // In-terminal find match highlight. Painted in theme.accent at
        // moderate alpha so it's visually distinct from the selection
        // overlay (surface2) — the user can tell at a glance which
        // band is the search hit vs which is their drag selection.
        paintSearchMatch(snap.searchMatch, ctx: ctx)

        // IME preedit overlay (Korean / Japanese / Chinese composition).
        // Paints OVER everything else at the cursor cell — what the
        // user is composing, before any of it touches the PTY.
        paintMarkedText(snap.cursor, ctx: ctx)
    }

    /// Paint the current find match. Single-row matches are a single
    /// rect (start_col..=end_col on the match row); multi-row matches
    /// (rare — pattern crossed an autowrap) paint each affected row:
    /// start row from start_col to last column, full middle rows,
    /// end row from column 0 to end_col. Theme.accent at ~0.45 alpha.
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

    /// Paint the in-progress IME composition at the cursor cell.
    /// Fills the underlying cells with theme.background opaque so the
    /// preedit is legible regardless of what was there before, then
    /// draws the marked string with an underline (single line for the
    /// whole composition; the IME-highlighted sub-range gets a thicker
    /// double underline).
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
        // IME preedit is overwhelmingly CJK/Hangul — the very thing the
        // base monospace face doesn't cover. Walk composed character
        // sequences (so e.g. a half-typed Hangul syllable stays one
        // unit) and swap `.font` per cluster when cascade picks a
        // different face.
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
        // Thicker double underline on the IME-highlighted sub-range
        // so the user can see which syllable / kana is "active" in a
        // multi-segment composition.
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
        // Typographic width tells us how many cells the preedit covers;
        // round up to a whole cell so the bg fill aligns with the grid.
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

        // CTLineDraw needs the text matrix flip that the main row loop
        // already applied; we're inside its scope (the defer-restore
        // hasn't fired yet) so `textPosition` + draw is correct.
        ctx.textPosition = CGPoint(x: x, y: y + ascent)
        CTLineDraw(line, ctx)
    }

    /// Paint a translucent `theme.surface2` overlay across the cells
    /// covered by the active selection. `end_row` / `end_col` are
    /// inclusive per alacritty's `SelectionRange` convention — paint
    /// `end_col - start_col + 1` cells on the end-row.
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

        // Block (rectangular) selection: each row paints the same column
        // span. is_block flows from alacritty's `SelectionRange.is_block`
        // through the snapshot wire; the FFI start-kind is what put us
        // in block mode (Option+drag). Span endpoints come in
        // pre-normalized — start_col ≤ end_col already.
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
            // Single-row selection: only the start_col..=end_col span.
            // Multi-row: start_row covers start_col..=lastCol, end_row
            // covers 0..=end_col, intermediate rows cover the full width.
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

    /// Cursor render. Style 0 = hidden (skip). Block (1) fills the
    /// whole cell, then re-renders the cell glyph in theme.background
    /// so the character under the cursor stays legible. Beam (2) is a
    /// 2-px vertical bar at the cell's leading edge. Underline (3)
    /// is a 2-px horizontal bar at the cell's bottom. When the window
    /// isn't key (e.g. user switched apps), block style draws as a
    /// hollow outline — Terminal.app + iTerm2 do the same.
    ///
    /// On busy wallpapers the accent block can blend into a low-
    /// contrast wallpaper pixel (Catppuccin mauve on a dark-purple
    /// image, for instance), so when an image background is active we
    /// edge every variant with a 1-px theme.background outline. The
    /// dark frame is invisible against the normal background but
    /// guarantees the cursor stays distinguishable from any wallpaper
    /// pixel underneath.
    private func drawCursor(snap: CopadTermFFI.Snapshot, ctx: CGContext) {
        let cursor = snap.cursor
        guard cursor.style != 0,
              cellWidth > 0, cellHeight > 0,
              // Honor TUI-requested blink: skip the draw on the OFF
              // phase so the cursor actually disappears between
              // `blinkInterval` ticks. Steady cursors (`blink == 0`)
              // ignore `blinkVisible` entirely.
              cursor.blink == 0 || blinkVisible
        else { return }
        let x = CGFloat(cursor.col) * cellWidth
        let y = CGFloat(cursor.row) * cellHeight
        let cell = CGRect(x: x, y: y, width: cellWidth, height: cellHeight)
        let isKey = window?.isKeyWindow ?? false
        let color = theme.accent.nsColor.cgColor
        // Cursor outline is for legibility against an unknown backdrop.
        // Both image mode and window-opacity mode put unpredictable
        // pixels behind the accent fill (wallpaper or blurred desktop),
        // so the dark frame around the accent helps in either.
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
                    // Stroke straddles the path; inset so the dark
                    // frame lands inside the just-filled cell rather
                    // than bleeding into neighbouring cells.
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

    /// Paint the glyph at the cursor cell using theme.background as
    /// the foreground color, so it stands out against the accent
    /// block underneath. Honors bold/italic flags on the underlying
    /// run so styled text under the cursor still reads correctly.
    private func redrawCursorGlyph(snap: CopadTermFFI.Snapshot, ctx: CGContext) {
        let cursor = snap.cursor
        let runs = snap.rowRuns(cursor.row)
        let utf8 = snap.rowUtf8(cursor.row)

        // Runs are emitted per cell (or per wide-cell-pair), so the
        // cursor sits inside exactly one run. Wide chars: cursor lands
        // on the leading half, so start_col == cursor.col still holds.
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

        // Pick the byte range and draw position. Three shapes:
        //   1. Aggregated uniform-ASCII run (multi-cell, all same byte):
        //      take exactly one byte and draw at the cursor cell's x.
        //      Drawing the full run would overpaint adjacent cells.
        //   2. Wide char (WIDE_LEADING flag, 2-cell span): draw the
        //      full utf8 at the run's start (= cursor.col for the
        //      leading half).
        //   3. Single cell, possibly with combining marks: draw the
        //      full utf8 at the run's start (= cursor.col).
        let flagBold: UInt16 = 1 << 0
        let flagItalic: UInt16 = 1 << 1
        let flagWideLeading: UInt16 = 1 << 7
        let runSpan = run.end_col - run.start_col
        let isWide = run.flags & flagWideLeading != 0
        let isAggregatedUniform = !isWide && runSpan > 1

        let drawBytes: UnsafeBufferPointer<UInt8>
        let drawX: CGFloat
        if isAggregatedUniform {
            // Every byte in the run is the same ASCII char by
            // construction (see walk_row in copad-term).
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

        // Cursor cell paints text in the inverted color: fg ←
        // theme.background so it's legible against the accent-filled
        // cursor block. No underline / strike on the cursor glyph.
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

    private func drawRow(
        row: UInt16,
        runs: UnsafeBufferPointer<CopadRun>,
        utf8: UnsafeBufferPointer<UInt8>,
        ctx: CGContext,
    ) {
        // Baseline in flipped coords: top of row + ascent.
        let baselineY = CGFloat(row) * cellHeight + ascent
        let defaultFg = theme.foreground.nsColor.cgColor
        let defaultBg = theme.background.nsColor.cgColor

        // Flag bits mirror copad_term::flags (see
        // copad-term/src/lib.rs). Kept as Swift constants to avoid
        // a third source of truth.
        let flagBold: UInt16 = 1 << 0
        let flagItalic: UInt16 = 1 << 1
        let flagInverse: UInt16 = 1 << 3
        let flagDim: UInt16 = 1 << 4
        let flagStrike: UInt16 = 1 << 5

        let transparentMode = isTransparentBgActive

        for i in 0 ..< runs.count {
            let run = runs[i]

            // Provenance — was this cell's bg from the default sentinel
            // (`run.bg_rgba == 0`)? We need to know BEFORE resolving so
            // we can decide whether transparent mode applies. Equality
            // check on the resolved color is not enough: an explicit
            // ANSI bg that happens to equal theme.background should
            // still paint (it's a real intent), and a real default cell
            // should NOT paint in transparent mode even though its
            // resolved color matches theme.bg.
            let bgIsDefault = run.bg_rgba == 0
            let isInverse = run.flags & flagInverse != 0

            // Resolve colors then apply inverse swap. Default-bg
            // materializes to theme.background BEFORE the swap (Zed
            // pattern from §Phase 3 in the plan — reverse-video over
            // transparent bg would render invisibly without it).
            var fg = resolveColor(run.fg_rgba, defaultColor: defaultFg)
            var bg = resolveColor(run.bg_rgba, defaultColor: defaultBg)
            if isInverse {
                swap(&fg, &bg)
            }
            // Dim → reduce fg alpha. ANSI spec is intentionally vague
            // here; ~65% is the conventional value across emulators.
            if run.flags & flagDim != 0, let dimmed = fg.copy(alpha: 0.65) {
                fg = dimmed
            }

            let x = CGFloat(run.start_col) * cellWidth
            let cellsWide = CGFloat(run.end_col - run.start_col)
            let cellRect = CGRect(x: x, y: CGFloat(row) * cellHeight,
                                  width: cellsWide * cellWidth, height: cellHeight)

            // Per-cell bg fill — overrides the global bounds fill.
            //   Opaque mode: skip when the resolved bg equals theme.bg
            //     (the bounds fill already covered it).
            //   Transparent mode: skip only when the cell came from the
            //     default sentinel AND is not inverse — those are the
            //     only cells we let the image bleed through. Inverse +
            //     default-bg is opaque theme.fg after swap and must
            //     still paint.
            let skipFill = transparentMode
                ? (bgIsDefault && !isInverse)
                : cgColorsApproxEqual(bg, defaultBg)
            if !skipFill {
                ctx.setFillColor(bg)
                ctx.fill(cellRect)
            }

            // Text. Empty/whitespace skipped to save a CTLine alloc.
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
            // styleBits packs the visual decoration into the cache
            // key. Underline byte is the raw FFI value (preserves
            // forward-compat for richer underline variants the renderer
            // doesn't yet distinguish — current code folds non-zero to
            // single, but the key still discriminates by raw style so
            // future expansion doesn't silently reuse stale lines).
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
        }
    }

    /// Cheap component-wise equality for the "is this cell's bg the
    /// same as the bounds fill we already did" early-out. Falls back
    /// to ObjectIdentifier when components aren't comparable (mixed
    /// color spaces).
    private func cgColorsApproxEqual(_ a: CGColor, _ b: CGColor) -> Bool {
        guard let ac = a.components, let bc = b.components, ac.count == bc.count else { return false }
        for (x, y) in zip(ac, bc) where abs(x - y) > 0.001 {
            return false
        }
        return true
    }

    // MARK: - Mouse selection

    /// Convert a window-coord mouse event into a grid (row, col, side)
    /// triple, clamping out-of-bounds drag positions so the FFI sees
    /// a valid `UInt16`. AppKit fires mouseDragged with coordinates
    /// outside the view bounds when the user drags past the edge —
    /// that's normal and should clamp to the nearest visible cell.
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

    /// 1-click → simple drag selection, 2 → semantic (word), 3+ →
    /// lines. Matches the iTerm2 / Terminal.app convention. Option held
    /// on a single click flips to rectangular (block) selection —
    /// matches both Terminal.app and iTerm2 (Option-drag, no Cmd; Cmd
    /// is reserved for URL-click). Word / line selection ignore the
    /// modifier because they're keyed off click count, not drag region.
    private func selectionKind(for event: NSEvent) -> CopadTermFFI.Handle.SelectionKind {
        switch event.clickCount {
        case 2: .word
        case let n where n >= 3: .line
        default: event.modifierFlags.contains(.option) ? .block : .simple
        }
    }

    /// When a TUI has any mouse-reporting mode on (`vim` with
    /// `set mouse=a`, `less`, `htop`, tmux with `set -g mouse on`,
    /// …), the click/drag goes to the TUI — Shift held overrides so
    /// the user can still grab text into the host selection.
    /// Forwarding (`forwardMouseEvent`) covers wheel + press/release +
    /// drag-with-button-held; bare-cursor MOTION-level forwarding
    /// (`\e[?1003h`) is not wired yet — rare in practice, deferred.
    private func shouldHandleAsSelection(_ event: NSEvent) -> Bool {
        if event.modifierFlags.contains(.shift) { return true }
        return !(termHandle?.mouseModeActive ?? false)
    }

    // MARK: - Mouse event forwarding (mouse-mode TUIs)

    /// "press" emits SGR `M` (or legacy press byte); "release" emits
    /// SGR `m` (or legacy button=3 release-marker); "motion" emits
    /// SGR `M` with the motion bit (32) set in the button code. Used
    /// only when a TUI has mouse reporting on AND Shift is not held.
    private enum MouseForwardKind { case press, release, motion }

    /// Last cell forwarded by a motion event. -1 = sentinel meaning
    /// "next motion will emit unconditionally". Reset on every
    /// press/release so the first drag-tick after a fresh click fires
    /// even when the cursor hasn't crossed a cell boundary. Without
    /// this, dragging within a single cell would never publish.
    private var lastMotionCol: Int = -1
    private var lastMotionRow: Int = -1

    private func forwardMouseEvent(event: NSEvent, button: Int, kind: MouseForwardKind) {
        guard let h = termHandle else { return }
        let encoding = h.mouseEncoding
        guard encoding != .none else { return }
        let (col, row) = mouseCellCoord(for: event)
        if kind == .motion {
            // Cell-granularity throttle. Motion events fire every few
            // pixels on the trackpad — the TUI doesn't care about
            // sub-cell precision, so dedupe by grid coord.
            guard col != lastMotionCol || row != lastMotionRow else { return }
            lastMotionCol = col
            lastMotionRow = row
        } else {
            lastMotionCol = -1
            lastMotionRow = -1
        }
        let modBits = mouseModifierBits(event)
        let motionBit = (kind == .motion) ? 32 : 0
        // SGR carries the button identity through release; legacy /
        // UTF8 collapse all releases to button-code 3 (xterm convention).
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

    /// 1-based grid coords clamped to the *visible grid extent* (not
    /// just UInt16). AppKit emits dragged events with coordinates
    /// outside the view bounds when the user drags past the edge —
    /// without clamping to (cols, rows), the TUI receives a report
    /// for e.g. row 65000 and silently ignores it instead of treating
    /// it as a drag-to-edge. Anchored at the mouse position — same
    /// convention as `forwardWheel`.
    private func mouseCellCoord(for event: NSEvent) -> (col: Int, row: Int) {
        guard cellWidth > 0, cellHeight > 0 else { return (1, 1) }
        let local = convert(event.locationInWindow, from: nil)
        let maxCol = max(1, Int(bounds.width / cellWidth))
        let maxRow = max(1, Int(bounds.height / cellHeight))
        let col = max(1, min(Int((local.x / cellWidth).rounded(.down)) + 1, maxCol))
        let row = max(1, min(Int((local.y / cellHeight).rounded(.down)) + 1, maxRow))
        return (col, row)
    }

    /// xterm mouse-mode modifier bits: 4=shift, 8=alt/meta, 16=ctrl.
    /// Shift is intentionally never set — Shift held already bypasses
    /// forwarding entirely (`shouldHandleAsSelection`), so the bit
    /// value is moot for our wire output and excluding it keeps the
    /// "Shift = host selection" contract crisp on the TUI side.
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
            // Single-byte coord cap at 223 (mouse-spec); +32 offset
            // matches `forwardWheel`'s legacy path.
            let cb = UInt8(min(255, buttonCode + 32))
            let cc = UInt8(min(255, col + 32))
            let cr = UInt8(min(255, row + 32))
            handle.input([0x1B, 0x5B, 0x4D, cb, cc, cr])
        case .none:
            return
        }
    }

    /// Whether a drag event should forward as a motion report. CLICK
    /// level only wants press/release; DRAG and MOTION want drag too.
    private func shouldForwardDragMotion() -> Bool {
        switch termHandle?.mouseReportLevel ?? .none {
        case .drag, .motion: true
        case .none, .click: false
        }
    }

    override func mouseDown(with event: NSEvent) {
        // Always take first responder on click, even if we're going to
        // bail out for mouse-mode TUI handling. An unfocused alacritty
        // pane needs to become focusable on click regardless of whether
        // the click also starts a selection — otherwise the subsequent
        // Cmd+C / keyboard interaction has no responder target.
        window?.makeFirstResponder(self)

        // Cmd+click takes priority over selection: try OSC 8 hyperlink
        // first, fall back to plain-text URL regex on the clicked row.
        // Matches iTerm2 / Terminal.app / SwiftTerm path behavior.
        if event.modifierFlags.contains(.command) {
            if openURLAtClick(event) {
                return
            }
            // No URL at that point — fall through to normal mouseDown
            // so the user gets a selection start instead of nothing.
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
        needsDisplay = true
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

    /// Resolve the URL at a Cmd+click point and hand it to NSWorkspace.
    /// Returns true when a URL was opened (so mouseDown can short-
    /// circuit). Checks OSC 8 first via the snapshot's hyperlink table,
    /// then falls back to URLClickHelper's plain-text regex.
    private func openURLAtClick(_ event: NSEvent) -> Bool {
        guard let snap = snapshotCache,
              let (row, col, _) = gridLocation(for: event)
        else { return false }

        // OSC 8: walk the row's runs for one whose hyperlink_id !=0 and
        // whose `start_col..<end_col` covers the clicked column.
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

        // Plain text: decode the row's utf8 and find a regex match
        // containing the clicked column. NSRegularExpression operates
        // on UTF-16 units; ASCII-dominant URL text lines up with the
        // column index, so range.contains(col) works for the common
        // case. Wide chars upstream shift the offset — accept that
        // mismatch (URLClickHelper takes the same trade-off).
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
            // TUI owns the drag — forward as a motion-with-button-1
            // when the TUI subscribed to DRAG / MOTION level. CLICK-
            // level subscribers only see press/release, never drag.
            if shouldForwardDragMotion() {
                forwardMouseEvent(event: event, button: 0, kind: .motion)
            }
            return
        }
        guard let (row, col, side) = gridLocation(for: event), let h = termHandle else {
            return
        }
        h.selectionUpdate(row: row, col: col, side: side)
        needsDisplay = true
    }

    // MARK: - Scrolling

    /// Mouse wheel / trackpad scroll. Maps NSEvent's `scrollingDeltaY`
    /// into an integer line count and tells alacritty's grid to shift
    /// `display_offset`. Positive deltaY ("natural" scroll: fingers
    /// down on trackpad, or wheel back on a mouse) brings older content
    /// into view; the FFI's `scrollLines(positive)` does the same.
    override func scrollWheel(with event: NSEvent) {
        guard let h = termHandle, cellHeight > 0 else {
            super.scrollWheel(with: event)
            return
        }
        let dy = event.scrollingDeltaY
        let lines: Int
        if event.hasPreciseScrollingDeltas {
            // Trackpad — fractional pixel deltas. Accumulate so slow
            // swipes don't round to zero on every tick.
            accumulatedScrollDelta += dy
            let whole = (accumulatedScrollDelta / cellHeight).rounded(.towardZero)
            lines = Int(whole)
            accumulatedScrollDelta -= whole * cellHeight
        } else {
            // Mouse wheel — `scrollingDeltaY` is roughly line-count
            // shaped already (≈ 1 per notch on most devices). No
            // accumulator needed; rounding toward zero matches the
            // direction of partial deltas.
            lines = Int(dy.rounded(.towardZero))
        }
        if lines == 0 { return }

        // If the TUI has mouse reporting on and the user didn't hold
        // Shift to override, forward wheel events to the PTY so the
        // TUI's own scrollback (tmux copy mode, less, nvim with
        // `set mouse=a`) advances. Host-side scrollback only
        // matters when the TUI isn't capturing input.
        let encoding = h.mouseEncoding
        if encoding != .none, !event.modifierFlags.contains(.shift) {
            forwardWheel(event: event, lines: lines, encoding: encoding, handle: h)
            return
        }

        h.scrollLines(Int32(lines))
        // No needsDisplay here — the vsync displayLink will see
        // the state-hash change on its next tick (≤16ms) and
        // schedule the redraw with a FRESH snapshot. Marking
        // dirty inline caused a double-render per scroll event:
        // AppKit drew the stale snapshotCache, then vsync drew
        // again with the post-scroll snapshot.
    }

    /// Send `\e[<64;col;rowM` (SGR wheel-up) or `\e[<65;col;rowm`
    /// (wheel-down) for each accumulated line, so tmux/less/nvim get
    /// the same events they'd see from xterm. Legacy/UTF8 encodings
    /// pad coords by 32 and cap at 223 (single-byte limit). One event
    /// per line keeps the TUI's scroll rate matching the user's input
    /// rate; we don't try to coalesce.
    private func forwardWheel(
        event: NSEvent,
        lines: Int,
        encoding: CopadTermFFI.Handle.MouseEncoding,
        handle: CopadTermFFI.Handle,
    ) {
        // SGR/legacy/UTF8 all use 1-based grid coords. Anchor at the
        // mouse position when the wheel fired (not the cursor) — that
        // matches xterm and lets tmux pick the right pane on
        // multi-pane layouts.
        let local = convert(event.locationInWindow, from: nil)
        let col = max(1, min(Int((local.x / cellWidth).rounded(.down)) + 1, Int(UInt16.max)))
        let row = max(1, min(Int((local.y / cellHeight).rounded(.down)) + 1, Int(UInt16.max)))
        // SGR/X10 wheel buttons: 64 = up (older content), 65 = down.
        // `lines > 0` matches our scrollLines convention (positive =
        // older content), which lines up with wheel-up.
        let button = lines > 0 ? 64 : 65
        let count = abs(lines)
        for _ in 0 ..< count {
            switch encoding {
            case .sgr:
                let s = "\u{1B}[<\(button);\(col);\(row)M"
                handle.input(Array(s.utf8))
            case .legacy, .utf8:
                // Coord byte = value + 32, clamped to 255 (single-byte
                // limit). UTF8 mode technically supports 2-byte coords
                // for >223 but tmux/SGR consumers wouldn't be on this
                // path anyway — we keep the encoding minimal.
                let cb = UInt8(min(255, button + 32))
                let cc = UInt8(min(255, col + 32))
                let cr = UInt8(min(255, row + 32))
                handle.input([0x1B, 0x5B, 0x4D, cb, cc, cr])
            case .none:
                return
            }
        }
    }

    /// Bring the view back to the live bottom. Called before sending
    /// user input to the PTY (typing) — convention is that any input
    /// dismisses the scrolled-back view so the user sees what they
    /// just typed land. PTY-side output (which arrives without a key
    /// press) leaves the scrolled state alone.
    private func scrollToBottomOnInput() {
        termHandle?.scrollToBottom()
    }

    // MARK: - Clipboard / Edit responder actions

    /// Standard responder action; fires for Cmd+C via the Edit menu
    /// key equivalent (which AppKit dispatches through the responder
    /// chain BEFORE keyDown ever runs). No selection → no-op so the
    /// chain continues to the next handler (matches Terminal.app).
    /// Not `override`-marked because NSResponder's cut/copy/paste are
    /// informal actions in Swift's bridging — they exist as Objective-C
    /// methods but aren't declared as overridable on NSView in Swift.
    /// `@objc` is enough to put them on the responder chain.
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
        needsDisplay = true
    }

    /// Cmd+V dispatch. Wraps the pasted bytes in bracketed-paste
    /// markers (`\e[200~ … \e[201~`) when the program enabled
    /// `\e[?2004h` — that's how zsh, vim's `set paste`, and modern
    /// shells distinguish pasted bytes from typed bytes.
    ///
    /// The `terminal.output` event carries the user-visible paste text
    /// (without the bracketed wrapper), matching how VTE's `commit`
    /// signal hides its own bracketed wrapper from listeners on Linux.
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

    /// Signal that we'll accept the drag — required for AppKit to
    /// emit `performDragOperation`. `.copy` matches user intent: we're
    /// pasting a path / saving an image, not "moving" anything.
    override func draggingEntered(_: NSDraggingInfo) -> NSDragOperation {
        .copy
    }

    /// Materialize the dropped pasteboard into a single shell-quoted
    /// text payload and route it through the standard paste path
    /// (`sendPaste` already handles bracketed-paste mode +
    /// `terminal.output` event publication).
    ///
    /// Three drop shapes, in priority order:
    ///   1. File URLs (Finder, multi-select, any app exporting files):
    ///      shell-quoted absolute paths joined by spaces.
    ///   2. Raw image bytes (browser drag, screenshot buffer, NSImage
    ///      drag): saved as PNG to `~/Library/Caches/copad/drops/`,
    ///      then the saved path is pasted.
    ///   3. Web URLs: pasted as bare text.
    ///
    /// Returning `false` falls back to AppKit's "drop rejected"
    /// animation — used when the pasteboard had nothing usable.
    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        let pb = sender.draggingPasteboard

        // Priority 1: file URLs. `readObjects` with
        // `urlReadingFileURLsOnly` filters out web URLs so they fall
        // through to priority 3 below — that's the right precedence
        // because Finder always exports `.fileURL` alongside `.URL`
        // and we want the local path, not the `file://` URL string.
        if let fileURLs = pb.readObjects(
            forClasses: [NSURL.self],
            options: [.urlReadingFileURLsOnly: true],
        ) as? [URL], !fileURLs.isEmpty {
            let quoted = fileURLs.map { Self.shellQuote($0.path) }.joined(separator: " ")
            sendPaste(quoted)
            return true
        }

        // Priority 2: raw image bytes. PNG first since browsers tend
        // to export it directly; TIFF as fallback because NSImage drag
        // (from Preview, screenshot buffer at Cmd+Shift+4 +Ctrl) lands
        // as `.tiff`. Both paths normalize to PNG on disk so callers
        // don't fork on format.
        let imageData = pb.data(forType: .png) ?? pb.data(forType: .tiff)
        if let data = imageData,
           let path = saveDroppedImage(data, prefersPng: pb.data(forType: .png) != nil)
        {
            sendPaste(Self.shellQuote(path))
            return true
        }

        // Priority 3: non-file URLs (web links). Pasted as text so the
        // shell / CLI receiving it can decide what to do (curl,
        // browser open, Claude Code ingestion, etc.).
        if let urls = pb.readObjects(forClasses: [NSURL.self], options: nil) as? [URL],
           let url = urls.first
        {
            sendPaste(url.absoluteString)
            return true
        }

        return false
    }

    /// POSIX single-quote escape: every `'` becomes `'\''` and the
    /// whole string is wrapped in `'...'`. Safe for any shell-
    /// interpreted character (spaces, `$`, backticks, glob chars).
    /// Empty string round-trips as `''`.
    private static func shellQuote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// Convert a TIFF/PNG byte blob to PNG and save under
    /// `~/Library/Caches/copad/drops/<timestamp>-<n>.png`. Returns
    /// the absolute path on success, `nil` if either the directory
    /// can't be created or PNG re-encoding fails (rare — only
    /// happens with malformed image data).
    ///
    /// `prefersPng = true` means the input is already PNG and we
    /// could write the bytes directly. We re-encode anyway so the
    /// on-disk file always carries the `.png` extension that matches
    /// its format (avoids `.tiff`-extension confusion if the user
    /// inspects the cache later).
    private func saveDroppedImage(_ data: Data, prefersPng: Bool) -> String? {
        _ = prefersPng // routed through the same encoder either way
        guard let dir = Self.dropsCacheDir() else { return nil }
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy-MM-dd-HHmmss"
        let stamp = formatter.string(from: Date())
        // Add a random 4-hex suffix so back-to-back drops in the same
        // second don't collide on the same filename.
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

    /// `~/Library/Caches/copad/drops/` — created lazily on first
    /// drop. `nil` only if the FileManager refuses to create the
    /// directory (read-only home, etc.), which is rare enough we
    /// just log + give up the drop.
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

    /// Centralized PTY-input dispatch for keyboard paths (typed text,
    /// IME commits, control combos, command-key shortcuts, special
    /// keys like arrows / enter / delete). Writes to the PTY AND
    /// publishes `terminal.output` so trigger consumers (AI agents,
    /// shell-watchers) can observe what the user typed. Matches Linux
    /// `copad-linux/src/tabs.rs`'s VTE `connect_commit` hook —
    /// keyboard / paste only; mouse-mode wheel forwarding intentionally
    /// stays on the raw `termHandle.input` path because VTE excludes
    /// mouse from `commit`. The "output" naming follows the terminal-
    /// widget perspective: bytes going OUT of the widget to the PTY.
    private func sendInput(_ bytes: [UInt8]) {
        termHandle?.input(bytes)
        publishTerminalOutput(bytes)
    }

    /// Broadcast a `terminal.output` event with the input bytes
    /// decoded as utf8. ASCII text, IME-committed unicode, and escape
    /// sequences for special keys all decode fine; truly binary input
    /// (rare for keyboard-driven paths) is dropped silently rather
    /// than emitting a weird placeholder.
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
        // Scroll navigation: Cmd+Up/Down (line), Cmd+Home/End (top/
        // bottom), Shift+PageUp/PageDown (page). These DON'T forward
        // to the PTY — they're host-side viewport controls.
        if handleScrollKey(event, mods: mods) {
            return
        }
        if mods.contains(.command) {
            // Menu key equivalents already fired in performKeyEquivalent
            // before keyDown, so anything left here is a Cmd combo with
            // no menu binding. Map the readline-style line-edit ones
            // (Cmd+←/→/⌫/⌦) before falling through — super.keyDown on
            // these would just beep.
            if let bytes = commandKeyBytes(forKeyCode: event.keyCode) {
                scrollToBottomOnInput()
                sendInput(bytes)
                return
            }
            super.keyDown(with: event)
            return
        }
        if mods == .control, let bytes = controlBytes(for: event) {
            // User typed → jump back to bottom so the keypress lands
            // visibly. Matches Terminal.app / iTerm2 behavior.
            scrollToBottomOnInput()
            sendInput(bytes)
            return
        }
        // Force-Meta keys: Option + Return / Escape (whichever subset
        // the user opted into via `force_meta_keys`) send `ESC + <byte>`
        // regardless of `optionAsAlt`. These are control-char keys that
        // the optionAsAlt printable filter below intentionally drops,
        // so they need their own explicit branch. Default config
        // includes Return so Claude Code / Python REPL / ipython get
        // newline-in-prompt out of the box.
        if mods.subtracting(.shift) == .option,
           forceMetaKeyCodes.contains(event.keyCode),
           let bytes = forceMetaBytes(forKeyCode: event.keyCode)
        {
            scrollToBottomOnInput()
            sendInput(bytes)
            return
        }
        // Option-as-Alt: route Option+key as `ESC + base_char` so tmux /
        // zsh / readline see the Meta prefix instead of the macOS
        // dead-key composition that turns Option+1 into `¡` via the IME.
        // Shift is allowed to coexist (Option+Shift+1 → ESC `!`); Cmd /
        // Ctrl combinations already returned above so they don't reach
        // here. Non-printable keys (arrows, delete, function keys) fall
        // through to interpretKeyEvents so the existing
        // moveWordLeft/Right / deleteWordBackward bindings keep working.
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

    /// Build the `ESC + utf8(base_char)` byte sequence for an
    /// Option-modified keystroke. `charactersIgnoringModifiers` already
    /// applies Shift but ignores Option, so Option+1 → `"1"` and
    /// Option+Shift+1 → `"!"` — the exact byte we want to follow ESC
    /// with. Returns nil for non-printable keys (arrows, fn keys,
    /// control chars) so they fall back to the IME / doCommand path.
    private func optionAltBytes(for event: NSEvent) -> [UInt8]? {
        guard
            let chars = event.charactersIgnoringModifiers,
            let scalar = chars.unicodeScalars.first
        else { return nil }
        // Skip ASCII control range and the NSFunctionKey block
        // (0xF700+: arrows, F-keys, page up/down, ...).
        let v = scalar.value
        if v < 0x20 || v == 0x7F || v >= 0xF700 { return nil }
        var out: [UInt8] = [0x1B]
        out.append(contentsOf: Array(chars.utf8))
        return out
    }

    /// macOS virtual key codes for the keys we own as scroll shortcuts.
    /// Using `keyCode` instead of `characters` so IME-active keystrokes
    /// (Korean Caps-Lock toggle, Japanese Eisu mode, …) don't shadow
    /// the shortcuts when no character is delivered.
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

    /// Translate a `force_meta_keys` keyCode to the `ESC + <byte>` PTY
    /// sequence. Only keys present in `forceMetaKeyCodes` reach this —
    /// the keyDown caller filters first, so an unmapped keyCode here is
    /// a programming error (caller and parser must stay in sync). Add
    /// a case here when extending `parseForceMetaKeyCodes`.
    private func forceMetaBytes(forKeyCode kc: UInt16) -> [UInt8]? {
        switch kc {
        case KeyCode.returnKey: [0x1B, 0x0D] // ESC + CR
        case KeyCode.escape: [0x1B, 0x1B] // ESC + ESC
        default: nil
        }
    }

    /// Map user-facing `force_meta_keys` names to macOS virtual key
    /// codes. Case-insensitive; recognized aliases follow common
    /// keyboard nomenclature. Unknown entries emit one stderr line
    /// each and are dropped — invalid config never crashes the app.
    /// Returns a Set so the hot `keyDown` path does O(1) contains
    /// checks instead of walking `[String]` on every keystroke.
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

    /// Intercept Cmd / Shift-modified scroll keys before they reach
    /// the PTY. Returns true when the key was consumed as a scroll
    /// gesture; caller short-circuits in that case.
    private func handleScrollKey(_ event: NSEvent, mods: NSEvent.ModifierFlags) -> Bool {
        guard let h = termHandle else { return false }
        let kc = event.keyCode
        // Same pattern as scrollWheel: skip needsDisplay because the
        // vsync displayLink will pick up the state-hash change within
        // one frame and trigger a draw with a fresh snapshot. Marking
        // dirty inline caused a stale draw before the vsync redraw.
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

    /// Cmd+arrow / Cmd+delete line-edit shortcuts. iTerm2/Terminal.app
    /// convention: map to the equivalent readline byte sequences so
    /// shells (bash/zsh) and Insert-mode vim react as users expect.
    /// Lives next to controlBytes/commandBytes for parity, but called
    /// directly from keyDown because Cmd-modified events never reach
    /// interpretKeyEvents (we route them to super for menu dispatch).
    private func commandKeyBytes(forKeyCode kc: UInt16) -> [UInt8]? {
        switch kc {
        case KeyCode.left: [0x01] // Cmd+← → Ctrl+A (beginning-of-line)
        case KeyCode.right: [0x05] // Cmd+→ → Ctrl+E (end-of-line)
        case KeyCode.delete: [0x15] // Cmd+⌫ → Ctrl+U (unix-line-discard)
        case KeyCode.forwardDelete: [0x0B] // Cmd+⌦ → Ctrl+K (kill-line)
        default: nil
        }
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
        // IME commit: the system normally calls unmarkText() before
        // delivering the committed string, but some IMEs (and some
        // commit paths) skip that. Clear here too so the preedit
        // overlay doesn't linger after the bytes land in the PTY.
        if markedText != nil {
            markedText = nil
            needsDisplay = true
        }
        guard !text.isEmpty else { return }
        scrollToBottomOnInput()
        sendInput(Array(text.utf8))
    }

    override func doCommand(by selector: Selector) {
        if let bytes = commandBytes(for: selector) {
            scrollToBottomOnInput()
            sendInput(bytes)
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
        // Option+←/→ — readline backward-word / forward-word.
        case #selector(NSStandardKeyBindingResponding.moveWordLeft(_:)):
            [0x1B, 0x62] // ESC b
        case #selector(NSStandardKeyBindingResponding.moveWordRight(_:)):
            [0x1B, 0x66] // ESC f
        // Option+⌫/⌦ — readline backward-kill-word / kill-word.
        // ESC+DEL is the meta-backspace sequence bash/zsh bind out of
        // the box; raw Ctrl+W (0x17) would also delete word but ignores
        // the readline word-boundary config.
        case #selector(NSStandardKeyBindingResponding.deleteWordBackward(_:)):
            [0x1B, 0x7F]
        case #selector(NSStandardKeyBindingResponding.deleteWordForward(_:)):
            [0x1B, 0x64] // ESC d
        // Defensive: if a custom DefaultKeyBinding.dict or a third-party
        // text-input plugin synthesizes these line-edit selectors, route
        // them too. Normal flow hits commandKeyBytes() in keyDown first.
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

    // IME preedit support. NSTextInputClient hands us the in-progress
    // composition via setMarkedText; we store it and paint it as an
    // overlay at the cursor cell in `draw(_:)`. Nothing flows to the
    // PTY until the IME calls `insertText` with the committed string.

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange _: NSRange) {
        let text: String = if let s = string as? String { s }
        else if let a = string as? NSAttributedString { a.string }
        else { "" }
        if text.isEmpty {
            markedText = nil
        } else {
            markedText = text
            // Clamp the IME-highlighted sub-range to the actual length
            // — some IMEs (and dictation) send ranges that extend past
            // the marked string. Drawing with an out-of-range index
            // would crash CoreText.
            let utf16Count = (text as NSString).length
            let loc = max(0, min(selectedRange.location, utf16Count))
            let len = max(0, min(selectedRange.length, utf16Count - loc))
            markedSelectedRange = NSRange(location: loc, length: len)
        }
        needsDisplay = true
    }

    func unmarkText() {
        guard markedText != nil else { return }
        markedText = nil
        markedSelectedRange = NSRange(location: 0, length: 0)
        needsDisplay = true
    }

    /// IMEs query this to know where the caret sits inside the
    /// "document." We don't have a real text buffer, so report a
    /// zero-length range at the start — Korean / Japanese IMEs
    /// accept this and key off `markedRange` + `firstRect` instead.
    /// Returning NSNotFound here breaks several IMEs (no input).
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

    /// We don't expose the terminal buffer to the IME (it'd be
    /// awkward to map cell coordinates to NSRange offsets). Returning
    /// nil is fine — used mostly by accessibility / dictation paths
    /// that gracefully degrade.
    func attributedSubstring(forProposedRange _: NSRange, actualRange _: NSRangePointer?) -> NSAttributedString? {
        nil
    }

    /// Minimal set of attribute keys the IME can include in marked
    /// text. We honor underline via our own painting and ignore
    /// segment styles — sufficient for Korean/Japanese/Chinese IMEs.
    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        [.underlineStyle, .underlineColor]
    }

    /// Where the IME should anchor its candidate window. Returns the
    /// cursor cell's rect in *screen* coordinates — AppKit's IME
    /// pipeline expects screen-space here, not view or window. Without
    /// this the candidate popup floats at (0, 0) on the main display.
    /// View is `isFlipped == true` but `convert(_:to:)` already
    /// handles the flip between the view's top-left origin and the
    /// window's bottom-left origin — passing local flipped coords
    /// directly is correct (manually inverting y here double-flips
    /// and anchors the candidate window at the mirror row).
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

    /// Hit-test for clicking into a preedit composition — we don't
    /// support it, but returning a deterministic NSNotFound keeps
    /// the IME from probing further.
    func characterIndex(for _: NSPoint) -> Int {
        NSNotFound
    }
}

/// In-terminal find UI. Mirrors `copad-linux/src/search.rs`'s
/// SearchBar shape — text field + prev/next/case-toggle/close —
/// laid out at the bottom of the pane and toggled by Cmd+F. Owns no
/// search logic itself; reports user actions back to the controller
/// via an `Action` callback, which drives the Rust FFI + redraws.
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

    /// Authoritative case-sensitive state. Tracked separately from
    /// `NSButton.state` because the borderless `.accessoryBar` style
    /// gives no obvious click feedback — `setButtonType(.toggle)`
    /// toggles `.state` but the button doesn't render any visible
    /// "pressed" indicator without a border, so users would click
    /// without knowing whether it landed. `updateCaseButtonAppearance`
    /// repaints the title in the accent color when active.
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
        // Slightly lighter than the terminal background so the bar
        // visually separates from the text grid behind it.
        layer?.backgroundColor = theme.surface2.nsColor
            .withAlphaComponent(0.92).cgColor
    }

    func focusSearchField() {
        window?.makeFirstResponder(textField)
        // Select-all so a fresh Cmd+F over an existing query lets the
        // user type to replace immediately.
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

    /// Repaint the "Aa" title to make the toggle state obvious. We
    /// can't use `NSButton.state` for this because borderless buttons
    /// show no built-in pressed indicator; coloring the glyph in the
    /// accent color when active is the clearest signal at our 32-pt
    /// bar height.
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

    /// Keyboard handling on the bar itself — Esc closes, Shift+Enter
    /// goes to prev match (Enter alone is wired via the text field's
    /// action selector above for next match).
    override func keyDown(with event: NSEvent) {
        // Escape (keyCode 53)
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

    /// Re-route Esc from the embedded text field's field editor.
    /// Without this the field editor swallows Esc as "cancel editing"
    /// and the bar wouldn't close.
    func control(_: NSControl, textView _: NSTextView, doCommandBy selector: Selector) -> Bool {
        if selector == #selector(NSStandardKeyBindingResponding.cancelOperation(_:)) {
            onAction(.close)
            return true
        }
        if selector == #selector(NSStandardKeyBindingResponding.insertBacktab(_:)) {
            // Shift+Tab → prev match (alternative to Shift+Enter for
            // keyboards where Shift+Enter triggers something else).
            onAction(.prev)
            return true
        }
        return false
    }
}
