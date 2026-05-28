import AppKit

/// Manages multiple tabs, each backed by a PaneManager (split-pane tree).
/// Panels can be terminals or webviews.
@MainActor
final class TabViewController: NSViewController {
    /// Mutable so config hot-reload affects panes spawned AFTER the reload (theme/font/security).
    /// Existing panes are updated separately via `applyConfig` fan-out.
    private var config: CopadConfig
    private var theme: CopadTheme

    private var tabBar: TabBarView!
    private var contentArea: NSView!
    /// Tier 4.2 — status bar at the bottom of the window. nil when
    /// `[statusbar] enabled = false`. Public so AppDelegate can wire
    /// it up post-launch (load modules from discovered plugin manifests
    /// + handle statusbar.show/hide/toggle socket commands).
    private(set) var statusBar: StatusBarView?
    private var paneManagers: [PaneManager] = []
    private(set) var activeIndex: Int = -1

    // Retained so new tabs inherit the current background state
    private(set) var currentBackgroundPath: String?
    private(set) var currentBackgroundTint: Double = 0.6
    private(set) var currentBackgroundOpacity: Double = 1.0

    // Tab bar collapsed state.
    // Default: collapsed (icon-only). Auto-expands on 1→2 tab transition
    // unless the user has manually toggled the bar.
    private var isBarCollapsed: Bool = true
    private var userToggledBar: Bool = false

    /// Set by AppDelegate; propagated to all PaneManagers.
    weak var eventBus: EventBus? {
        didSet { paneManagers.forEach { $0.eventBus = eventBus } }
    }

    var isTabBarCollapsed: Bool {
        isBarCollapsed
    }

    var activePaneManager: PaneManager? {
        paneManagers.indices.contains(activeIndex) ? paneManagers[activeIndex] : nil
    }

    var activeTerminal: TerminalViewController? {
        activePaneManager?.activeTerminal()
    }

    /// Backend-agnostic terminal accessor — returns either backend's
    /// controller (or nil for webview / plugin pane). Use this for
    /// socket `terminal.*` dispatch; reserve `activeTerminal` for the
    /// SwiftTerm-only call sites (URL click handler, custom-title
    /// setter, etc.).
    var activeTerminalPanel: (any TerminalCapable)? {
        activePaneManager?.activeTerminalPanel()
    }

    var activeWebView: WebViewController? {
        activePaneManager?.activeWebView()
    }

    /// Polymorphic zoom dispatch — covers both terminal backends
    /// without AppDelegate needing to know which one is in front.
    /// Returns nil for non-terminal panes (webview, plugin) so the
    /// View → Zoom menu items become no-ops there, matching what
    /// the SwiftTerm-only `activeTerminal` path used to do.
    var activeZoomable: (any Zoomable)? {
        activePaneManager?.activePane as? Zoomable
    }

    /// Cross-tab panel lookup by stable UUID. Used by socket commands that take an
    /// `id` param (parity with Linux's `find_panel_by_id`). Walks every tab's split
    /// tree — O(N panels) but N is small in practice.
    func panel(id: String) -> (any CopadPanel)? {
        for manager in paneManagers {
            if let p = manager.allPanels().first(where: { $0.panelID == id }) {
                return p
            }
        }
        return nil
    }

    /// First terminal panel across all tabs in DFS order — matches
    /// Linux's `TabManager::find_first_terminal`. Used by
    /// `resolveTerminalPanel` as the last-resort fallback when the
    /// caller passed no id and the active pane isn't a terminal.
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

