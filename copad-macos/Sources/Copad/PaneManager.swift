import AppKit
import CopadCore

/// NSSplitView that distributes subviews equally on the first resize pass; afterwards the user
/// can drag dividers freely. Done in the delegate's `resizeSubviewsWithOldSize` because
/// NSSplitView sets frames in resizeSubviews, before `layout()` runs.
private class EqualSplitView: NSSplitView, NSSplitViewDelegate {
    private var initialSizeSet = false

    override init(frame: NSRect) {
        super.init(frame: frame)
        delegate = self
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    func splitView(_ splitView: NSSplitView, resizeSubviewsWithOldSize _: NSSize) {
        let total = isVertical ? splitView.frame.width : splitView.frame.height
        guard total > 0, splitView.subviews.count >= 2 else {
            splitView.adjustSubviews()
            return
        }

        if initialSizeSet {
            splitView.adjustSubviews()
            return
        }
        initialSizeSet = true

        let n = splitView.subviews.count
        let eachSize = (total - dividerThickness * CGFloat(n - 1)) / CGFloat(n)
        if isVertical {
            var x: CGFloat = 0
            for sub in splitView.subviews {
                sub.frame = NSRect(x: x, y: 0, width: eachSize, height: splitView.frame.height)
                x += eachSize + dividerThickness
            }
        } else {
            var y: CGFloat = 0
            for sub in splitView.subviews {
                sub.frame = NSRect(x: 0, y: y, width: splitView.frame.width, height: eachSize)
                y += eachSize + dividerThickness
            }
        }
    }
}

enum InitialPanel {
    case terminal
    /// Terminal seeded with a cwd and/or initial PTY input (used by `claude.start`); separate from
    /// `.terminal` so existing callers stay unchanged.
    case terminalSeed(cwd: String?, initialInput: String?)
    case webview(url: URL?)
    /// Pre-constructed plugin panel; TabViewController builds it because it needs the registry
    /// and event bus.
    case pluginPanel(any CopadPanel)
}

/// Manages the split-pane tree for a single tab, rebuilding `containerView`'s contents on every
/// split/close.
@MainActor
final class PaneManager {
    /// Mutable so panes split after a config hot-reload get the new values; `applyTheme` /
    /// `applyFont` / `applyOSC52Policy` already update existing panes.
    private var config: CopadConfig
    private var theme: CopadTheme

    private(set) var root: SplitNode
    private(set) var activePane: any CopadPanel

    /// Stable container, pinned to contentArea once by TabViewController.
    let containerView: NSView

    var onLastPaneClosed: (() -> Void)?
    /// Fires after any new pane is added. The pane was built from `self.config`, which carries
    /// only load-time background state, so TabViewController uses this to apply runtime state
    /// (e.g. after `background.set`) or the new pane would cover the wallpaper.
    var onPaneAdded: ((any CopadPanel) -> Void)?
    var onActivePaneChanged: (() -> Void)?
    /// Fires when the split tree changes shape (e.g. a background terminal exiting via
    /// `close_on_exit`) even if the active pane is unchanged, so TabViewController can save the
    /// session (decision #61 C4).
    var onLayoutChanged: (() -> Void)?

    weak var eventBus: EventBus? {
        didSet { propagateEventBus() }
    }

    private nonisolated(unsafe) var clickMonitor: Any?
    /// Fill constraints on containerView, deactivated before the next rebuild.
    private var rootConstraints: [NSLayoutConstraint] = []

    // MARK: - Init

    init(config: CopadConfig, theme: CopadTheme, initialPanel: InitialPanel = .terminal) {
        self.config = config
        self.theme = theme

        let panel: any CopadPanel = switch initialPanel {
        case .terminal:
            Self.makeTerminalPanel(config: config, theme: theme)
        case let .terminalSeed(cwd, initialInput):
            Self.makeTerminalPanel(config: config, theme: theme, cwd: cwd, initialInput: initialInput)
        case let .webview(url):
            WebViewController(url: url)
        case let .pluginPanel(p):
            p
        }

        root = .leaf(panel)
        activePane = panel

        containerView = NSView()
        containerView.translatesAutoresizingMaskIntoConstraints = false

        wirePanel(panel)
        rebuildViewHierarchy()
        installClickMonitor()
    }

