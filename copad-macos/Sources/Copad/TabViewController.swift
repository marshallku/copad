import AppKit
import Combine
import CopadCore
import SwiftUI

/// Manages multiple tabs, each backed by a PaneManager (split-pane tree).
/// Panels can be terminals or webviews.
@MainActor
final class TabViewController: NSViewController {
    /// Mutable so config hot-reload affects panes spawned AFTER the reload (theme/font/security).
    /// Existing panes are updated separately via `applyConfig` fan-out.
    private var config: CopadConfig
    private(set) var theme: CopadTheme

    private var tabBar: TabBarView!
    /// The tab bar's primary size constraint (height for horizontal bars, width for vertical),
    /// held so collapse/expand can shrink a vertical bar to icon-only width.
    private var tabBarSizeConstraint: NSLayoutConstraint?

    /// Debounced session persistence (decision #61 C4). Coalesces structural mutations into one
    /// atomic write so a crash loses at most the debounce window. This VC is the single writer;
    /// suppressed during restore so replay doesn't re-save.
    private var sessionSaveTimer: Timer?
    /// Removal tokens for the controller-scoped notification observers.
    private var globalObservers: [any NSObjectProtocol] = []
    private var suppressSessionSave = false

    /// Reopens a plugin panel by name on session restore (decision #61).
    /// Set by AppDelegate, which owns the manifest store + action registry that
    /// plugin construction needs. nil-returning name → unavailable placeholder.
    var pluginFactory: ((_ name: String, _ panelName: String?) -> (any CopadPanel)?)?
    private var contentArea: NSView!
    /// comux's sidebar / notification center, drawn natively while a visible pane of this
    /// window runs a comux client whose sidebar we claimed (decisions #169). Zero width otherwise.
    private var comuxSidebar: NSView!
    private var comuxNotifications: NSView!
    private var comuxSidebarWidth: NSLayoutConstraint!
    private var comuxNotificationsWidth: NSLayoutConstraint!
    private var comuxStatus: NSView!
    private var comuxStatusHeight: NSLayoutConstraint!
    /// The right dock: where the work-graph panel takes room from the terminal (decisions
    /// #204); zero wide while it is closed.
    private var comuxNotifSlot: NSView!
    private var comuxNotifSlotWidth: NSLayoutConstraint!
    /// The notification dropdown's height (it hangs under the top bar's bell).
    private var comuxNotificationsHeight: NSLayoutConstraint!
    /// The top bar over the terminal (decisions #204).
    private var comuxTopBar: NSView!
    private var comuxTopHeight: NSLayoutConstraint!
    /// The work-graph panel in the right dock, and the grip on its left edge.
    private var comuxGraph: ComuxGraphPanel!
    private weak var comuxGraphGrip: NSView?
    /// Closes the notification dropdown on Esc or a click outside it.
    private var comuxFloatMonitor: Any?
    /// The delegated overlay this window is showing as a sheet, if any.
    private var comuxOverlayShown: UInt64?
    /// The overlay layer over the content (decisions #180), and the monitor that keeps keys in it.
    private var comuxOverlayLayer: NSView?
    private var comuxOverlayMonitor: Any?
    /// The sidebar row preview card (decisions #181).
    private var comuxCardWidth: NSLayoutConstraint!
    private weak var comuxGrip: NSView?
    private var comuxCardObservation: AnyCancellable?
    private var comuxNotificationsOpen = false
    private var comuxObservation: AnyCancellable?
    private var comuxServeTimer: Timer?
    private var comuxServed: [String] = []
    /// What this window's comux chrome shows (the comux its visible panes host).
    private let comuxWindow = ComuxWindow()
    /// Window-level background image: one image fills the whole `contentArea` so splits share a
    /// wallpaper. Created lazily, inserted below the PaneManager containerView so panes with
    /// transparent default-bg cells overlay it. A layer-backed plain NSView with
    /// `contentsGravity = .resizeAspectFill` (CSS `cover`, like Linux's `ContentFit::Cover`):
    /// `NSImageView` has no fill-and-crop scaling mode.
    private var backgroundView: NSView?
    private var tintView: NSView?
    /// Monotonic load token: drops a slow decode that lands after a newer
    /// applyBackground/clearBackground.
    private var backgroundLoadToken: UInt64 = 0
    /// Status bar at the bottom of the window. nil when
    /// `[statusbar] enabled = false`. Public so AppDelegate can wire
    /// it up post-launch (load modules from discovered plugin manifests
    /// + handle statusbar.show/hide/toggle socket commands).
    private(set) var statusBar: StatusBarView?
    private var paneManagers: [PaneManager] = []
    private(set) var activeIndex: Int = -1

    /// Reopens the agent cockpit pane on restore (a `.cockpit` leaf). Injected by
    /// AppDelegate, which owns the cockpit model. nil → placeholder terminal.
    var cockpitFactory: (() -> (any CopadPanel)?)?

    /// Number of tabs, so the `cmd+shift+N` jump only swallows the key when that tab exists.
    var tabCount: Int { paneManagers.count }

    // Retained so new tabs inherit the current background state
    private(set) var currentBackgroundPath: String?
    private(set) var currentBackgroundTint: Double = 0.6
    private(set) var currentBackgroundOpacity: Double = 1.0
    // Whether `currentBackgroundPath` was picked from the wallpaper list rather than set
    // manually or via `[background] image`; `background.delete_current` only deletes a
    // list-picked image (Linux's `current.1` flag).
    private(set) var currentBackgroundFromList = false

    // Tab bar collapsed state. Default collapsed (icon-only); auto-expands on 1->2 tabs unless
    // the user toggled the bar manually.
    private var isBarCollapsed: Bool = true
    private var userToggledBar: Bool = false

    /// Set by AppDelegate; propagated to all PaneManagers.
    weak var eventBus: EventBus? {
        didSet { paneManagers.forEach { $0.eventBus = eventBus } }
    }

    /// Invoked when the "+" popover's plugin-panel row is clicked. A callback so
    /// TabViewController stays free of `ActionRegistry` / `EventBus` dependencies; AppDelegate
    /// runs the same path as the `plugin.open` RPC. Called on the main actor; mode mirrors the
    /// RPC's `mode` ("tab" / "split_h" / "split_v").
    var onOpenPlugin: ((_ name: String, _ panelName: String, _ mode: AddPanelMode) -> Void)?

    var isTabBarCollapsed: Bool {
        isBarCollapsed
    }

    var activePaneManager: PaneManager? {
        paneManagers.indices.contains(activeIndex) ? paneManagers[activeIndex] : nil
    }

    var activeTerminalPanel: (any TerminalCapable)? {
        activePaneManager?.activeTerminalPanel()
    }

    var activeWebView: WebViewController? {
        activePaneManager?.activeWebView()
    }

    var activeZoomable: (any Zoomable)? {
        activePaneManager?.activePane as? Zoomable
    }

    /// Cross-tab panel lookup by stable UUID (Linux `find_panel_by_id`).
    func panel(id: String) -> (any CopadPanel)? {
        for manager in paneManagers {
            if let p = manager.allPanels().first(where: { $0.panelID == id }) {
                return p
            }
        }
        return nil
    }

    /// Per-pane snapshot for the agent cockpit, terminal panes only. macOS has no
    /// `terminal.cwd_changed` event, so cwd is pulled from `reportedCwd` at each refresh.
    func terminalPaneSnapshot() -> [(panelID: String, title: String, cwd: String, tabIndex: Int)] {
        var rows: [(panelID: String, title: String, cwd: String, tabIndex: Int)] = []
        for (i, manager) in paneManagers.enumerated() {
            for panel in manager.allPanels() {
                if let term = panel as? AlacrittyTerminalViewController {
                    rows.append((term.panelID, term.currentTitle, term.reportedCwd ?? "", i))
                }
            }
        }
        return rows
    }

    /// Focus a pane by id from anywhere (cockpit click): switch to its tab, make it active, and
    /// give it keyboard focus.
    @discardableResult
    func activatePanel(id: String) -> Bool {
        for (i, manager) in paneManagers.enumerated() {
            if let panel = manager.allPanels().first(where: { $0.panelID == id }) {
                if i != activeIndex { switchTab(to: i) }
                manager.setActive(panel)
                panel.view.window?.makeFirstResponder(panel.focusTarget)
                return true
            }
        }
        return false
    }