    /// Push a shell-reported cwd onto the matching terminal panel.
    /// Returns true when the panel was found and updated; false for
    /// unknown id or non-terminal target. Called from the
    /// `panel.report_cwd` registry handler.
    @discardableResult
    func applyReportedCwd(panelID: String, cwd: String) -> Bool {
        guard let p = panel(id: panelID) else { return false }
        if let t = p as? TerminalViewController {
            t.setReportedCwd(cwd)
            return true
        }
        if let a = p as? AlacrittyTerminalViewController {
            a.setReportedCwd(cwd)
            return true
        }
        return false
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
        root.layer?.backgroundColor = theme.background.nsColor.cgColor

        tabBar = TabBarView(theme: theme)
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        tabBar.onSelectTab = { [weak self] i in self?.switchTab(to: i) }
        tabBar.onCloseTab = { [weak self] i in self?.closeTabByButton(at: i) }
        tabBar.onToggle = { [weak self] in
            self?.toggleTabBar(userInitiated: true)
        }
        tabBar.onRenameTab = { [weak self] index, title in
            guard let self else { return }
            renameTab(at: index, title: title)
            // Restore focus to the active pane's keyboard view after
            // the tab bar field resigns. `panel.focusTarget` returns
            // the inner input view rather than the layout container.
            if let activePanel = activePaneManager?.activePane {
                view.window?.makeFirstResponder(activePanel.focusTarget)
            }
        }
        tabBar.onNewPanel = { [weak self] type, mode in
            guard let self else { return }
            switch (type, mode) {
            case (.terminal, .tab): newTab()
            case (.terminal, .splitH): splitActivePane(orientation: .horizontal)
            case (.terminal, .splitV): splitActivePane(orientation: .vertical)
            case (.webview, .tab): newWebViewTab()
            case (.webview, .splitH): splitActivePaneWithWebView(orientation: .horizontal)
            case (.webview, .splitV): splitActivePaneWithWebView(orientation: .vertical)
            }
        }
        root.addSubview(tabBar)

        contentArea = NSView()
        contentArea.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(contentArea)

        // Tier 4.2 — status bar at the very bottom of the root view, BELOW
        // the tab bar even when `tabsPosition = bottom`. Linux does the
        // same: statusbar is the lowest-priority container and the rest
        // of the layout sits on top of it. macOS only supports
        // `[statusbar] position = bottom` for now; top would need to flip
        // the tabBar/contentArea anchors against statusBar's top edge,
        // which isn't worth the layout complexity until somebody asks.
        var statusBarBottom: NSLayoutYAxisAnchor = root.bottomAnchor
        if config.statusBar.enabled {
            let bar = StatusBarView(theme: theme)
            statusBar = bar
            root.addSubview(bar)
            NSLayoutConstraint.activate([
                bar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                bar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
                bar.bottomAnchor.constraint(equalTo: root.bottomAnchor),
                bar.heightAnchor.constraint(equalToConstant: CGFloat(config.statusBar.height)),
            ])
            statusBarBottom = bar.topAnchor
        }

        // Tier 1.4 — tabs position. The tabBar is always full-width and at
        // either the top or bottom of root; contentArea fills the rest.
        // left/right would need a 90-degree rotation of the bar view itself
        // (different layout pass) and is deferred until requested.
        var constraints: [NSLayoutConstraint] = [
            tabBar.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            tabBar.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            tabBar.heightAnchor.constraint(equalToConstant: TabBarView.height),
            contentArea.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            contentArea.trailingAnchor.constraint(equalTo: root.trailingAnchor),
        ]
        switch config.tabsPosition {
        case .top:
            constraints.append(contentsOf: [
                tabBar.topAnchor.constraint(equalTo: root.topAnchor),
                contentArea.topAnchor.constraint(equalTo: tabBar.bottomAnchor),
                contentArea.bottomAnchor.constraint(equalTo: statusBarBottom),
            ])
        case .bottom:
            constraints.append(contentsOf: [
                contentArea.topAnchor.constraint(equalTo: root.topAnchor),
                contentArea.bottomAnchor.constraint(equalTo: tabBar.topAnchor),
                tabBar.bottomAnchor.constraint(equalTo: statusBarBottom),
            ])
        }
        NSLayoutConstraint.activate(constraints)

        // Sync view to controller's initial state (single source of truth: isBarCollapsed)
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

    /// Build a wire snapshot of every live tab. Tabs that boil down to
    /// zero terminal panels (webview-only or all-plugin) are skipped —
    /// matches Linux's `snapshot_session` filter. The `current_tab`
    /// index is remapped onto the surviving tab list so the restored
    /// session lands on the same logical tab even when others were
    /// elided.
    func snapshotSession() -> Session.Snapshot {
        var tabs: [Session.TabSnap] = []
        var currentTab = 0
        let activeIdx = activeIndex
        for (idx, manager) in paneManagers.enumerated() {
            guard let root = manager.snapshotTree() else { continue }
            if idx == activeIdx {
                currentTab = tabs.count
            } else if idx < activeIdx {
                // Active tab might be elided itself — the closest
                // surviving tab BEFORE it is the best fallback.
                currentTab = tabs.count
            }
            tabs.append(Session.TabSnap(customTitle: manager.customTabTitle(), root: root))
        }
        let clamped = max(0, min(currentTab, tabs.count - 1))
        return Session.Snapshot(version: Session.version, tabs: tabs, currentTab: clamped)
    }

    /// Build tabs + splits to mirror `snap`. Caller (AppDelegate) is
    /// responsible for falling back to `openInitialTab` if this is a
    /// no-op (snap has zero tabs). Restored panels start fresh — we
    /// can't replay shell history or process state, just cwd + layout.
    func restoreSession(_ snap: Session.Snapshot) {
        for tabSnap in snap.tabs {
            let leftmost = Session.leftmostCwd(tabSnap.root)
            let manager = PaneManager(
                config: config,
                theme: theme,
                initialPanel: .terminalSeed(cwd: leftmost, initialInput: nil),
            )
            addTab(manager: manager)
            manager.restoreSplits(into: manager.activePane, from: tabSnap.root)
            if let title = tabSnap.customTitle {
                manager.setCustomTitle(title)
            }
        }
        let clamped = max(0, min(snap.currentTab, paneManagers.count - 1))
        if paneManagers.indices.contains(clamped) {
            switchTab(to: clamped)
        }
    }

    // MARK: - Tab Operations

    func newTab() {
        addTab(manager: makeTerminalManager())
    }

    /// PR 8 — terminal tab seeded with cwd + initial-input. Used by
    /// `claude.start` so the user lands in a worktree directory with
    /// `tmux new-session …` already running. Returns `(panel_id, tab)`
    /// so the socket reply can include both — same shape as Linux's
    /// `add_tab_with_cwd_and_initial_input` return tuple.
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

    func newWebViewTab(url: URL? = nil) {
        let manager = PaneManager(config: config, theme: theme, initialPanel: .webview(url: url))
        addTab(manager: manager)
    }

    /// Tier 4.1 — open a pre-built plugin panel as a new tab. Caller is
    /// AppDelegate's `plugin.open` handler, which has the registry + event
    /// bus references PluginPanelController needs at construction time.
    /// Returns the panel id so the caller can include it in the socket
    /// response for trigger/automation use cases.
    @discardableResult
    func newPluginPanelTab(_ panel: any CopadPanel) -> String {
        let manager = PaneManager(config: config, theme: theme, initialPanel: .pluginPanel(panel))
        addTab(manager: manager)
        return panel.panelID
    }

    /// Tier 4.1 — split active pane with a plugin panel. Same construction
    /// pattern as `newPluginPanelTab`; routes through PaneManager's
    /// `splitActiveWithPluginPanel`.
    @discardableResult
    func splitActivePaneWithPluginPanel(_ panel: any CopadPanel, orientation: SplitOrientation = .horizontal) -> String? {
        guard let manager = activePaneManager else { return nil }
        manager.splitActiveWithPluginPanel(panel, orientation: orientation)
        return panel.panelID
    }

    private func makeTerminalManager() -> PaneManager {
        PaneManager(config: config, theme: theme)
    }

    private func addTab(manager: PaneManager) {
        manager.onLastPaneClosed = { [weak self, weak manager] in
            guard let self, let manager else { return }
            if let index = paneManagers.firstIndex(where: { $0 === manager }) {
                closeTab(at: index)
            }
        }
        manager.onActivePaneChanged = { [weak self] in
            self?.refreshTabBar()
        }

        NotificationCenter.default.addObserver(
            forName: .terminalTitleChanged,
            object: nil,
            queue: .main,
        ) { [weak self] _ in
            Task { @MainActor in self?.refreshTabBar() }
        }

        manager.eventBus = eventBus
        paneManagers.append(manager)
        let tabIndex = paneManagers.count - 1

        // Auto-expand when going from 1 to 2 tabs (unless user manually toggled)
        if paneManagers.count == 2, isBarCollapsed, !userToggledBar {
            isBarCollapsed = false
            tabBar.setCollapsed(false)
        }

        switchTab(to: tabIndex)
        eventBus?.broadcast(event: "tab.opened", data: [
            "index": tabIndex,
            "panel_id": manager.activePane.panelID,
        ])
        if let path = currentBackgroundPath {
            manager.applyBackground(path: path, tint: currentBackgroundTint, opacity: currentBackgroundOpacity)
        }
    }

    func closeTab(at index: Int) {
        guard paneManagers.indices.contains(index) else { return }

        let manager = paneManagers[index]
        manager.containerView.removeFromSuperview()
        paneManagers.remove(at: index)
        eventBus?.broadcast(event: "tab.closed", data: ["index": index])

        if paneManagers.isEmpty {
            view.window?.close()
            return
        }

        let nextIndex = min(activeIndex, paneManagers.count - 1)
        activeIndex = -1
        switchTab(to: nextIndex)
    }

    /// Called from tab bar close button — closes all panes in the tab.
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

        contentArea.addSubview(manager.containerView)
        NSLayoutConstraint.activate([
            manager.containerView.topAnchor.constraint(equalTo: contentArea.topAnchor),
            manager.containerView.leadingAnchor.constraint(equalTo: contentArea.leadingAnchor),
            manager.containerView.trailingAnchor.constraint(equalTo: contentArea.trailingAnchor),
            manager.containerView.bottomAnchor.constraint(equalTo: contentArea.bottomAnchor),
        ])

        view.layoutSubtreeIfNeeded()
        manager.allPanels().forEach { $0.startIfNeeded() }
        manager.activePane.view.window?.makeFirstResponder(manager.activePane.focusTarget)

        refreshTabBar()
    }

