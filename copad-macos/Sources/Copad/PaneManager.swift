import AppKit

/// NSSplitView subclass that distributes all subviews equally on the first resize pass.
/// Works for any number of subviews (N panes → each gets 1/N of available space).
/// After the initial layout the user can freely drag dividers to any position.
///
/// Using NSSplitViewDelegate.splitView(_:resizeSubviewsWithOldSize:) rather than
/// layout() because NSSplitView sets subview frames via resizeSubviews, which runs
/// *before* layout(). By the time layout() fires, the (wrong) frames are already
/// committed. The delegate method intercepts at exactly the right moment.
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
            // After initial sizing: let NSSplitView handle normal proportional resize.
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
    /// PR 8 — terminal seeded with a specific cwd and/or initial PTY
    /// input. Used by `claude.start` to land the user in a worktree
    /// directory and feed the `tmux new-session` command. Separate
    /// case (rather than associated values on `.terminal`) so existing
    /// `.terminal` callers stay unchanged.
    case terminalSeed(cwd: String?, initialInput: String?)
    case webview(url: URL?)
    /// Tier 4.1 — pre-constructed plugin panel. Caller (TabViewController)
    /// builds the PluginPanelController itself because it needs the registry
    /// + event bus references; PaneManager just embeds it.
    case pluginPanel(any CopadPanel)
}

/// Manages the split-pane tree for a single tab.
/// TabViewController embeds `containerView` once; PaneManager rebuilds its
/// contents on every split/close using fresh NSSplitView instances.
@MainActor
final class PaneManager {
    /// Mutable so split-spawned panes after a config hot-reload pick up the new values
    /// (theme/font/security). `applyTheme` / `applyFont` / `applyOSC52Policy` already
    /// fan out to existing panes; updating the snapshot here keeps new splits in step.
    private var config: CopadConfig
    private var theme: CopadTheme

    private(set) var root: SplitNode
    private(set) var activePane: any CopadPanel

    /// Stable container — TabViewController pins this to contentArea once and never re-embeds.
    let containerView: NSView

    var onLastPaneClosed: (() -> Void)?
    /// Fires after any new pane is added to this tab (split, webview
    /// split, plugin split). TabViewController uses it to fan
    /// runtime-applied state (window-level background, …) onto the
    /// new pane — the pane was constructed from `self.config`, which
    /// only carries load-time bg state, so without this hook a split
    /// after a `background.set` socket call would render the new
    /// pane opaque and cover the wallpaper.
    var onPaneAdded: ((any CopadPanel) -> Void)?
    var onActivePaneChanged: (() -> Void)?
    /// Fires whenever the split tree changes shape (a pane closes) even if the
    /// active pane is unchanged — e.g. a background terminal exiting via
    /// `close_on_exit`. TabViewController uses it to schedule a session save so
    /// non-active-pane closes aren't lost on crash (decision #61 C4).
    var onLayoutChanged: (() -> Void)?

    /// Propagated from AppDelegate so all panels can emit events.
    weak var eventBus: EventBus? {
        didSet { propagateEventBus() }
    }