    /// First terminal panel across all tabs in DFS order (Linux `find_first_terminal`), the
    /// last-resort fallback of `resolveTerminalPanel`.
    func firstTerminalPanel() -> (any TerminalCapable)? {
        for manager in paneManagers {
            for panel in manager.allPanels() {
                if let term = panel as? TerminalCapable {
                    return term
                }
            }
        }
        return nil
    }

    /// Push a shell-reported cwd onto the matching terminal panel (the `panel.report_cwd`
    /// handler). False for an unknown id or non-terminal target.
    @discardableResult
    func applyReportedCwd(panelID: String, cwd: String) -> Bool {
        guard let p = panel(id: panelID) else { return false }
        if let a = p as? AlacrittyTerminalViewController {
            a.setReportedCwd(cwd)
            return true
        }
        return false
    }

    var activePaneID: String? {
        activePaneManager?.activePane.panelID
    }

    func webView(id: String) -> WebViewController? {
        panel(id: id) as? WebViewController
    }

    init(config: CopadConfig, theme: CopadTheme) {
        self.config = config
        self.theme = theme
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    // MARK: - View Lifecycle

    override func loadView() {
        let root = NSView()
        root.wantsLayer = true
        // With `[window] opacity < 1.0` leave the root layer bg clear so the translucent window
        // and blur view show through. Hot-reload is in `applyConfig`.
        root.layer?.backgroundColor = Self.rootBg(theme: theme, opacity: config.windowOpacity)

        tabBar = TabBarView(theme: theme, windowOpacity: config.windowOpacity, position: config.tabsPosition)
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        tabBar.onSelectTab = { [weak self] i in self?.switchTab(to: i) }
        tabBar.onCloseTab = { [weak self] i in self?.closeTabByButton(at: i) }
        tabBar.onToggle = { [weak self] in
            self?.toggleTabBar(userInitiated: true)
        }
        tabBar.onRenameTab = { [weak self] index, title in
            guard let self else { return }
            renameTab(at: index, title: title)  // schedules the save itself
            // Restore focus to the active pane's input view (`panel.focusTarget`) after the tab
            // bar field resigns.
            if let activePanel = activePaneManager?.activePane {
                view.window?.makeFirstResponder(activePanel.focusTarget)
            }
        }
        tabBar.onNewPanel = { [weak self] type, mode in
            guard let self else { return }
            switch type {
            case .terminal:
                switch mode {
                case .tab: newTab()
                case .splitH: splitActivePane(orientation: .horizontal)
                case .splitV: splitActivePane(orientation: .vertical)
                }
            case .webview:
                switch mode {
                case .tab: newWebViewTab()
                case .splitH: splitActivePaneWithWebView(orientation: .horizontal)
                case .splitV: splitActivePaneWithWebView(orientation: .vertical)
                }
            case let .plugin(name, panelName, _, _):
                // Construction and registry/eventBus wiring live in AppDelegate's
                // `openPluginPanel`, which mirrors the `plugin.open` handler; errors are logged
                // there, so this is fire-and-forget.
                onOpenPlugin?(name, panelName, mode)
            }
        }
        root.addSubview(tabBar)

        contentArea = NSView()
        contentArea.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(contentArea)
        installComuxChrome()

        // Status bar pinned to the top or bottom per `[statusbar] position`; tab bar and content
        // anchor against its inward edge. left/right is unsupported (the bar is a horizontal
        // NSStackView).
        var topEdge: NSLayoutYAxisAnchor = root.topAnchor
        var bottomEdge: NSLayoutYAxisAnchor = root.bottomAnchor
        let statusOnTop = config.statusBar.position.lowercased() == "top"
        if config.statusBar.enabled {
            let bar = StatusBarView(theme: theme, windowOpacity: config.windowOpacity)
            statusBar = bar
            root.addSubview(bar)
            var barConstraints: [NSLayoutConstraint] = [
                bar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                bar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
                bar.heightAnchor.constraint(equalToConstant: CGFloat(config.statusBar.height)),
            ]
            if statusOnTop {
                barConstraints.append(bar.topAnchor.constraint(equalTo: root.topAnchor))
                topEdge = bar.bottomAnchor
            } else {
                barConstraints.append(bar.bottomAnchor.constraint(equalTo: root.bottomAnchor))
                bottomEdge = bar.topAnchor
            }
            NSLayoutConstraint.activate(barConstraints)
        }

        // Tabs position. Horizontal (top/bottom): a full-width bar of fixed height, content
        // fills the rest. Vertical (left/right): a full-height bar of fixed width (`[tabs]
        // width`, icon-only when collapsed) at the leading/trailing edge.
        var constraints: [NSLayoutConstraint] = []
        if config.tabsPosition.isVertical {
            let barWidth = isBarCollapsed ? TabBarView.collapsedBarWidth : CGFloat(config.tabsWidth)
            let widthC = tabBar.widthAnchor.constraint(equalToConstant: barWidth)
            tabBarSizeConstraint = widthC
            constraints.append(contentsOf: [
                widthC,
                tabBar.topAnchor.constraint(equalTo: topEdge),
                tabBar.bottomAnchor.constraint(equalTo: bottomEdge),
                contentArea.topAnchor.constraint(equalTo: topEdge),
                contentArea.bottomAnchor.constraint(equalTo: bottomEdge),
            ])
            if config.tabsPosition == .left {
                constraints.append(contentsOf: [
                    tabBar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                    contentArea.leadingAnchor.constraint(equalTo: tabBar.trailingAnchor),
                    contentArea.trailingAnchor.constraint(equalTo: root.trailingAnchor),
                ])
            } else {
                constraints.append(contentsOf: [
                    tabBar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
                    contentArea.trailingAnchor.constraint(equalTo: tabBar.leadingAnchor),
                    contentArea.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                ])
            }
        } else {
            let heightC = tabBar.heightAnchor.constraint(equalToConstant: TabBarView.height)
            tabBarSizeConstraint = heightC
            constraints.append(contentsOf: [
                tabBar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                tabBar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
                heightC,
                contentArea.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                contentArea.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            ])
            switch config.tabsPosition {
            case .top:
                constraints.append(contentsOf: [
                    tabBar.topAnchor.constraint(equalTo: topEdge),
                    contentArea.topAnchor.constraint(equalTo: tabBar.bottomAnchor),
                    contentArea.bottomAnchor.constraint(equalTo: bottomEdge),
                ])
            case .bottom:
                constraints.append(contentsOf: [
                    contentArea.topAnchor.constraint(equalTo: topEdge),
                    contentArea.bottomAnchor.constraint(equalTo: tabBar.topAnchor),
                    tabBar.bottomAnchor.constraint(equalTo: bottomEdge),
                ])
            case .left, .right:
                break  // handled in the vertical branch above
            }
        }
        NSLayoutConstraint.activate(constraints)

        tabBar.setCollapsed(isBarCollapsed)

        view = root
    }

    override func viewDidLoad() {
        super.viewDidLoad()
    }

    func openInitialTab() {
        newTab()
    }

    // MARK: - Session persistence

    /// Wire snapshot of every live tab (v3, decision #64). Terminals carry only their cwd.
    func snapshotSession() -> Session.Snapshot {
        var tabs: [Session.TabSnap] = []
        for manager in paneManagers {
            guard let root = manager.snapshotTree() else { continue }
            tabs.append(Session.TabSnap(customTitle: manager.customTabTitle(), root: root))
        }
        let clamped = max(0, min(activeIndex, tabs.count - 1))
        return Session.Snapshot(version: Session.version, tabs: tabs, currentTab: clamped)
    }

    /// Reclaim history blobs no longer referenced by a COMMITTED snapshot. Call only after
    /// `Session.save` returns: against a set gathered before the write it would delete the blob
    /// the new session document points at (blob generations are immutable because these two
    /// writes are not atomic together).
    static func gcHistoryBlobs(after snap: Session.Snapshot) {
        let panes = snap.tabs.flatMap { browserPanes(in: $0.root) }
        BrowserFFI.gcHistory(live: BrowserSnapshot.liveGenerations(panes))
    }

    private static func browserPanes(in node: Session.SplitSnap) -> [BrowserPaneSnap] {
        switch node {
        case let .leaf(content):
            if case let .webview(_, pane) = content, let pane { return [pane] }
            return []
        case let .branch(_, _, first, second):
            return browserPanes(in: first) + browserPanes(in: second)
        }
    }

    /// Build tabs + splits to mirror `snap` (v3). Only cwd, layout and split ratios are replayed.
    /// The caller falls back to `openInitialTab` if `snap` has zero tabs.
    func restoreSession(_ snap: Session.Snapshot) {
        suppressSessionSave = true
        defer {
            suppressSessionSave = false
            scheduleSessionSave()
        }
        for tabSnap in snap.tabs {
            let initialPanel = PaneManager.makePanel(
                from: PaneManager.leftmostContent(tabSnap.root),
                config: config,
                theme: theme,
                pluginFactory: pluginFactory,
                cockpitFactory: cockpitFactory,
            )
            let manager = PaneManager(config: config, theme: theme, initialPanel: .pluginPanel(initialPanel))
            manager.pluginFactory = pluginFactory
            manager.cockpitFactory = cockpitFactory
            addTab(manager: manager)
            manager.restoreTree(into: manager.activePane, from: tabSnap.root)
            if let title = tabSnap.customTitle {
                manager.setCustomTitle(title)
            }
            // Apply persisted divider ratios once the tab's split views are sized.
            manager.setPendingRatioLayout(tabSnap.root)
        }
        let clamped = max(0, min(snap.currentTab, paneManagers.count - 1))
        if paneManagers.indices.contains(clamped) {
            switchTab(to: clamped)
        }
    }

    deinit {
        if let m = comuxFloatMonitor { NSEvent.removeMonitor(m) }
        if let m = comuxOverlayMonitor { NSEvent.removeMonitor(m) }
        for token in globalObservers {
            NotificationCenter.default.removeObserver(token)
        }
        comuxServeTimer?.invalidate()
        let me = ObjectIdentifier(self)
        Task { @MainActor in ComuxHost.shared.windowClosed(me) }
    }

    // MARK: - comux chrome (decisions #169)

    private func installComuxChrome() {
        let host = comuxWindow
        host.consumer = ComuxHost.consumer(ObjectIdentifier(self))
        let settle: () -> Void = { [weak self] in self?.settleComuxInput() }
        // The chrome never takes the keys from the terminal (it has no text field): a click on a
        // chip or a row acts and the pane keeps typing. Only `Ctrl-b e` hands the sidebar the keys.
        let sidebar = ChromeHostingView(rootView: ComuxSidebar(host: host, settle: settle))
        let topBar = ChromeHostingView(rootView: ComuxTopBar(host: host, settle: settle, toggleGraph: { [weak self] mode in
            self?.toggleComuxGraph(mode: mode)
        }, toggleNotifications: { [weak self] in
            guard let self else { return }
            self.comuxNotificationsOpen.toggle()
            self.refreshComuxChrome()
        }))
        let graph = ComuxGraphPanel(host: host)
        graph.refocusPane = { [weak self] in self?.refocusPane() }
        graph.modeChanged = { [weak self] mode in
            guard let self, self.comuxWindow.graphMode != mode else { return }
            self.comuxWindow.graphMode = mode
            UserDefaults.standard.set(mode, forKey: Self.comuxGraphModeKey)
        }
        let graphGrip = ComuxResizeGrip()
        graphGrip.direction = -1
        graphGrip.onDrag = { [weak self] width in
            UserDefaults.standard.set(width, forKey: Self.comuxGraphWidthKey)
            self?.layoutComuxChrome()
        }
        graphGrip.currentWidth = { [weak self] in self?.comuxGraph.frame.width ?? 0 }
        sidebar.takesKeys = { [weak host] in host?.sidebarFocus != nil }
        let notifications = ChromeHostingView(rootView: ComuxNotifications(host: host, settle: settle, close: { [weak self] in
            self?.closeComuxNotifications()
        }))
        let status = ChromeHostingView(rootView: ComuxStatusStrip(host: host, settle: settle))
        let slot = NSView()
        let card = PassThroughHostingView(rootView: ComuxHoverCard(host: host))
        // The sidebar's right edge resizes it (decisions #181).
        let grip = ComuxResizeGrip()
        grip.translatesAutoresizingMaskIntoConstraints = false
        grip.onDrag = { [weak self] width in
            UserDefaults.standard.set(width, forKey: Self.comuxSidebarWidthKey)
            self?.layoutComuxChrome()
        }
        grip.currentWidth = { [weak self] in self?.comuxSidebar.frame.width ?? 0 }
        grip.isHidden = true
        comuxGrip = grip
        comuxCardWidth = card.widthAnchor.constraint(equalToConstant: 0)
        card.isHidden = true
        // Only there while a row is previewed, so it never sits over the terminal otherwise.
        comuxCardObservation = host.$hoverCard.sink { [weak card] c in
            DispatchQueue.main.async { card?.isHidden = c == nil }
        }
        for v in [slot, graph, topBar, sidebar, notifications, status, card, grip, graphGrip] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            contentArea.addSubview(v)
        }
        comuxSidebar = sidebar
        comuxTopBar = topBar
        comuxGraph = graph
        comuxGraphGrip = graphGrip
        comuxWindow.graphOpen = UserDefaults.standard.bool(forKey: Self.comuxGraphOpenKey)
        comuxWindow.graphMode = UserDefaults.standard.string(forKey: Self.comuxGraphModeKey) ?? "agent"
        comuxNotifications = notifications
        comuxStatus = status
        comuxNotifSlot = slot
        comuxStatusHeight = status.heightAnchor.constraint(equalToConstant: 0)
        comuxTopHeight = topBar.heightAnchor.constraint(equalToConstant: 0)
        comuxNotificationsHeight = notifications.heightAnchor.constraint(equalToConstant: 0)
        comuxSidebarWidth = sidebar.widthAnchor.constraint(equalToConstant: 0)
        comuxNotificationsWidth = notifications.widthAnchor.constraint(equalToConstant: 0)
        comuxNotifSlotWidth = slot.widthAnchor.constraint(equalToConstant: 0)
        NSLayoutConstraint.activate([
            sidebar.leadingAnchor.constraint(equalTo: contentArea.leadingAnchor),
            sidebar.topAnchor.constraint(equalTo: contentArea.topAnchor),
            sidebar.bottomAnchor.constraint(equalTo: status.topAnchor),
            comuxSidebarWidth,
            status.leadingAnchor.constraint(equalTo: contentArea.leadingAnchor),
            status.trailingAnchor.constraint(equalTo: contentArea.trailingAnchor),
            status.bottomAnchor.constraint(equalTo: contentArea.bottomAnchor),
            comuxStatusHeight,
            // The top bar spans the terminal's column, between the sidebar and the graph dock.
            topBar.leadingAnchor.constraint(equalTo: sidebar.trailingAnchor),
            topBar.trailingAnchor.constraint(equalTo: slot.leadingAnchor),
            topBar.topAnchor.constraint(equalTo: contentArea.topAnchor),
            comuxTopHeight,
            // The notifications hang under the bell, at the top bar's right end.
            notifications.trailingAnchor.constraint(equalTo: topBar.trailingAnchor, constant: -6),
            notifications.topAnchor.constraint(equalTo: topBar.bottomAnchor, constant: 4),
            comuxNotificationsHeight,
            comuxNotificationsWidth,
            graph.leadingAnchor.constraint(equalTo: slot.leadingAnchor),
            graph.trailingAnchor.constraint(equalTo: slot.trailingAnchor),
            graph.topAnchor.constraint(equalTo: slot.topAnchor),
            graph.bottomAnchor.constraint(equalTo: slot.bottomAnchor),
            graphGrip.centerXAnchor.constraint(equalTo: graph.leadingAnchor),
            graphGrip.widthAnchor.constraint(equalToConstant: 6),
            graphGrip.topAnchor.constraint(equalTo: graph.topAnchor),
            graphGrip.bottomAnchor.constraint(equalTo: graph.bottomAnchor),
            grip.centerXAnchor.constraint(equalTo: sidebar.trailingAnchor),
            grip.widthAnchor.constraint(equalToConstant: 6),
            grip.topAnchor.constraint(equalTo: sidebar.topAnchor),
            grip.bottomAnchor.constraint(equalTo: sidebar.bottomAnchor),
            card.leadingAnchor.constraint(equalTo: sidebar.trailingAnchor),
            card.topAnchor.constraint(equalTo: contentArea.topAnchor),
            card.bottomAnchor.constraint(equalTo: status.topAnchor),
            comuxCardWidth,
            slot.trailingAnchor.constraint(equalTo: contentArea.trailingAnchor),
            slot.topAnchor.constraint(equalTo: contentArea.topAnchor),
            slot.bottomAnchor.constraint(equalTo: status.topAnchor),
            comuxNotifSlotWidth,
        ])
        comuxWindow.style = ComuxStyle.make(config: config, theme: theme)
        sidebar.isHidden = true
        notifications.isHidden = true
        status.isHidden = true
        topBar.isHidden = true
        graph.isHidden = true
        graphGrip.isHidden = true
        comuxObservation = ComuxHost.shared.$states.sink { [weak self] _ in
            DispatchQueue.main.async { self?.refreshComuxChrome() }
        }
        // Which panes are on screen changes with tab switches, splits and closes; re-reading it
        // once a second is cheaper than threading a hook through every one of those paths.
        comuxServeTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.reportComuxServed() }
        }
    }

    /// The terminal panels this window shows (every split of the visible tab).
    private var visibleTerminalPanels: [String] {
        (activePaneManager?.allPanels() ?? [])
            .filter { $0 is AlacrittyTerminalViewController }
            .map(\.panelID)
    }

    private func reportComuxServed() {
        let panels = view.window?.isVisible == true ? visibleTerminalPanels : []
        guard panels != comuxServed else { return }
        comuxServed = panels
        ComuxHost.shared.serve(window: ObjectIdentifier(self), panels: panels)
        refreshComuxChrome()
    }

    private func refreshComuxChrome() {
        guard comuxSidebar != nil else { return }
        comuxWindow.update(panels: comuxServed)
        // `Ctrl-b e` from a comux client in this window (decisions #181). Only the comux whose
        // sidebar this window shows can be focused; another link's request stays unanswered.
        if let key = comuxWindow.key, comuxWindow.state != nil,
           comuxServed.contains(where: { ComuxHost.shared.takeFocus(key: key, panel: $0) }) {
            enterComuxSidebarFocus()
        }
        let on = comuxWindow.state != nil
        comuxSidebar.isHidden = !on
        comuxGrip?.isHidden = !on
        comuxStatus.isHidden = !on
        comuxTopBar.isHidden = !on
        if comuxWindow.notificationsShown != (on && comuxNotificationsOpen) {
            comuxWindow.notificationsShown = on && comuxNotificationsOpen
        }
        layoutComuxChrome()
        presentComuxOverlay()
        if !comuxGraph.isHidden { comuxGraph.feed() }
    }

    /// Open the work-graph panel on `mode`, or close it when it already shows that (the top
    /// bar's two buttons, ⌘⇧G = `agent`).
    func toggleComuxGraph(mode: String) {
        let w = comuxWindow
        let open = !(w.graphOpen && w.graphMode == mode)
        setComuxGraph(open: open, mode: mode, animated: true)
    }

    func setComuxGraph(open: Bool, mode: String?, animated: Bool) {
        let w = comuxWindow
        if let mode, mode != w.graphMode {
            w.graphMode = mode
            UserDefaults.standard.set(mode, forKey: Self.comuxGraphModeKey)
            comuxGraph.setMode(mode)
        }
        guard open != w.graphOpen else { return }
        w.graphOpen = open
        UserDefaults.standard.set(open, forKey: Self.comuxGraphOpenKey)
        comuxGraphAnimated = animated && !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        layoutComuxChrome()
        comuxGraphAnimated = false
        if open { comuxGraph.feed() }
    }

    /// The e2e's view of the panel: open, mode, and the page's own read-out.
    func comuxGraphStatus(_ done: @escaping ([String: Any]) -> Void) {
        let w = comuxWindow
        var out: [String: Any] = ["open": w.graphOpen, "mode": w.graphMode, "shown": !comuxGraph.isHidden,
                                  "width": comuxGraph.frame.width, "loaded": comuxGraph.loaded,
                                  "terminal_width": activePaneManager?.containerView.frame.width ?? 0]
        comuxGraph.pageState { s in
            out["page"] = s ?? NSNull()
            done(out)
        }
    }

    func setComuxNotifications(open: Bool) {
        comuxNotificationsOpen = open
        refreshComuxChrome()
        view.layoutSubtreeIfNeeded()
    }

    /// The dropdown's state for the e2e: shown, and its frame (x, y from the content's top,
    /// width, height) beside the top bar's.
    func comuxNotificationsStatus() -> [String: Any] {
        let f = comuxNotifications.frame
        let top = contentArea.bounds.height
        let bar = comuxTopBar.frame
        return ["shown": !comuxNotifications.isHidden,
                "frame": [f.minX, top - f.maxY, f.width, f.height],
                "top_bar": [bar.minX, top - bar.maxY, bar.width, bar.height]]
    }

    private var comuxGraphAnimated = false
    static let comuxGraphOpenKey = "comuxGraphOpen"
    static let comuxGraphModeKey = "comuxGraphMode"
    static let comuxGraphWidthKey = "comuxGraphWidth"

    /// Panel sizes follow the terminal font (decisions #178) within ONE budget of the window
    /// (decisions #204): the terminal keeps at least 40 columns (and 320 pt); past that the
    /// work-graph dock gives way first (to its 260 pt floor), then the sidebar (to 160 pt), and
    /// if that is still too much the dock is hidden — not closed: it comes back when the window
    /// grows. Restored and dragged widths go through the same budget.
    private func layoutComuxChrome() {
        guard comuxSidebar != nil else { return }
        let on = comuxWindow.state != nil
        let st = comuxWindow.style
        let width = contentArea.bounds.width
        let natural = min(max(30 * st.advance, 200), max(0.3 * width, 160))
        // A width the user dragged to (decisions #181) wins, within what leaves the terminal usable.
        let dragged = UserDefaults.standard.double(forKey: Self.comuxSidebarWidthKey)
        var side = on ? (dragged > 0 ? min(max(dragged, 160), max(0.6 * width, 160)) : natural).rounded() : 0
        let graphWanted = on && comuxWindow.graphOpen
        let draggedGraph = UserDefaults.standard.double(forKey: Self.comuxGraphWidthKey)
        var graph = graphWanted
            ? (draggedGraph > 0 ? max(draggedGraph, 260) : min(max(42 * st.advance, 300), 0.45 * width)).rounded() : 0
        let budget = width - max(40 * st.advance, 320)
        if side + graph > budget, graph > 0 { graph = max(260, budget - side) }
        if side + graph > budget, on { side = max(160, budget - graph) }
        if side + graph > budget { graph = 0 }
        set(comuxSidebarWidth, side)
        set(comuxCardWidth, on ? (72 * st.advance + 40).rounded() : 0)
        set(comuxStatusHeight, on ? st.statusHeight : 0)
        set(comuxTopHeight, on ? st.statusHeight : 0)
        if comuxGraphAnimated {
            NSAnimationContext.runAnimationGroup { ctx in
                ctx.duration = 0.18
                ctx.allowsImplicitAnimation = true
                comuxNotifSlotWidth.animator().constant = graph
                contentArea.layoutSubtreeIfNeeded()
            }
        } else {
            set(comuxNotifSlotWidth, graph)
        }
        comuxGraph.isHidden = graph == 0
        comuxGraphGrip?.isHidden = graph == 0
        let notifOpen = on && comuxNotificationsOpen
        let notif = notifOpen ? min(max(38 * st.advance, 260), max(0.35 * width, 220)).rounded() : 0
        set(comuxNotificationsWidth, notif)
        // As tall as its rows (a header, then two lines per notification), within 60% of the window.
        let rows = CGFloat(max(comuxWindow.state?.notifications?.items.count ?? 0, 1))
        let fits = st.oneLineHeight + 7 + rows * (st.lineHeight * 2 + 14) + 8
        set(comuxNotificationsHeight, notifOpen ? min(fits, max(0.6 * contentArea.bounds.height, 120)).rounded() : 0)
        comuxNotifications.isHidden = !notifOpen
        watchFloatingNotifications(notifOpen)
    }

    private func set(_ c: NSLayoutConstraint, _ v: CGFloat) {
        if c.constant != v { c.constant = v }
    }

    /// While the notification panel floats, Esc or a click outside it closes it.
    private func watchFloatingNotifications(_ on: Bool) {
        if !on {
            if let m = comuxFloatMonitor { NSEvent.removeMonitor(m) }
            comuxFloatMonitor = nil
            return
        }
        guard comuxFloatMonitor == nil else { return }
        comuxFloatMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .leftMouseDown]) { [weak self] e in
            guard let self, e.window === self.view.window, self.comuxNotificationsOpen else { return e }
            if e.type == .keyDown, e.keyCode == 53 {
                self.closeComuxNotifications()
                return nil
            }
            if e.type == .leftMouseDown {
                // The top bar (whose bell toggles this dropdown) is not "outside": closing here
                // and then running the bell's toggle would reopen it.
                let inside = [self.comuxNotifications, self.comuxTopBar].contains { v in
                    v.map { $0.bounds.contains($0.convert(e.locationInWindow, from: nil)) } ?? false
                }
                if !inside { self.closeComuxNotifications() }
            }
            return e
        }
    }

    private func enterComuxSidebarFocus() {
        settleComuxInput()
        let active = comuxWindow.state?.tree?.active
        comuxWindow.onSidebarExit = { [weak self] in self?.refocusPane() }
        comuxWindow.sidebarFocus = ComuxWindow.SidebarFocus(agents: false, id: active, index: 0)
        view.window?.makeFirstResponder(comuxSidebar)
    }

    static let comuxSidebarWidthKey = "comuxSidebarWidth"

    private func closeComuxNotifications() {
        comuxNotificationsOpen = false
        refreshComuxChrome()
        refocusPane()
    }

    override func viewDidLayout() {
        super.viewDidLayout()
        layoutComuxChrome()
    }

    /// Show the oldest overlay a comux client in THIS window's visible panes opened: a sheet, or
    /// for a context menu (decisions #172) a native menu at the pointer.
    private func presentComuxOverlay() {
        let host = comuxWindow
        let mine = host.state?.overlays.first { o in o.panel.map(comuxServed.contains) ?? false }
        if let shown = comuxOverlayShown, mine?.overlay != shown {
            // Answered elsewhere, or comux closed it: drop our layer.
            closeComuxOverlay()
        }
        // The same overlay again (rows landed, a live re-send) updates in place: the layer reads
        // the window's state itself.
        // A host prompt of copad's own (Open remote comux) is up: comux's waits for it.
        guard comuxOverlayShown == nil, comuxOverlayLayer == nil, let o = mine else { return }
        settleComuxInput()
        comuxOverlayShown = o.overlay
        if case let .menu(_, items) = o.spec {
            popUpComuxMenu(o.overlay, items: items)
            return
        }
        let layer = NSHostingView(rootView: ComuxOverlayHost(host: host, id: o.overlay) { [weak self] in
            self?.closeComuxOverlay()
        })
        mountComuxOverlay(layer)
    }

    /// Put an overlay layer over the content area and keep the keys in it until it closes.
    private func mountComuxOverlay(_ layer: NSView) {
        layer.translatesAutoresizingMaskIntoConstraints = false
        contentArea.addSubview(layer)
        NSLayoutConstraint.activate([
            layer.leadingAnchor.constraint(equalTo: contentArea.leadingAnchor),
            layer.trailingAnchor.constraint(equalTo: contentArea.trailingAnchor),
            layer.topAnchor.constraint(equalTo: contentArea.topAnchor),
            layer.bottomAnchor.constraint(equalTo: contentArea.bottomAnchor),
        ])
        comuxOverlayLayer = layer
        view.window?.makeFirstResponder(layer)
        DispatchQueue.main.async { [weak self] in
            if self?.comuxOverlayLayer === layer { self?.view.window?.makeFirstResponder(layer) }
        }
        // Keys stay in the overlay: if focus wandered out of it (a click that landed nowhere),
        // the next key is routed back before it can reach a terminal.
        comuxOverlayMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
            guard let self, let layer = self.comuxOverlayLayer, e.window === self.view.window else { return e }
            let inside = (e.window?.firstResponder as? NSView).map { $0.isDescendant(of: layer) } ?? false
            if !inside { e.window?.makeFirstResponder(layer) }
            return e
        }
    }

    private func closeComuxOverlay() {
        guard comuxOverlayShown != nil || comuxOverlayLayer != nil else { return }
        if let m = comuxOverlayMonitor { NSEvent.removeMonitor(m) }
        comuxOverlayMonitor = nil
        comuxOverlayLayer?.removeFromSuperview()
        comuxOverlayLayer = nil
        comuxOverlayShown = nil
        comuxWindow.preview(pane: nil, rows: 0, cols: 0)
        refocusPane()
    }

    private func refocusPane() {
        if let p = activePaneManager?.activePane { view.window?.makeFirstResponder(p.focusTarget) }
    }

    /// comux's right-click menu as an NSMenu where the pointer is; closing it unpicked cancels.
    private func popUpComuxMenu(_ overlay: UInt64, items: [L0MenuItem]) {
        let menu = NSMenu()
        let picker = ComuxMenuPicker()
        for item in items {
            let mi = NSMenuItem(title: item.label, action: #selector(ComuxMenuPicker.pick(_:)), keyEquivalent: "")
            mi.target = picker
            mi.representedObject = item.id
            if item.destructive == true {
                mi.attributedTitle = NSAttributedString(string: item.label, attributes: [.foregroundColor: NSColor.systemRed])
            }
            menu.addItem(mi)
        }
        let at = view.window.map { view.convert($0.mouseLocationOutsideOfEventStream, from: nil) } ?? .zero
        // popUp blocks until the menu closes; the picked item (if any) was recorded by then.
        menu.popUp(positioning: nil, at: at, in: view)
        comuxWindow.answer(overlay, choice: picker.picked, text: nil)
        comuxOverlayShown = nil
        refocusPane()
    }

    /// Open a remote comux in a new tab (decisions #171): copad starts the remote client itself
    /// (`ssh -t`), with a fresh binding tag, and the L0 link to the same host — so the native
    /// chrome follows a REMOTE comux exactly as it does a local one. Returns an error message.
    func openRemoteComux(dest: String, comux: String) -> String? {
        newTab()
        guard let panel = activePaneManager?.activePane.panelID else { return "no new tab" }
        guard let cmd = ComuxHost.shared.openRemote(dest: dest, comux: comux, panel: panel) else {
            return "refused destination or command: \(dest) / \(comux)"
        }
        execCommand(cmd)
        return nil
    }

    /// Ask for an SSH destination in the window's own overlay card, the one comux's prompts use,
    /// and open its comux; a refused or failed destination asks again with the reason.
    func promptOpenRemoteComux(title: String = "Open remote comux", initial: String = "") {
        guard comuxOverlayLayer == nil else { return }
        let o = L0Overlay(overlay: 0, client: 0, panel: nil,
                          spec: .prompt(title: title, initial: initial, placeholder: "user@host or an ~/.ssh/config host"))
        let layer = NSHostingView(rootView: ComuxOverlayLayer(
            overlay: o, look: comuxWindow.style.overlayLook, context: OverlayContext(),
            handlers: OverlayHandlers(
                answer: { [weak self] _, text in
                    self?.closeComuxOverlay()
                    let dest = (text ?? "").trimmingCharacters(in: .whitespaces)
                    guard !dest.isEmpty else { return }
                    // Not from inside the layer's own callback: it was just removed.
                    DispatchQueue.main.async {
                        guard let self, let err = self.openRemoteComux(dest: dest, comux: "comux") else { return }
                        self.promptOpenRemoteComux(title: "Open remote comux — \(err)", initial: dest)
                    }
                },
                act: { _ in }, preview: { _, _, _ in })))
        mountComuxOverlay(layer)
    }

    /// Commit the focused terminal's IME composition before a panel click moves comux's view,
    /// so a half-typed syllable is not delivered to the pane comux switches to.
    private func settleComuxInput() {
        if let client = activePaneManager?.activePane.focusTarget as? NSTextInputClient,
           client.hasMarkedText() {
            client.unmarkText()
        }
    }

    /// Notification observers that are per-CONTROLLER, not per-tab. They must not be registered
    /// in `addTab`: that added one observer per tab ever opened, never removed on close, so every
    /// navigation enqueued more main-actor tasks and timer resets. Tokens are kept so `deinit`
    /// can remove them.
    private func registerGlobalObserversIfNeeded() {
        guard globalObservers.isEmpty else { return }
        globalObservers = [
            NotificationCenter.default.addObserver(
                forName: .terminalTitleChanged, object: nil, queue: .main,
            ) { [weak self] _ in
                Task { @MainActor in self?.refreshTabBar() }
            },
            // A browser pane's URL is part of the persisted layout, so a navigation must
            // schedule a save like a split or tab switch; otherwise only the URL at the next
            // structural change reached disk.
            NotificationCenter.default.addObserver(
                forName: .webviewStateChanged, object: nil, queue: .main,
            ) { [weak self] _ in
                Task { @MainActor in self?.scheduleSessionSave() }
            },
            // The background is decoded at the size the CURRENT screens need (see
            // `decodeBackground`), so a display being attached, detached or rescaled is the
            // one event that can invalidate it.
            NotificationCenter.default.addObserver(
                forName: NSApplication.didChangeScreenParametersNotification,
                object: nil, queue: .main,
            ) { [weak self] _ in
                Task { @MainActor in self?.reapplyBackgroundForScreenChange() }
            },
        ]
    }

    /// Call after any structural mutation. Coalesces bursts into one atomic write ~800ms later.
    /// No-op while restoring.
    func scheduleSessionSave() {
        guard !suppressSessionSave else { return }
        sessionSaveTimer?.invalidate()
        sessionSaveTimer = Timer.scheduledTimer(withTimeInterval: 0.8, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                let snap = self.snapshotSession()
                if snap.tabs.isEmpty {
                    Session.clear()
                } else if Session.save(snap) {
                    // Only after a save that COMMITTED: reclaiming against a snapshot that never
                    // reached disk would delete generations the committed session still points at.
                    Self.gcHistoryBlobs(after: snap)
                }
            }
        }
    }

    // MARK: - Tab Operations

    func newTab() {
        addTab(manager: makeTerminalManager())
    }

    /// Terminal tab seeded with cwd + initial input (used by `claude.start`). Returns
    /// `(panel_id, tab)` like Linux's `add_tab_with_cwd_and_initial_input`.
    @discardableResult
    func newTerminalTab(cwd: String?, initialInput: String?) -> (panelID: String, tab: Int) {
        let manager = PaneManager(
            config: config,
            theme: theme,
            initialPanel: .terminalSeed(cwd: cwd, initialInput: initialInput),
        )
        addTab(manager: manager)
        return (manager.activePane.panelID, paneManagers.count - 1)
    }

    /// `background: true` creates the copad tab without switching to it, so an
    /// agent opening a page does not yank the user out of what they were
    /// looking at.
    func newWebViewTab(url: URL? = nil, background: Bool = false) {
        let manager = PaneManager(config: config, theme: theme, initialPanel: .webview(url: url))
        addTab(manager: manager, background: background)
    }

    @discardableResult
    func newPluginPanelTab(_ panel: any CopadPanel) -> String {
        let manager = PaneManager(config: config, theme: theme, initialPanel: .pluginPanel(panel))
        addTab(manager: manager)
        return panel.panelID
    }

    @discardableResult
    func splitActivePaneWithPluginPanel(_ panel: any CopadPanel, orientation: SplitOrientation = .horizontal) -> String? {
        guard let manager = activePaneManager else { return nil }
        manager.splitActiveWithPluginPanel(panel, orientation: orientation)
        return panel.panelID
    }

    private func makeTerminalManager() -> PaneManager {
        PaneManager(config: config, theme: theme)
    }

    private func addTab(manager: PaneManager, background: Bool = false) {
        manager.onLastPaneClosed = { [weak self, weak manager] in
            guard let self, let manager else { return }
            if let index = paneManagers.firstIndex(where: { $0 === manager }) {
                closeTab(at: index)
            }
        }
        manager.onActivePaneChanged = { [weak self] in
            self?.refreshTabBar()
            self?.scheduleSessionSave()  // split / focus changes alter the tree
        }
        manager.onPaneAdded = { [weak self] panel in
            // Fan window-level state onto the new pane: the background-active gate, so
            // default-bg cells stay transparent and the shared wallpaper shows through.
            guard let self else { return }
            if currentBackgroundPath != nil {
                panel.applyBackground(path: "", tint: 0, opacity: 0)
            }
            scheduleSessionSave()  // a split added a pane
        }
        manager.onLayoutChanged = { [weak self] in
            self?.scheduleSessionSave()  // a pane closed (incl. non-active)
        }

        registerGlobalObserversIfNeeded()

        manager.eventBus = eventBus
        paneManagers.append(manager)
        let tabIndex = paneManagers.count - 1

        if paneManagers.count == 2, isBarCollapsed, !userToggledBar {
            isBarCollapsed = false
            tabBar.setCollapsed(false)
            syncVerticalBarWidth()
        }

        if background {
            // `switchTab` is what STARTS a tab's panes; skipping it left a background browser
            // pane with an empty tab array (page commands answered `tab_closed`, autosave
            // persisted an empty URL). Start it here without selecting it.
            manager.allPanels().forEach { $0.startIfNeeded() }
        } else {
            switchTab(to: tabIndex)
        }
        eventBus?.broadcast(event: "tab.opened", data: [
            "index": tabIndex,
            "panel_id": manager.activePane.panelID,
        ])
        if let path = currentBackgroundPath {
            manager.applyBackground(path: path, tint: currentBackgroundTint, opacity: currentBackgroundOpacity)
        }
        scheduleSessionSave()
    }

    func closeTab(at index: Int) {
        guard paneManagers.indices.contains(index) else { return }

        let manager = paneManagers[index]
        manager.containerView.removeFromSuperview()
        paneManagers.remove(at: index)
        eventBus?.broadcast(event: "tab.closed", data: ["index": index])
        scheduleSessionSave()

        if paneManagers.isEmpty {
            view.window?.close()
            return
        }

        let nextIndex = min(activeIndex, paneManagers.count - 1)
        activeIndex = -1
        switchTab(to: nextIndex)
    }

    private func closeTabByButton(at index: Int) {
        guard paneManagers.indices.contains(index) else { return }
        let manager = paneManagers[index]
        manager.allPanels().forEach { $0.view.removeFromSuperview(); $0.removeFromParent() }
        manager.containerView.removeFromSuperview()
        paneManagers.remove(at: index)

        if paneManagers.isEmpty {
            view.window?.close()
            return
        }

        let nextIndex = min(activeIndex, paneManagers.count - 1)
        activeIndex = -1
        switchTab(to: nextIndex)
    }

    func switchTab(to index: Int) {
        guard paneManagers.indices.contains(index), index != activeIndex else { return }

        if let current = activePaneManager {
            current.containerView.removeFromSuperview()
        }

        activeIndex = index
        let manager = paneManagers[index]

        // Under the comux chrome, so a floating notification panel stays on top of it.
        contentArea.addSubview(manager.containerView, positioned: .below, relativeTo: comuxNotifSlot)
        NSLayoutConstraint.activate([
            manager.containerView.topAnchor.constraint(equalTo: comuxTopBar.bottomAnchor),
            // Between the comux panels, which are zero-width unless this window hosts a comux.
            manager.containerView.leadingAnchor.constraint(equalTo: comuxSidebar.trailingAnchor),
            manager.containerView.trailingAnchor.constraint(equalTo: comuxNotifSlot.leadingAnchor),
            manager.containerView.bottomAnchor.constraint(equalTo: comuxStatus.topAnchor),
        ])

        view.layoutSubtreeIfNeeded()
        // The split views now have a real size: apply persisted divider ratios that couldn't be
        // set while the tab was off-screen.
        manager.applyPendingRatios()
        manager.allPanels().forEach { $0.startIfNeeded() }
        manager.activePane.view.window?.makeFirstResponder(manager.activePane.focusTarget)

        refreshTabBar()
        scheduleSessionSave()  // active tab changed
    }

    // MARK: - Split Operations

    func splitActivePane(orientation: SplitOrientation) {
        activePaneManager?.splitActive(orientation: orientation)
    }

    func focusNextPane(direction: Int = 1) {
        activePaneManager?.focusNextPane(direction: direction)
    }

    func splitActivePaneWithWebView(
        url: URL? = nil,
        orientation: SplitOrientation = .horizontal,
        background: Bool = false,
    ) {
        activePaneManager?.splitActiveWithWebView(
            url: url, orientation: orientation, background: background,
        )
    }

    func closeActivePane() {
        activePaneManager?.closeActive()
    }

    // MARK: - Tab Bar

    func toggleTabBar(userInitiated: Bool = false) {
        if userInitiated { userToggledBar = true }
        isBarCollapsed.toggle()
        tabBar.setCollapsed(isBarCollapsed)
        syncVerticalBarWidth()
        refreshTabBar()
        eventBus?.broadcast(event: "tab.bar_toggled", data: ["collapsed": isBarCollapsed])
    }

    /// Vertical bars carry their collapsed state in the bar WIDTH (horizontal bars only swap
    /// tab-pill widths inside `TabBarView`): re-point the width constraint whenever
    /// `isBarCollapsed` changes. No-op for horizontal bars.
    private func syncVerticalBarWidth() {
        guard config.tabsPosition.isVertical, let sizeConstraint = tabBarSizeConstraint else { return }
        sizeConstraint.constant = isBarCollapsed ? TabBarView.collapsedBarWidth : CGFloat(config.tabsWidth)
    }

    private func refreshTabBar() {
        let titles = paneManagers.map(\.activePane.currentTitle)
        let types: [TabPanelType] = paneManagers.map { m in
            m.activePane is WebViewController ? .webview : .terminal
        }
        tabBar.setTabs(titles: titles, types: types, activeIndex: activeIndex)
    }

    // MARK: - Config Hot-Reload

    /// Called when the config file changes. Applies theme and font to running terminals; the
    /// background is re-applied only if the path/tint changed. Shell changes only affect new
    /// terminals.
    func applyConfig(_ newConfig: CopadConfig, theme: CopadTheme) {
        config = newConfig
        self.theme = theme

        // Root view bg follows window opacity: an opaque root blocks both the translucent window
        // backgroundColor and the blur view beneath it. Same conditional as `loadView`.
        view.layer?.backgroundColor = Self.rootBg(theme: theme, opacity: newConfig.windowOpacity)

        // Chrome: rebuild bg colors for the new theme + opacity. Pills stay opaque; only the
        // outer bar bgs pick up the alpha.
        tabBar?.applyWindowOpacity(newConfig.windowOpacity, theme: theme)
        statusBar?.applyWindowOpacity(newConfig.windowOpacity, theme: theme)
        comuxWindow.style = ComuxStyle.make(config: newConfig, theme: theme)
        layoutComuxChrome()

        for paneManager in paneManagers {
            paneManager.applyConfig(newConfig, theme: theme)
        }

        // Re-scale the window-level bg / tint alpha against the new window.opacity.
        refreshBackgroundForWindowOpacity()

        if let path = newConfig.backgroundPath {
            applyBackground(path: path, tint: newConfig.backgroundTint, opacity: newConfig.backgroundOpacity)
        } else if currentBackgroundPath != nil, !currentBackgroundFromList {
            clearBackground()
        } else {
            // No config image, or a rotated wallpaper to preserve (a reload touching only
            // tint/opacity/interval must not wipe a rotation pick, as Linux's
            // `showing_list_image` guard). Only the tint knob may have changed.
            setTint(newConfig.backgroundTint)
        }

        view.window?.backgroundColor = theme.background.nsColor
    }

    // MARK: - Background

    /// Window-level background: one image across the whole `contentArea` (under all splits).
    /// Decoded async with a monotonic load token to drop stale loads; image and tint alpha are
    /// scaled by `window.opacity` so the desktop bleeds through when transparent.
    func applyBackground(path: String, tint: Double, opacity: Double = 1.0, fromList: Bool = false) {
        currentBackgroundPath = path
        currentBackgroundTint = tint
        currentBackgroundOpacity = opacity
        currentBackgroundFromList = fromList
        ensureBackgroundViews()
        backgroundLoadToken &+= 1
        let token = backgroundLoadToken
        // Read the screen geometry HERE: AppKit is main-thread-only, and the decode below
        // runs on a background queue.
        let envelope = Self.backingPixelEnvelope()
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let cgImage = Self.decodeBackground(path: path, envelope: envelope)
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                // Stale: a newer applyBackground / clearBackground won the race.
                guard token == backgroundLoadToken else { return }
                // A failed decode must not wipe a previously-good image.
                guard let cgImage else { return }
                let windowOpacity = currentWindowOpacity()
                backgroundView?.layer?.contents = cgImage
                backgroundView?.alphaValue = CGFloat(opacity * windowOpacity)
                backgroundView?.isHidden = false
                tintView?.layer?.backgroundColor = NSColor.black
                    .withAlphaComponent(CGFloat(tint * windowOpacity)).cgColor
                tintView?.isHidden = opacity == 0
                // Fan to every pane's render view so default-bg cells
                // stay transparent and the image shows through.
                fanSetImageBackgroundActive(true)
            }
        }
    }

    /// The largest backing-pixel size any screen could ask this window's background to fill.
    ///
    /// Derived from the SCREENS rather than the view: `.resizeAspectFill` means a bitmap sized
    /// to today's window is upscaled (and visibly softens) the moment the window grows or
    /// moves to a denser display, and a view-derived cap would force a fresh decode on every
    /// resize. A screen-derived envelope is stable, so the decode happens once per image.
    private static func backingPixelEnvelope() -> [CGSize] {
        let screens = NSScreen.screens.map { screen -> CGSize in
            let scale = screen.backingScaleFactor
            return CGSize(
                width: screen.frame.width * scale,
                height: screen.frame.height * scale,
            )
        }
        // No screens at all (headless/locked) — fall back to something sane rather than
        // deciding the image may be one pixel wide.
        return screens.isEmpty ? [CGSize(width: 3840, height: 2160)] : screens
    }

    /// Decode `path` at the smallest size that still fully covers `envelope` under
    /// `.resizeAspectFill`. Decoding at the image's NATURAL size cost ~96 MB of RGBA per
    /// 6000x4000 camera JPEG, far more than any screen could show.
    ///
    /// Sizing is aspect-fill-aware, NOT a plain longest-side cap: capping the long side of a
    /// 4000x6000 portrait at a 3440-wide screen yields 2293x3440, which upscales when filling
    /// 3440x1440. So the scale is computed as the layer will use it (`max(W/Iw, H/Ih)`, clamped
    /// to 1 so a small image is never enlarged) and then turned into a longest-side bound.
    private static func decodeBackground(path: String, envelope: [CGSize]) -> CGImage? {
        let url = URL(fileURLWithPath: path)
        guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
              let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let rawW = (props[kCGImagePropertyPixelWidth] as? NSNumber)?.doubleValue,
              let rawH = (props[kCGImagePropertyPixelHeight] as? NSNumber)?.doubleValue,
              rawW > 0, rawH > 0
        else { return fallbackDecode(path: path) }

        // EXIF orientations 5-8 mean the stored pixel dimensions are transposed relative to
        // how the image is DISPLAYED. Sizing against the stored ones would compute the scale
        // for a sideways image. This only affects the arithmetic — ImageIO applies the
        // rotation itself, once, via `kCGImageSourceCreateThumbnailWithTransform` below.
        let orientation = (props[kCGImagePropertyOrientation] as? NSNumber)?.intValue ?? 1
        let sourceW = (5...8).contains(orientation) ? rawH : rawW
        let sourceH = (5...8).contains(orientation) ? rawW : rawH

        let scale = envelope.reduce(0.0) { best, target in
            max(best, min(1.0, max(target.width / sourceW, target.height / sourceH)))
        }
        let bound = Int(ceil(scale * max(sourceW, sourceH)))
        guard bound > 0 else { return fallbackDecode(path: path) }

        let options: [CFString: Any] = [
            // Scale the real image. Without this a file carrying a small embedded EXIF
            // thumbnail would hand us THAT, and the wallpaper would render as mush.
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            // Without EXIF orientation every photo shot in portrait comes out rotated.
            kCGImageSourceCreateThumbnailWithTransform: true,
            // Decode HERE, on the background queue, rather than lazily on the main thread at
            // first draw.
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: bound,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
        else { return fallbackDecode(path: path) }
        return image
    }

    /// The original full-size path, kept as a fallback so a format ImageIO will open but not
    /// thumbnail can still show a background rather than none at all.
    private static func fallbackDecode(path: String) -> CGImage? {
        NSImage(contentsOfFile: path)?.cgImage(forProposedRect: nil, context: nil, hints: nil)
    }

    /// Re-decode the current background after a display change.
    ///
    /// The size envelope above is only stable while the screen configuration is: plugging in a
    /// larger or denser display raises what aspect-fill needs, and the bitmap already resident
    /// would be upscaled from then on.
    private func reapplyBackgroundForScreenChange() {
        guard let path = currentBackgroundPath else { return }
        applyBackground(
            path: path,
            tint: currentBackgroundTint,
            opacity: currentBackgroundOpacity,
            fromList: currentBackgroundFromList,
        )
    }

    func clearBackground() {
        currentBackgroundPath = nil
        currentBackgroundFromList = false
        backgroundLoadToken &+= 1
        backgroundView?.layer?.contents = nil
        backgroundView?.isHidden = true
        tintView?.isHidden = true
        fanSetImageBackgroundActive(false)
    }

    func setTint(_ alpha: Double) {
        currentBackgroundTint = alpha
        let windowOpacity = currentWindowOpacity()
        tintView?.layer?.backgroundColor = NSColor.black
            .withAlphaComponent(CGFloat(alpha * windowOpacity)).cgColor
    }

    /// `window.opacity` hot-reload: re-scales image + tint alpha.
    func refreshBackgroundForWindowOpacity() {
        let windowOpacity = currentWindowOpacity()
        backgroundView?.alphaValue = CGFloat(currentBackgroundOpacity * windowOpacity)
        tintView?.layer?.backgroundColor = NSColor.black
            .withAlphaComponent(CGFloat(currentBackgroundTint * windowOpacity)).cgColor
    }

    private func ensureBackgroundViews() {
        if backgroundView != nil { return }
        // Autoresizing, not Auto Layout: NSImageView reported its intrinsic size as the image's
        // pixel dimensions even with edge constraints, and that fittingSize propagated up to the
        // window. A plain layer-backed NSView has no intrinsic size; aspect-fill moves to the
        // layer (`contentsGravity = .resizeAspectFill`, set below), like Linux's Cover fit.
        let bg = NSView(frame: contentArea.bounds)
        bg.wantsLayer = true
        bg.isHidden = true
        bg.autoresizingMask = [.width, .height]
        if let layer = bg.layer {
            layer.contentsGravity = .resizeAspectFill
            // Clip cropped overflow at the view bounds (essential —
            // without this, the over-sized scaled image draws past
            // the view into split dividers and the tab bar).
            layer.masksToBounds = true
        }
        let firstSubview = contentArea.subviews.first
        if let firstSubview {
            contentArea.addSubview(bg, positioned: .below, relativeTo: firstSubview)
        } else {
            contentArea.addSubview(bg)
        }
        backgroundView = bg

        let tint = NSView(frame: contentArea.bounds)
        tint.wantsLayer = true
        tint.isHidden = true
        tint.autoresizingMask = [.width, .height]
        contentArea.addSubview(tint, positioned: .above, relativeTo: bg)
        tintView = tint
    }

    /// Flip every pane's renderer transparent-default-bg gate when the window-level background
    /// image appears or disappears.
    private func fanSetImageBackgroundActive(_ active: Bool) {
        for manager in paneManagers {
            for panel in manager.allPanels() {
                if active {
                    panel.applyBackground(path: "", tint: 0, opacity: 0)
                } else {
                    panel.clearBackground()
                }
            }
        }
    }

    /// Read `window.opacity` from the live config, which lives on each PaneManager (hot-reloaded
    /// there); any pane's value is authoritative.
    private func currentWindowOpacity() -> Double {
        paneManagers.first?.configWindowOpacity() ?? 1.0
    }

    // MARK: - Socket Commands

    //
    // Thin wrappers over `activeTerminalPanel` for the "active terminal, no id-resolution, no
    // error reporting" case; AppDelegate's `resolveTerminalPanel` is the path for id-based
    // lookup and Linux-style errors.

    func execCommand(_ command: String) {
        activeTerminalPanel?.execCommand(command)
    }

    func feedText(_ text: String) {
        activeTerminalPanel?.feedText(text)
    }

    func terminalState() -> [String: Any] {
        activeTerminalPanel?.terminalState() ?? [:]
    }

    func readScreen() -> [String: Any] {
        activeTerminalPanel?.readScreen() ?? [:]
    }

    func history(lines: Int = 100) -> [String: Any] {
        activeTerminalPanel?.history(lines: lines) ?? [:]
    }

    func context(historyLines: Int = 50) -> [String: Any] {
        activeTerminalPanel?.context(historyLines: historyLines) ?? [:]
    }

    func tabList() -> [[String: Any]] {
        paneManagers.enumerated().map { i, m in
            ["index": i, "title": m.activePane.currentTitle, "active": i == activeIndex]
        }
    }

    func tabInfo() -> [[String: Any]] {
        paneManagers.enumerated().map { i, m in
            [
                "index": i,
                "title": m.activePane.currentTitle,
                "active": i == activeIndex,
                "pane_count": m.allPanels().count,
            ]
        }
    }

    func renameTab(at index: Int, title: String) {
        guard paneManagers.indices.contains(index) else { return }
        paneManagers[index].setCustomTitle(title)
        refreshTabBar()
        if index == activeIndex {
            view.window?.title = title
        }
        eventBus?.broadcast(event: "tab.renamed", data: ["index": index, "title": title])
        scheduleSessionSave()  // in renameTab itself so socket-driven tab.rename persists too
    }

    func sessionList() -> [[String: Any]] {
        tabList()
    }

    func sessionInfo(index: Int) -> [String: Any]? {
        guard paneManagers.indices.contains(index) else { return nil }
        let m = paneManagers[index]
        let state = m.activeTerminalPanel()?.terminalState() ?? [:]
        return [
            "index": index,
            "title": m.activePane.currentTitle,
            "active": index == activeIndex,
            "pane_count": m.allPanels().count,
            "cols": state["cols"] ?? 0,
            "rows": state["rows"] ?? 0,
        ]
    }

    private static func rootBg(theme: CopadTheme, opacity: Double) -> CGColor {
        opacity < 1.0 ? NSColor.clear.cgColor : theme.background.nsColor.cgColor
    }
}