    deinit {
        if let m = clickMonitor { NSEvent.removeMonitor(m) }
    }

    // MARK: - Public API

    func splitActive(orientation: SplitOrientation) {
        let newTermVC = Self.makeTerminalPanel(config: config, theme: theme)
        assignEventBus(to: newTermVC)
        wirePanel(newTermVC)

        root = root.splitting(activePane, with: .leaf(newTermVC), orientation: orientation)

        rebuildViewHierarchy()

        setActive(newTermVC)
        newTermVC.startIfNeeded()
        // Lets TabViewController fan runtime state (window-level background) onto the new pane.
        onPaneAdded?(newTermVC)
        // Target `focusTarget`: alacritty panes wrap the render view in a container that does
        // not accept first-responder.
        newTermVC.view.window?.makeFirstResponder(newTermVC.focusTarget)
    }

    /// Factory for the terminal renderer (alacritty is the only backend).
    static func makeTerminalPanel(
        config: CopadConfig,
        theme: CopadTheme,
        cwd: String? = nil,
        initialInput: String? = nil,
    ) -> any CopadPanel {
        AlacrittyTerminalViewController(config: config, theme: theme, cwd: cwd, initialInput: initialInput)
    }

    /// `background: true` adds the pane without focusing it, as an agent asks for.
    func splitActiveWithWebView(
        url: URL? = nil,
        orientation: SplitOrientation = .horizontal,
        background: Bool = false,
    ) {
        let previouslyActive = activePane
        // Save the responder, not just the pane: refocusing the pane would evict a field editor
        // and lose the caret if the user was mid-edit in the URL bar.
        let previousWindow = activePane.view.window
        let previousResponder = previousWindow?.firstResponder
        let webVC = WebViewController(url: url)
        assignEventBus(to: webVC)
        wirePanel(webVC)

        root = root.splitting(activePane, with: .leaf(webVC), orientation: orientation)

        rebuildViewHierarchy()

        setActive(webVC)
        webVC.startIfNeeded()
        onPaneAdded?(webVC)
        if background {
            // The pane is still started so it loads; only the focus move is skipped.
            setActive(previouslyActive)
            if let previousResponder, let previousWindow {
                previousWindow.makeFirstResponder(previousResponder)
            } else {
                previouslyActive.view.window?.makeFirstResponder(previouslyActive.focusTarget)
            }
        } else {
            webVC.view.window?.makeFirstResponder(webVC.focusTarget)
        }
    }

    /// Split with a pre-built plugin panel, so PaneManager need not reach into AppDelegate state.
    func splitActiveWithPluginPanel(_ panel: any CopadPanel, orientation: SplitOrientation = .horizontal) {
        assignEventBus(to: panel)
        wirePanel(panel)

        root = root.splitting(activePane, with: .leaf(panel), orientation: orientation)

        rebuildViewHierarchy()

        setActive(panel)
        panel.startIfNeeded()
        onPaneAdded?(panel)
        panel.view.window?.makeFirstResponder(panel.focusTarget)
    }

    func closeActive() {
        closePanel(activePane)
    }

    /// Close a specific pane (Cmd+W, or `close_on_exit` in any pane). Closing the last leaf fires
    /// `onLastPaneClosed`. Only an active-pane close moves focus to a sibling.
    func closePanel(_ closing: any CopadPanel) {
        let closingActive = ObjectIdentifier(closing as AnyObject) == ObjectIdentifier(activePane as AnyObject)
        guard let newRoot = root.removing(closing) else {
            closing.view.removeFromSuperview()
            closing.removeFromParent()
            onLastPaneClosed?()
            return
        }

        root = newRoot
        closing.view.removeFromSuperview()
        closing.removeFromParent()
        rebuildViewHierarchy()

        if closingActive {
            let next = root.allLeaves().first!
            setActive(next)
            next.view.window?.makeFirstResponder(next.focusTarget)
        } else {
            activePane.view.window?.makeFirstResponder(activePane.focusTarget)
        }
        // The tree changed shape even if the active pane did not move; the save is debounced, so
        // a duplicate notification is harmless.
        onLayoutChanged?()
    }