    // MARK: - Split Operations

    func splitActivePane(orientation: SplitOrientation) {
        activePaneManager?.splitActive(orientation: orientation)
    }

    /// Tier 1.1 — proxy to active tab's PaneManager.focusNextPane. No-op
    /// when no tab is active (no panes to cycle).
    func focusNextPane(direction: Int = 1) {
        activePaneManager?.focusNextPane(direction: direction)
    }

    func splitActivePaneWithWebView(url: URL? = nil, orientation: SplitOrientation = .horizontal) {
        activePaneManager?.splitActiveWithWebView(url: url, orientation: orientation)
    }

    func closeActivePane() {
        activePaneManager?.closeActive()
    }

    // MARK: - Tab Bar

    func toggleTabBar(userInitiated: Bool = false) {
        if userInitiated { userToggledBar = true }
        isBarCollapsed.toggle()
        tabBar.setCollapsed(isBarCollapsed)
        refreshTabBar()
        eventBus?.broadcast(event: "tab.bar_toggled", data: ["collapsed": isBarCollapsed])
    }

    private func refreshTabBar() {
        let titles = paneManagers.map(\.activePane.currentTitle)
        let types: [TabPanelType] = paneManagers.map { m in
            m.activePane is WebViewController ? .webview : .terminal
        }
        tabBar.setTabs(titles: titles, types: types, activeIndex: activeIndex)
    }