/// Records which comux menu item was picked (NSMenu actions need an object target).
private final class ComuxMenuPicker: NSObject {
    var picked: String?
    @objc func pick(_ sender: NSMenuItem) { picked = sender.representedObject as? String }
}

/// A thin strip on the comux sidebar's right edge: the resize cursor over it, and dragging it sets
/// the sidebar's width (decisions #181).
final class ComuxResizeGrip: NSView {
    var onDrag: ((CGFloat) -> Void)?
    /// 1 = dragging right widens (a left panel's grip), −1 = dragging left does (a right one's).
    var direction: CGFloat = 1
    var currentWidth: (() -> CGFloat)?
    private var start: (x: CGFloat, width: CGFloat)?

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .resizeLeftRight)
    }

    override func mouseDown(with e: NSEvent) {
        start = (e.locationInWindow.x, currentWidth?() ?? 0)
    }

    override func mouseDragged(with e: NSEvent) {
        guard let start else { return }
        onDrag?(max(120, start.width + (e.locationInWindow.x - start.x) * direction))
    }

    override func mouseUp(with e: NSEvent) {
        start = nil
    }
}

/// A hosting view for comux chrome that a click never makes first responder (so the terminal
/// keeps the keys), unless `takesKeys` says it should have them (sidebar keyboard mode, #181).
final class ChromeHostingView<Content: View>: NSHostingView<Content> {
    var takesKeys: () -> Bool = { false }
    override var acceptsFirstResponder: Bool { takesKeys() }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}