    func setActive(_ panel: any CopadPanel) {
        activePane = panel
        onActivePaneChanged?()
        eventBus?.broadcast(event: "panel.focused", data: ["panel_id": panel.panelID])
    }

    private func propagateEventBus() {
        allPanels().forEach { assignEventBus(to: $0) }
    }

    private func assignEventBus(to panel: any CopadPanel) {
        if let a = panel as? AlacrittyTerminalViewController { a.eventBus = eventBus }
        if let w = panel as? WebViewController {
            w.eventBus = eventBus
            w.captureBodies = config.browserCaptureBodies
        }
    }

    func allPanels() -> [any CopadPanel] {
        root.allLeaves()
    }

    /// Cycle the active pane forward (`+1`) or backward (`-1`) over the DFS leaf order, wrapping.
    /// Used by the Cmd+Shift+] / [ menu items.
    func focusNextPane(direction: Int = 1) {
        let leaves = root.allLeaves()
        guard leaves.count > 1 else { return }
        let currentIdx = leaves.firstIndex { ObjectIdentifier($0 as AnyObject) == ObjectIdentifier(activePane as AnyObject) }
        guard let idx = currentIdx else { return }
        let count = leaves.count
        let nextIdx = ((idx + direction) % count + count) % count
        let next = leaves[nextIdx]
        setActive(next)
        next.view.window?.makeFirstResponder(next.focusTarget)
    }

    /// All terminal panels in DFS leaf order.
    func allTerminals() -> [any TerminalCapable] {
        root.allLeaves().compactMap { $0 as? TerminalCapable }
    }

    /// Active terminal for socket `terminal.*` dispatch; nil for webview/plugin panes
    /// (`wrong_panel_type`).
    func activeTerminalPanel() -> (any TerminalCapable)? {
        activePane as? TerminalCapable
    }

    func activeWebView() -> WebViewController? {
        activePane as? WebViewController
    }

    func setCustomTitle(_ title: String) {
        (activePane as? TerminalCapable)?.setCustomTitle(title)
    }

    // MARK: - Session persistence

    /// Typed leaves plus divider ratios captured from the live split views, walking the model
    /// and NSSplitView trees together so `ratio` reflects real sizes.
    func snapshotTree() -> Session.SplitSnap? {
        buildSnapV3(node: root, view: containerView.subviews.first)
    }

    private func buildSnapV3(node: SplitNode, view: NSView?) -> Session.SplitSnap? {
        switch node {
        case let .leaf(panel):
            return .leaf(content: paneContent(panel))
        case let .branch(orientation, children):
            let wire: Session.SplitOrientation = (orientation == .horizontal) ? .horizontal : .vertical
            let sv = view as? NSSplitView
            // A `.horizontal` SplitNode is a vertical NSSplitView (side-by-side), so its axis is
            // width; `.vertical` stacks top/bottom.
            let axisIsWidth = (orientation == .horizontal)
            var pairs: [(Session.SplitSnap, CGFloat)] = []
            for (i, child) in children.enumerated() {
                let childView = (sv?.subviews.indices.contains(i) == true) ? sv?.subviews[i] : nil
                guard let snap = buildSnapV3(node: child, view: childView) else { continue }
                let size = childView.map { axisIsWidth ? $0.frame.width : $0.frame.height } ?? 0
                pairs.append((snap, size))
            }
            return Self.foldBinary(orientation: wire, pairs: pairs)
        }
    }