    private nonisolated(unsafe) var clickMonitor: Any?
    /// Tracks the fill constraints added to containerView so they can be
    /// deactivated before the next rebuild.
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
        // Notify TabViewController so it can fan runtime state
        // (window-level background, …) onto the new pane.
        onPaneAdded?(newTermVC)
        // Target the panel's `focusTarget` (the inner keyboard view)
        // rather than `view` — alacritty panes wrap the render view in
        // a layout container that doesn't accept first-responder, so
        // targeting `view` silently fails.
        newTermVC.view.window?.makeFirstResponder(newTermVC.focusTarget)
    }

    /// Factory: construct the terminal renderer. Phase 10b removed
    /// the SwiftTerm fallback — alacritty is the only macOS backend
    /// now. Stale `[renderer] backend = "swiftterm"` config keys are
    /// parsed but ignored (see `RendererSection.backend`).
    static func makeTerminalPanel(
        config: CopadConfig,
        theme: CopadTheme,
        cwd: String? = nil,
        initialInput: String? = nil,
    ) -> any CopadPanel {
        AlacrittyTerminalViewController(config: config, theme: theme, cwd: cwd, initialInput: initialInput)
    }

    func splitActiveWithWebView(url: URL? = nil, orientation: SplitOrientation = .horizontal) {
        let webVC = WebViewController(url: url)
        assignEventBus(to: webVC)
        wirePanel(webVC)

        root = root.splitting(activePane, with: .leaf(webVC), orientation: orientation)

        rebuildViewHierarchy()

        setActive(webVC)
        webVC.startIfNeeded()
        onPaneAdded?(webVC)
        webVC.view.window?.makeFirstResponder(webVC.focusTarget)
    }

    /// Tier 4.1 — split with a pre-built plugin panel. Caller assembles the
    /// PluginPanelController (registry + eventBus deps) and hands us the
    /// CopadPanel to embed; PaneManager doesn't reach into AppDelegate state.
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

    /// Close a specific pane (Cmd+W via `closeActive`, or auto-close
    /// from `[terminal] close_on_exit` when a shell terminates in any
    /// pane — not necessarily the active one). When the closed pane
    /// was the last leaf, fires `onLastPaneClosed` so TabViewController
    /// can drop the tab (and the window if it was the last tab).
    /// When a non-active pane closes, the active pane keeps focus —
    /// only an active-pane close transfers focus to a sibling.
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
        // The tree changed shape regardless of whether the active pane moved,
        // so persist it (an active close already notified via setActive, but a
        // duplicate is harmless — the save is debounced).
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
        if let w = panel as? WebViewController { w.eventBus = eventBus }
    }

    func allPanels() -> [any CopadPanel] {
        root.allLeaves()
    }

    /// Tier 1.1 — pane focus navigation. Cycle the active pane forward (`+1`)
    /// or backward (`-1`) over the DFS order of leaves under `root`. Wraps
    /// at both ends. No-op when the tab has only one pane. Used by the
    /// Cmd+Shift+] / Cmd+Shift+[ menu items in `AppDelegate`.
    func focusNextPane(direction: Int = 1) {
        let leaves = root.allLeaves()
        guard leaves.count > 1 else { return }
        let currentIdx = leaves.firstIndex { ObjectIdentifier($0 as AnyObject) == ObjectIdentifier(activePane as AnyObject) }
        guard let idx = currentIdx else { return }
        let count = leaves.count
        // Modulo handles both directions including negative wrap.
        let nextIdx = ((idx + direction) % count + count) % count
        let next = leaves[nextIdx]
        setActive(next)
        next.view.window?.makeFirstResponder(next.focusTarget)
    }

    /// All terminal panels in DFS leaf order. Returns the
    /// backend-agnostic `TerminalCapable` interface — alacritty is
    /// the only conformer today but the protocol keeps call sites
    /// free of backend identity.
    func allTerminals() -> [any TerminalCapable] {
        root.allLeaves().compactMap { $0 as? TerminalCapable }
    }

    /// Active terminal-typed accessor used by socket `terminal.*`
    /// dispatch. Returns nil if the focused pane is non-terminal
    /// (webview / plugin), which AppDelegate surfaces as
    /// `wrong_panel_type`.
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

    /// Build a wire snapshot of this tab's split tree. Returns nil
    /// when no terminal panel survived the walk (webview-only or
    /// plugin-only tabs are skipped to keep parity with Linux —
    /// `panel.as_terminal()` filter in `tabs.rs::snapshot_session`).
    /// Build a v3 wire snapshot of this tab's split tree: typed leaves
    /// (terminal cwd / webview url / plugin ref / cockpit) + normalized divider
    /// ratios captured from the LIVE split views. Walks the model tree and the
    /// parallel NSSplitView tree together so `ratio` reflects real sizes.
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
            // A `.horizontal` SplitNode renders as a vertical NSSplitView
            // (panes side-by-side), so its split axis is width; `.vertical`
            // stacks top/bottom, axis is height.
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
            // An unavailable-plugin placeholder re-captures as its plugin so the
            // reference isn't lost on re-save (decision #61 C5).
            if let pluginRef = t.unavailablePluginRef {
                return .plugin(name: pluginRef, panelName: nil, version: nil)
            }
            return .terminal(cwd: t.currentCwd)
        }
        if let w = panel as? WebViewController {
            return .webview(url: Self.canonicalWebviewURL(w.currentURL))
        }
        if let pl = panel as? PluginPanelController {
            return .plugin(name: pl.pluginName, panelName: pl.currentPanelName, version: nil)
        }
        if panel is CockpitViewController {
            return .cockpit
        }
        // Unknown panel type — persist as a fresh terminal (harmless on restore).
        return .terminal(cwd: nil)
    }

    /// Divider-ratio clamp band (mirrors `copad_core::session::clamp_ratio`).
    static let minRatio: Float = 0.05
    static let maxRatio: Float = 0.95
    static func clampRatio(_ r: Float) -> Float {
        guard r.isFinite else { return 0.5 }
        return min(max(r, minRatio), maxRatio)
    }

    /// Fold an n-ary branch (with each child's live size along the split axis)
    /// into a right-leaning binary tree, computing each divider's real ratio
    /// from the cumulative sizes. Falls back to 0.5 when sizes are unknown
    /// (tab never displayed).
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

    /// Factory for reopening a plugin panel on restore. Injected by
    /// TabViewController (which gets it from AppDelegate's registry) because
    /// plugin construction needs the manifest store + action registry PaneManager
    /// can't reach. `panelName` selects the specific panel of a multi-panel
    /// plugin. Returns nil when the plugin is uninstalled → placeholder.
    var pluginFactory: ((_ name: String, _ panelName: String?) -> (any CopadPanel)?)?

    /// Factory for reopening the agent cockpit pane on restore (a `.cockpit`
    /// leaf). Injected by AppDelegate, which owns the cockpit model. nil →
    /// placeholder terminal.
    var cockpitFactory: (() -> (any CopadPanel)?)?

    /// Build a live panel from persisted v3 pane content. Used to seed a
    /// restored tab's first pane and each split leaf.
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
        case let .webview(url):
            return WebViewController(url: URL(string: url))
        case let .plugin(name, panelName, _):
            if let factory = pluginFactory, let panel = factory(name, panelName) {
                return panel
            }
            // Plugin uninstalled / no factory: a placeholder terminal tagged with
            // the plugin ref so the next snapshot re-captures it AS the plugin
            // (the ref survives re-saves until the plugin returns — decision #61
            // C5). The name is NOT interpolated into shell input.
            let placeholder = AlacrittyTerminalViewController(config: config, theme: theme)
            placeholder.unavailablePluginRef = name
            return placeholder
        case .cockpit:
            if let factory = cockpitFactory, let panel = factory() { return panel }
            // Cockpit model unavailable → placeholder terminal keeps the layout.
            return AlacrittyTerminalViewController(config: config, theme: theme)
        }
    }

    /// Content of the leftmost (DFS pre-order) leaf — seeds the panel that opens
    /// a restored tab / split subtree.
    static func leftmostContent(_ snap: Session.SplitSnap) -> Session.PaneContent {
        switch snap {
        case let .leaf(content): return content
        case let .branch(_, _, first, _): return leftmostContent(first)
        }
    }

    /// Decision #61 C6: reduce a live webview URL to a safe canonical form for
    /// persistence — the **origin only** (`scheme://host[:port]`), http(s) only.
    /// Path, query, fragment, and userinfo are ALL dropped, because every one of
    /// them can carry credentials or one-time tokens (`user:pass@`, `?code=…`,
    /// `#access_token=…`, `/reset/<token>`, `/oauth/callback/<code>`). Nothing
    /// beyond the origin reaches disk. Restore reopens the site root — the safe
    /// default absent a per-URL user approval. Non-http(s) / hostless / unparseable
    /// persist as "" and restore to the blank URL-entry placeholder.
    static func canonicalWebviewURL(_ raw: String) -> String {
        // Parse first, then compare a lowercased scheme — URL schemes are
        // case-insensitive, so `HTTPS://…` is valid and must not be dropped.
        guard let comps = URLComponents(string: raw),
              let scheme = comps.scheme?.lowercased(),
              scheme == "http" || scheme == "https",
              let host = comps.host, !host.isEmpty
        else { return "" }
        var origin = URLComponents()
        origin.scheme = scheme
        origin.host = host
        origin.port = comps.port
        return origin.string ?? ""
    }

    /// First non-nil custom title in DFS order. Mirrors Linux's
    /// per-tab custom-title lookup (collect panels, find_map). Returns
    /// nil if no panel has a custom title — restored tab falls back to
    /// the live title (cwd / process name) on reopen.
    func customTabTitle() -> String? {
        for panel in allPanels() {
            if let t = panel as? TerminalCapable, let title = t.customTitle {
                return title
            }
        }
        return nil
    }

    /// Replay a saved v3 snap onto an existing leaf panel. Walks the snap
    /// depth-first: every Branch turns into one new typed panel (seeded from the
    /// leftmost content of `second`), pushed into the live tree via the same
    /// `SplitNode.splitting` the interactive split path uses. Leaf snaps are
    /// no-ops — the target leaf already represents that cell. Divider ratios are
    /// applied separately by `applyPendingRatios` once the views are sized.
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

    /// The persisted layout for this tab, held after restore so divider ratios
    /// can be applied once the split views have a real (non-zero) size — they're
    /// zero until the tab is displayed. Applied + cleared by `applyPendingRatios`.
    private var pendingRatioLayout: Session.SplitSnap?

    /// Record the restored layout and start trying to apply its divider ratios.
    func setPendingRatioLayout(_ snap: Session.SplitSnap) {
        pendingRatioLayout = snap
        scheduleApplyRatios()
    }

    /// Apply persisted divider ratios to the live split views if they are sized;
    /// no-op (leaves the layout pending) while the tab is still zero-sized.
    /// Called from `setPendingRatioLayout` (active tab) and `TabViewController.
    /// switchTab` (when an off-screen tab is first shown).
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
            // Retry a bounded number of times while the active tab lays out; if
            // still pending (off-screen tab), switchTab applies it on display.
            if pendingRatioLayout != nil, attempt < 20 {
                scheduleApplyRatios(attempt: attempt + 1)
            }
        }
    }

    /// Walk the persisted snap alongside the live split-view tree, setting each
    /// divider to its persisted ratio. The restored tree is binary (each restore
    /// step creates a 2-child branch), so `first` ↔ subview 0 and `second` ↔
    /// subview 1, matching `restoreTree`.
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

    /// Fan a background-applied notification to every pane's render
    /// view so default-bg cells flip transparent. Image + tint are
    /// now drawn at the window level (TabViewController owns the
    /// NSImageView / tint overlay) — these per-pane calls just
    /// update renderer state. Kept for new-tab inheritance.
    func applyBackground(path: String, tint: Double, opacity: Double) {
        allPanels().forEach { $0.applyBackground(path: path, tint: tint, opacity: opacity) }
    }

    func clearBackground() {
        allPanels().forEach { $0.clearBackground() }
    }

    func setTint(_ alpha: Double) {
        allPanels().forEach { $0.setTint(alpha) }
    }

    /// Expose the live `window.opacity` for TabViewController's
    /// window-level bg / tint alpha calc. Held on the per-pane config
    /// snapshot; PaneManager.applyConfig keeps it fresh.
    func configWindowOpacity() -> Double {
        config.windowOpacity
    }

    /// Single hot-reload entry: snapshot the new config/theme so
    /// split-spawned panes pick them up, then fan out to live alacritty
    /// terminals. Methods called here are alacritty-specific (not on
    /// the `TerminalCapable` protocol — the protocol covers the
    /// socket-facing surface, not internal config hooks).
    func applyConfig(_ newConfig: CopadConfig, theme newTheme: CopadTheme) {
        config = newConfig
        theme = newTheme
        for pane in root.allLeaves() {
            if let alac = pane as? AlacrittyTerminalViewController {
                alac.applyTheme(newTheme)
                alac.applyFont(family: newConfig.fontFamily, baseSize: CGFloat(newConfig.fontSize))
                alac.applyOSC52Policy(newConfig.osc52)
                alac.applyWindowOpacity(newConfig.windowOpacity)
            }
        }
    }

    // MARK: - View Hierarchy

    /// Rebuilds the entire view hierarchy from the SplitNode tree.
    /// This is called on every split/close, creating fresh EqualSplitViews each time.
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

    /// Recursively builds the view tree. NSSplitView manages subview sizing,
    /// so direct children use translatesAutoresizingMaskIntoConstraints = true.
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

    /// Wire lifecycle callbacks on a newly-created panel. Today the
    /// only hook is alacritty's child-exit close cascade, gated on
    /// `[terminal] close_on_exit` — when the user's shell exits
    /// (Ctrl+D, `exit`, killed parent), we close the owning pane;
    /// PaneManager's `closePanel` cascades up to TabViewController's
    /// `onLastPaneClosed` if it was the last leaf, mirroring Linux's
    /// `tab.close_panel` → `notebook.remove_page` chain in
    /// `copad-linux/src/tabs.rs::handle_panel_exit`. Webview /
    /// plugin panels are no-ops here (no PTY child).
    ///
    /// The gate is checked at fire time (not at wiring time) so a
    /// hot-reload of `close_on_exit` applies to already-open panes —
    /// matches Linux's live read of `self.config.borrow().terminal.
    /// close_on_exit` inside `handle_panel_exit`.
    private func wirePanel(_ panel: any CopadPanel) {
        guard let alac = panel as? AlacrittyTerminalViewController else { return }
        alac.onChildExited = { [weak self] panel in
            guard let self, config.closeOnExit else { return }
            closePanel(panel)
        }
    }
}