    // MARK: - Config Hot-Reload

    /// Called when the config file changes at runtime. Applies theme and font to all
    /// running terminals. Background is re-applied only if the path/tint changed.
    /// Shell changes do not affect existing terminals — only new ones pick them up.
    func applyConfig(_ newConfig: CopadConfig, theme: CopadTheme) {
        // Update stored config/theme so tabs spawned AFTER hot-reload pick up the new values.
        config = newConfig
        self.theme = theme

        // Fan out to existing pane trees (theme/font/security; current zoom preserved).
        for paneManager in paneManagers {
            paneManager.applyConfig(newConfig, theme: theme)
        }

        // Background: apply/clear based on new config
        if let path = newConfig.backgroundPath {
            applyBackground(path: path, tint: newConfig.backgroundTint, opacity: newConfig.backgroundOpacity)
        } else if currentBackgroundPath != nil {
            clearBackground()
        } else {
            // Tint may have changed even if path stayed the same
            setTint(newConfig.backgroundTint)
        }

        // Update window background to match new theme
        view.window?.backgroundColor = theme.background.nsColor
    }

    // MARK: - Background

    func applyBackground(path: String, tint: Double, opacity: Double = 1.0) {
        currentBackgroundPath = path
        currentBackgroundTint = tint
        currentBackgroundOpacity = opacity
        paneManagers.forEach { $0.applyBackground(path: path, tint: tint, opacity: opacity) }
    }

    func clearBackground() {
        currentBackgroundPath = nil
        paneManagers.forEach { $0.clearBackground() }
    }

    func setTint(_ alpha: Double) {
        currentBackgroundTint = alpha
        paneManagers.forEach { $0.setTint(alpha) }
    }

    // MARK: - Socket Commands

    //
    // These dispatch through `activeTerminalPanel` so both backends
    // (SwiftTerm + alacritty) are handled identically. AppDelegate's
    // `resolveTerminalPanel` is the preferred path for any caller that
    // needs id-based panel lookup or Linux-style error reporting; these
    // are thin convenience wrappers for the "active terminal, no
    // id-resolution, no error reporting" case.

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
}