    private func paneContent(_ panel: any CopadPanel) -> Session.PaneContent {
        if let t = panel as? AlacrittyTerminalViewController {
            // An unavailable-plugin placeholder re-captures as its plugin (decision #61 C5).
            if let pluginRef = t.unavailablePluginRef {
                return .plugin(name: pluginRef, panelName: nil, version: nil)
            }
            return .terminal(cwd: t.currentCwd)
        }
        if let w = panel as? WebViewController {
            let policy = config.browserRestore
            // One source for both fields so the pane `url` and its tab `url` agree.
            // `snapshotSourceURL`, not `currentURL`: a pane still loading has no live URL, and
            // autosave would erase it.
            return .webview(
                url: BrowserFFI.canonicalize(w.snapshotSourceURL, policy: policy).url,
                pane: w.snapshotPane(policy: policy),
            )
        }
        if let pl = panel as? PluginPanelController {
            return .plugin(name: pl.pluginName, panelName: pl.currentPanelName, version: nil)
        }
        if panel is CockpitViewController {
            return .cockpit
        }
        // Unknown panel type: persist as a fresh terminal.
        return .terminal(cwd: nil)
    }

    /// Divider-ratio clamp band (mirrors `copad_core::session::clamp_ratio`).
    static let minRatio: Float = 0.05
    static let maxRatio: Float = 0.95
    static func clampRatio(_ r: Float) -> Float {
        guard r.isFinite else { return 0.5 }
        return min(max(r, minRatio), maxRatio)
    }

    /// Fold an n-ary branch into a right-leaning binary tree, computing each divider's ratio from
    /// the cumulative live sizes along the split axis; 0.5 when sizes are unknown.
    private static func foldBinary(
        orientation: Session.SplitOrientation,
        pairs: [(Session.SplitSnap, CGFloat)],
    ) -> Session.SplitSnap? {
        guard let head = pairs.first else { return nil }
        if pairs.count == 1 { return head.0 }
        let rest = Array(pairs.dropFirst())
        guard let restSnap = foldBinary(orientation: orientation, pairs: rest) else { return head.0 }
        let restSize = rest.reduce(0) { $0 + $1.1 }
        let total = head.1 + restSize
        let ratio: Float = total > 0 ? clampRatio(Float(head.1 / total)) : 0.5
        return .branch(orientation: orientation, ratio: ratio, first: head.0, second: restSnap)
    }

    /// Factory for reopening a plugin panel on restore, injected because construction needs the
    /// manifest store and action registry PaneManager cannot reach. `panelName` selects one panel
    /// of a multi-panel plugin. Nil (plugin uninstalled) gives a placeholder.
    var pluginFactory: ((_ name: String, _ panelName: String?) -> (any CopadPanel)?)?

    /// Factory for reopening the agent cockpit pane on restore; nil gives a placeholder terminal.
    var cockpitFactory: (() -> (any CopadPanel)?)?

    static func makePanel(
        from content: Session.PaneContent,
        config: CopadConfig,
        theme: CopadTheme,
        pluginFactory: ((_ name: String, _ panelName: String?) -> (any CopadPanel)?)? = nil,
        cockpitFactory: (() -> (any CopadPanel)?)? = nil,
    ) -> any CopadPanel {
        switch content {
        case let .terminal(cwd):
            return AlacrittyTerminalViewController(config: config, theme: theme, cwd: cwd, initialInput: nil)
        case let .webview(url, pane):
            // An untrusted snapshot goes through the core normalizer; if the rule cannot be
            // consulted the pane falls back to single-URL restore.
            let checked = pane.flatMap { BrowserFFI.normalize($0) }
            return WebViewController(url: URL(string: url), pane: checked)
        case let .plugin(name, panelName, _):
            if let factory = pluginFactory, let panel = factory(name, panelName) {
                return panel
            }
            // Plugin uninstalled or no factory: a placeholder terminal tagged with the plugin ref,
            // so re-saves keep the ref until the plugin returns (decision #61 C5). The name is not
            // interpolated into shell input.
            let placeholder = AlacrittyTerminalViewController(config: config, theme: theme)
            placeholder.unavailablePluginRef = name
            return placeholder
        case .cockpit:
            if let factory = cockpitFactory, let panel = factory() { return panel }
            // No cockpit model: a placeholder terminal keeps the layout.
            return AlacrittyTerminalViewController(config: config, theme: theme)
        }
    }

    /// Content of the leftmost (DFS pre-order) leaf, which seeds a restored tab or subtree.
    static func leftmostContent(_ snap: Session.SplitSnap) -> Session.PaneContent {
        switch snap {
        case let .leaf(content): return content
        case let .branch(_, _, first, _): return leftmostContent(first)
        }
    }


    /// First non-nil custom title in DFS order, mirroring Linux; nil falls back to the live title.
    func customTabTitle() -> String? {
        for panel in allPanels() {
            if let t = panel as? TerminalCapable, let title = t.customTitle {
                return title
            }
        }
        return nil
    }

    /// Replay a saved snap onto an existing leaf: each Branch becomes one new typed panel (seeded
    /// from the leftmost content of `second`) via `SplitNode.splitting`. Leaf snaps are no-ops.
    /// Ratios are applied later by `applyPendingRatios`.
    func restoreTree(into target: any CopadPanel, from snap: Session.SplitSnap) {
        guard case let .branch(orientation, _, first, second) = snap else { return }
        let newPanel = Self.makePanel(
            from: Self.leftmostContent(second),
            config: config,
            theme: theme,
            pluginFactory: pluginFactory,
            cockpitFactory: cockpitFactory,
        )
        assignEventBus(to: newPanel)
        wirePanel(newPanel)
        let oriented: SplitOrientation = (orientation == .horizontal) ? .horizontal : .vertical
        root = root.splitting(target, with: .leaf(newPanel), orientation: oriented)
        rebuildViewHierarchy()
        newPanel.startIfNeeded()
        restoreTree(into: target, from: first)
        restoreTree(into: newPanel, from: second)
    }

    // MARK: - Divider-ratio restore

    /// The persisted layout, held until the split views have a non-zero size (they are zero until
    /// the tab is displayed).
    private var pendingRatioLayout: Session.SplitSnap?

    func setPendingRatioLayout(_ snap: Session.SplitSnap) {
        pendingRatioLayout = snap
        scheduleApplyRatios()
    }

    /// Apply persisted divider ratios once the split views are sized; otherwise leave the layout
    /// pending. Also called from `TabViewController.switchTab` when an off-screen tab is first
    /// shown.
    func applyPendingRatios() {
        guard let snap = pendingRatioLayout, let rootView = containerView.subviews.first else { return }
        containerView.layoutSubtreeIfNeeded()
        guard rootView.bounds.width > 1, rootView.bounds.height > 1 else { return }
        Self.applyRatios(snap: snap, view: rootView)
        pendingRatioLayout = nil
    }

    private func scheduleApplyRatios(attempt: Int = 0) {
        guard pendingRatioLayout != nil else { return }
        DispatchQueue.main.async { [weak self] in
            guard let self, pendingRatioLayout != nil else { return }
            applyPendingRatios()
            // Bounded retry while the active tab lays out; an off-screen tab is handled by
            // `switchTab`.
            if pendingRatioLayout != nil, attempt < 20 {
                scheduleApplyRatios(attempt: attempt + 1)
            }
        }
    }

    /// Walk the persisted snap alongside the live split views, setting each divider's ratio. The
    /// restored tree is binary, so `first` is subview 0 and `second` is subview 1, as in
    /// `restoreTree`.
    private static func applyRatios(snap: Session.SplitSnap, view: NSView) {
        guard case let .branch(_, ratio, first, second) = snap,
              let sv = view as? NSSplitView, sv.subviews.count >= 2 else { return }
        let axis = sv.isVertical ? sv.bounds.width : sv.bounds.height
        if axis > 1 {
            sv.setPosition(CGFloat(ratio) * axis, ofDividerAt: 0)
            sv.layoutSubtreeIfNeeded()
        }
        applyRatios(snap: first, view: sv.subviews[0])
        applyRatios(snap: second, view: sv.subviews[1])
    }

    /// Flip default-bg cells transparent in every pane's renderer; the image and tint are drawn
    /// at the window level by TabViewController. Kept for new-tab inheritance.
    func applyBackground(path: String, tint: Double, opacity: Double) {
        allPanels().forEach { $0.applyBackground(path: path, tint: tint, opacity: opacity) }
    }

    func clearBackground() {
        allPanels().forEach { $0.clearBackground() }
    }

    func setTint(_ alpha: Double) {
        allPanels().forEach { $0.setTint(alpha) }
    }

    /// The live `window.opacity` for TabViewController's window-level background alpha.
    func configWindowOpacity() -> Double {
        config.windowOpacity
    }

    /// Single hot-reload entry: snapshot the config/theme for future splits, then fan out to live
    /// alacritty terminals. These hooks are alacritty-specific, not on `TerminalCapable`.
    func applyConfig(_ newConfig: CopadConfig, theme newTheme: CopadTheme) {
        let oldKitty = config.kittyKeyboard
        let oldScrollback = config.scrollback
        let oldImages = config.images
        config = newConfig
        theme = newTheme
        for pane in root.allLeaves() {
            if let alac = pane as? AlacrittyTerminalViewController {
                alac.applyTheme(newTheme)
                alac.applyFont(family: newConfig.fontFamily, baseSize: CGFloat(newConfig.fontSize))
                alac.applyFontThicken(newConfig.fontThicken)
                alac.applyAlphaBlending(newConfig.alphaBlending)
                alac.applyOSC52Policy(newConfig.osc52)
                if newConfig.kittyKeyboard != oldKitty {
                    alac.applyKittyKeyboard(newConfig.kittyKeyboard)
                }
                if newConfig.scrollback != oldScrollback {
                    alac.applyScrollback(newConfig.scrollback)
                }
                if newConfig.images != oldImages {
                    alac.applyImages(newConfig.images)
                }
                alac.applyWindowOpacity(newConfig.windowOpacity)
            }
        }
    }

    // MARK: - View Hierarchy

    /// Rebuilds the view hierarchy from the SplitNode tree with fresh EqualSplitViews on every
    /// split/close.
    private func rebuildViewHierarchy() {
        NSLayoutConstraint.deactivate(rootConstraints)
        rootConstraints = []
        containerView.subviews.forEach { $0.removeFromSuperview() }

        let rootView = buildView(from: root)
        rootView.translatesAutoresizingMaskIntoConstraints = false
        containerView.addSubview(rootView)

        let constraints = [
            rootView.topAnchor.constraint(equalTo: containerView.topAnchor),
            rootView.leadingAnchor.constraint(equalTo: containerView.leadingAnchor),
            rootView.trailingAnchor.constraint(equalTo: containerView.trailingAnchor),
            rootView.bottomAnchor.constraint(equalTo: containerView.bottomAnchor),
        ]
        NSLayoutConstraint.activate(constraints)
        rootConstraints = constraints
    }

    /// NSSplitView sizes its subviews, so direct children keep
    /// `translatesAutoresizingMaskIntoConstraints = true`.
    private func buildView(from node: SplitNode) -> NSView {
        switch node {
        case let .leaf(panel):
            panel.view.translatesAutoresizingMaskIntoConstraints = true
            panel.view.autoresizingMask = [.width, .height]
            return panel.view

        case let .branch(orientation, children):
            let sv = EqualSplitView()
            sv.isVertical = (orientation == .horizontal)
            sv.dividerStyle = .thin
            for child in children {
                sv.addSubview(buildView(from: child))
            }
            return sv
        }
    }

    // MARK: - Focus Monitor

    private func installClickMonitor() {
        clickMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { [weak self] event in
            guard let self else { return event }
            let leaves = root.allLeaves()
            guard leaves.count > 1 else { return event }
            for panel in leaves {
                let view = panel.view
                let locationInView = view.convert(event.locationInWindow, from: nil)
                if view.bounds.contains(locationInView) {
                    setActive(panel)
                    break
                }
            }
            return event
        }
    }

    // MARK: - Panel Wiring

    /// Wires alacritty's child-exit close cascade, gated on `[terminal] close_on_exit`, mirroring
    /// `copad-linux/src/tabs.rs::handle_panel_exit`. The gate is read at fire time so a hot-reload
    /// applies to already-open panes. Webview and plugin panels have no PTY child.
    private func wirePanel(_ panel: any CopadPanel) {
        guard let alac = panel as? AlacrittyTerminalViewController else { return }
        alac.onChildExited = { [weak self] panel in
            guard let self, config.closeOnExit else { return }
            closePanel(panel)
        }
    }
}
