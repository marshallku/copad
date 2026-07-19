import AppKit

/// Manages multiple tabs, each backed by a PaneManager (split-pane tree).
/// Panels can be terminals or webviews.
@MainActor
final class TabViewController: NSViewController {
    /// Mutable so config hot-reload affects panes spawned AFTER the reload (theme/font/security).
    /// Existing panes are updated separately via `applyConfig` fan-out.
    private var config: CopadConfig
    /// `private(set)` so AppDelegate's plugin-panel construction path can
    /// pass the active theme into PluginPanelController without piping it
    /// through every call site.
    private(set) var theme: CopadTheme

    private var tabBar: TabBarView!
    /// The tab bar's primary size constraint — height for horizontal
    /// (top/bottom) bars, width for vertical (left/right) bars. Held so
    /// collapse/expand can shrink a vertical bar to icon-only width.
    private var tabBarSizeConstraint: NSLayoutConstraint?

    /// Debounced v2 session persistence (decision #61 C4). Coalesces rapid
    /// structural mutations into one atomic write so a crash / forced kill
    /// loses at most the debounce window, not the whole session — the old
    /// path only saved on orderly `applicationWillTerminate`. This VC is the
    /// single writer. Suppressed during restore so replay doesn't re-save.
    private var sessionSaveTimer: Timer?
    private var suppressSessionSave = false

    /// Reopens a plugin panel by name on session restore (decision #61 slice 6).
    /// Set by AppDelegate, which owns the manifest store + action registry that
    /// plugin construction needs. nil-returning name → unavailable placeholder.
    var pluginFactory: ((_ name: String, _ restoreID: String) -> (any CopadPanel)?)?
    private var contentArea: NSView!
    /// Window-level background image (Phase 10b follow-up). One image
    /// fills the entire `contentArea` so splits share the same
    /// wallpaper instead of each pane duplicating it. Lazily created
    /// on first `applyBackground`. z-order: inserted below the
    /// PaneManager containerView so panes (with transparent
    /// default-bg cells via `setImageBackgroundActive`) overlay it.
    /// Layer-backed plain NSView (not `NSImageView`). `NSImageView`'s
    /// `imageScaling` doesn't include a CSS `cover`-equivalent mode —
    /// `.scaleAxesIndependently` (stretch) distorts the image,
    /// `.scaleProportionallyUpOrDown` (CSS `contain`) leaves letterbox
    /// bars. We want fill-and-crop. CALayer's `contentsGravity =
    /// .resizeAspectFill` is the exact equivalent of CSS `background-
    /// size: cover` — image scales up until it fully covers the view
    /// while preserving aspect ratio, overflow is clipped. Matches
    /// Linux's `gtk4::Picture::set_content_fit(ContentFit::Cover)`.
    private var backgroundView: NSView?
    private var tintView: NSView?
    /// Monotonic load token guards against a slow `NSImage(contentsOfFile:)`
    /// decode landing after a newer applyBackground/clearBackground
    /// took ownership of the visual state.
    private var backgroundLoadToken: UInt64 = 0
    /// Tier 4.2 — status bar at the bottom of the window. nil when
    /// `[statusbar] enabled = false`. Public so AppDelegate can wire
    /// it up post-launch (load modules from discovered plugin manifests
    /// + handle statusbar.show/hide/toggle socket commands).
    private(set) var statusBar: StatusBarView?
    // The ACTIVE workspace's live tabs. Inactive workspaces keep theirs in
    // `workspaces`, swapped in on `switchWorkspace` (decision #61 slice 7).
    private var paneManagers: [PaneManager] = []
    private(set) var activeIndex: Int = -1

    /// One workspace-session (decision #61 slice 7 — the owner's top-level
    /// "tab": a workspace dir + its own sub-tabs). The ACTIVE workspace's tabs
    /// are the live `paneManagers`; INACTIVE workspaces are parked as a v2
    /// `snapshot` and rebuilt on switch. Snapshot-and-rebuild (not parking live
    /// PaneManagers) so a background terminal exiting can't fire callbacks
    /// against the wrong workspace's live state; tmux-backed terminals reattach
    /// their surviving session on rebuild, so no process is lost on switch.
    private struct WorkspaceRuntime {
        let id: String
        var workspace: String?
        var snapshot: Session.WorkspaceSession?
    }

    private var workspaces: [WorkspaceRuntime] = [
        WorkspaceRuntime(id: "sess-0", workspace: nil, snapshot: nil),
    ]
    private var activeWorkspaceIndex = 0

    /// The active workspace's id / workspace dir (for the indicator + snapshot).
    var activeWorkspaceID: String { workspaces[activeWorkspaceIndex].id }
    var activeWorkspaceDir: String? { workspaces[activeWorkspaceIndex].workspace }
    var workspaceCount: Int { workspaces.count }
    /// (id, workspace, tabCount, isActive) for each workspace — drives the
    /// switcher UI + `workspace.list`.
    func workspaceSummaries() -> [(id: String, workspace: String?, tabs: Int, active: Bool)] {
        syncActiveWorkspace()
        return workspaces.enumerated().map { i, w in
            (w.id, w.workspace, w.snapshot?.subTabs.count ?? 0, i == activeWorkspaceIndex)
        }
    }

    /// Fold the live tabs into the active workspace's snapshot (call before any
    /// snapshot read or workspace switch).
    private func syncActiveWorkspace() {
        guard workspaces.indices.contains(activeWorkspaceIndex) else { return }
        let w = workspaces[activeWorkspaceIndex]
        workspaces[activeWorkspaceIndex].snapshot = buildWorkspaceSession(id: w.id, workspace: w.workspace)
    }

    /// Build a v2 WorkspaceSession from the LIVE tabs (`paneManagers`).
    private func buildWorkspaceSession(id: String, workspace: String?) -> Session.WorkspaceSession {
        var subTabs: [Session.SubTab] = []
        var activeSubId: String?
        for (idx, manager) in paneManagers.enumerated() {
            guard let root = manager.snapshotTreeV2() else { continue }
            let subID = "sub-\(subTabs.count)"
            if idx == activeIndex { activeSubId = subID }
            subTabs.append(Session.SubTab(
                id: subID,
                name: manager.customTabTitle(),
                root: root,
                focusedPaneId: manager.activePane.panelID,
            ))
        }
        if activeSubId == nil { activeSubId = subTabs.first?.id }
        return Session.WorkspaceSession(id: id, name: nil, workspace: workspace, subTabs: subTabs, activeSubTabId: activeSubId)
    }

    /// Number of sub-tabs (decision #61: the tabs of the current session).
    /// Exposed so the `opt+N` numeric jump only swallows the key when that
    /// sub-tab actually exists — otherwise `opt+5` with 3 sub-tabs should fall
    /// through to the terminal as a normal Option keystroke.
    var tabCount: Int { paneManagers.count }

    // Retained so new tabs inherit the current background state
    private(set) var currentBackgroundPath: String?
    private(set) var currentBackgroundTint: Double = 0.6
    private(set) var currentBackgroundOpacity: Double = 1.0
    // Whether `currentBackgroundPath` was picked from the wallpaper list
    // (rotation / `background.next` / `toggle`) rather than set manually or
    // via `[background] image`. `background.delete_current` only ever
    // deletes a list-picked image — matches Linux's `current.1` flag.
    private(set) var currentBackgroundFromList = false

    // Tab bar collapsed state.
    // Default: collapsed (icon-only). Auto-expands on 1→2 tab transition
    // unless the user has manually toggled the bar.
    private var isBarCollapsed: Bool = true
    private var userToggledBar: Bool = false

    /// Set by AppDelegate; propagated to all PaneManagers.
    weak var eventBus: EventBus? {
        didSet { paneManagers.forEach { $0.eventBus = eventBus } }
    }

    /// Invoked when the "+" popover's plugin-panel row is clicked. Kept
    /// as a callback (set by AppDelegate at launch) so TabViewController
    /// stays free of `ActionRegistry` / `EventBus` dependencies for
    /// plugin construction — AppDelegate owns those and runs the same
    /// path the `plugin.open` RPC takes. Closure is called on the main
    /// actor (the popover dispatch lives there). Mode mirrors the RPC's
    /// `mode` param ("tab" / "split_h" / "split_v").
    var onOpenPlugin: ((_ name: String, _ panelName: String, _ mode: AddPanelMode) -> Void)?

    var isTabBarCollapsed: Bool {
        isBarCollapsed
    }

    var activePaneManager: PaneManager? {
        paneManagers.indices.contains(activeIndex) ? paneManagers[activeIndex] : nil
    }

    /// Backend-agnostic terminal accessor — returns the active
    /// alacritty controller via the `TerminalCapable` protocol, or
    /// nil for webview / plugin panes. Used by socket `terminal.*`
    /// dispatch.
    var activeTerminalPanel: (any TerminalCapable)? {
        activePaneManager?.activeTerminalPanel()
    }

    var activeWebView: WebViewController? {
        activePaneManager?.activeWebView()
    }

    /// Polymorphic zoom dispatch — works for any pane that conforms
    /// to `Zoomable`. Returns nil for non-zoomable panes (webview,
    /// plugin) so the View → Zoom menu items become no-ops there.
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

    /// Per-pane snapshot for the agent cockpit — terminal panes only (agents run
    /// there; excludes webview/plugin/cockpit panels). `paneManagers` is private,
    /// so this is the public enumeration entry point. macOS has no
    /// `terminal.cwd_changed` event, so cwd is pulled from `reportedCwd` here at
    /// each refresh rather than tracked incrementally.
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

    /// Focus a pane by id from anywhere (cockpit click): switch to its tab, make
    /// it the active pane, and give it keyboard focus. Composes the primitives —
    /// there is no single such method otherwise.
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
        // When `[window] opacity < 1.0`, leave the root view's layer bg
        // clear so the non-opaque window's backgroundColor + blur view
        // (installed under contentView by AppDelegate) show through.
        // An opaque root would cover both, defeating the entire
        // window-transparency feature. Hot-reload handled in
        // `applyConfig`.
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
            // Restore focus to the active pane's keyboard view after
            // the tab bar field resigns. `panel.focusTarget` returns
            // the inner input view rather than the layout container.
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
                // Construction (PluginPanelController) and registry/eventBus
                // wiring live in AppDelegate where `openPluginPanel` mirrors
                // the `plugin.open` RPC handler. Errors are logged inside
                // that helper — popover dispatch stays fire-and-forget.
                onOpenPlugin?(name, panelName, mode)
            }
        }
        // Workspace switcher — the pull-down fetches the list lazily on open
        // (snapshot rebuild only happens then) and jumps / creates workspaces.
        tabBar.workspaceProvider = { [weak self] in
            guard let self else { return [] }
            return workspaceSummaries().enumerated().map { i, s in
                TabBarView.WorkspaceMenuItem(
                    title: Self.workspaceDisplayName(dir: s.workspace, index: i),
                    subtitle: s.workspace,
                    active: s.active,
                )
            }
        }
        tabBar.onSwitchWorkspace = { [weak self] i in self?.switchWorkspace(to: i) }
        tabBar.onNewWorkspace = { [weak self] in self?.newWorkspaceInteractive() }
        root.addSubview(tabBar)

        contentArea = NSView()
        contentArea.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(contentArea)

        // Tier 4.2 — status bar pinned to either the top or bottom of
        // the root view, depending on `[statusbar] position`. Tab bar
        // + content area then anchor against whichever edge of the
        // status bar faces inward (so they never overlap the bar).
        // Linux supports the same two values. left/right deferred —
        // status bar is currently a horizontal NSStackView so vertical
        // would need a separate layout pass.
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

        // Tier 1.4 — tabs position. Horizontal (top/bottom): a full-width
        // bar with a fixed height, content fills the rest vertically.
        // Vertical (left/right): a full-height bar with a fixed width
        // (`[tabs] width`, shrunk to icon-only when collapsed) pinned to
        // the leading/trailing edge, content fills the rest horizontally.
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

    // MARK: - v2 session persistence (decision #61)

    // slice 3d bridges the existing v1 snapshot/restore to the v2 model rather
    // than rewriting PaneManager: the runtime tree stays SplitSnap and is
    // converted to/from the v2 SessionFileV2 here. Today's runtime tabs are
    // terminal split trees, so every tab maps to one sub-tab holding a Terminal
    // pane tree; multi-session, workspaces, and webview/plugin panes arrive in
    // later slices. Stable pane ids are informational until slice 4 wires tmux
    // reattach — v1 pixel divider positions become an even 0.5 ratio (macOS
    // already re-equalizes on restore, decision #61 I4).

    /// Snapshot ALL workspace-sessions as a v2 document (decision #61 slice 7):
    /// the active one from its live tabs, the rest from their parked snapshots.
    /// Each terminal leaf carries its panelID + tmux_ref so restore reattaches.
    func snapshotSessionV2() -> Session.SessionFileV2 {
        syncActiveWorkspace()
        let sessions = workspaces.compactMap(\.snapshot)
        let activeId = workspaces.indices.contains(activeWorkspaceIndex)
            ? workspaces[activeWorkspaceIndex].id
            : sessions.first?.id
        return Session.SessionFileV2(version: Session.versionV2, sessions: sessions, activeSessionId: activeId)
    }

    /// Debounced persistence trigger — call after any structural mutation
    /// (new/close/switch tab, rename). Coalesces bursts into one atomic write
    /// ~800ms later on the main run loop. No-op while restoring.
    func scheduleSessionSave() {
        guard !suppressSessionSave else { return }
        sessionSaveTimer?.invalidate()
        sessionSaveTimer = Timer.scheduledTimer(withTimeInterval: 0.8, repeats: false) { [weak self] _ in
            // The timer fires on the main run loop, so the main-actor snapshot
            // is safe; assumeIsolated makes that explicit for the checker.
            MainActor.assumeIsolated {
                guard let self else { return }
                let snap = self.snapshotSessionV2()
                if snap.sessions.allSatisfy(\.subTabs.isEmpty) {
                    Session.clear()
                } else {
                    Session.saveV2(snap)
                }
            }
        }
    }

    /// Restore ALL workspace-sessions from a v2 document (decision #61 slice 7);
    /// the active workspace's tabs are rebuilt live, the rest are parked as
    /// snapshots and rebuilt on switch. Empty document → caller falls back to
    /// `openInitialTab`.
    func restoreSessionV2(_ file: Session.SessionFileV2) {
        suppressSessionSave = true
        defer {
            suppressSessionSave = false
            // Re-persist once: a just-migrated v1 file lands as v2, and because
            // panes are rebuilt with their PERSISTED ids this save keeps the
            // same ids + tmux_ref rather than minting fresh ones.
            scheduleSessionSave()
        }
        guard !file.sessions.isEmpty else { return }
        workspaces = file.sessions.map {
            WorkspaceRuntime(id: $0.id, workspace: $0.workspace, snapshot: $0)
        }
        activeWorkspaceIndex = file.sessions.firstIndex { $0.id == file.activeSessionId } ?? 0
        rebuildLiveTabs(from: file.sessions[activeWorkspaceIndex])
    }

    /// Rebuild the LIVE tabs (`paneManagers`) from a workspace-session's
    /// sub-tabs, reusing pane ids so tmux-backed terminals reattach. Assumes
    /// live tabs were torn down. Callers keep `suppressSessionSave` on.
    private func rebuildLiveTabs(from session: Session.WorkspaceSession) {
        for sub in session.subTabs {
            let initial = PaneManager.panelFromPane(PaneManager.leftmostPane(sub.root), config: config, theme: theme, pluginFactory: pluginFactory)
            let manager = PaneManager(config: config, theme: theme, initialPanel: .pluginPanel(initial))
            manager.pluginFactory = pluginFactory
            addTab(manager: manager)
            manager.restoreSplitsV2(into: manager.activePane, from: sub.root)
            if let title = sub.name {
                manager.setCustomTitle(title)
            }
            // Restore the focused pane so the post-restore save doesn't churn it
            // (decision #61 I3). Falls back to the leftmost pane when absent.
            if let fid = sub.focusedPaneId,
               let panel = manager.allPanels().first(where: { $0.panelID == fid })
            {
                manager.setActive(panel)
            }
        }
        if let activeId = session.activeSubTabId,
           let idx = session.subTabs.firstIndex(where: { $0.id == activeId }),
           paneManagers.indices.contains(idx)
        {
            switchTab(to: idx)
        }
        // `switchTab` early-returns when the target sub-tab is already active
        // (addTab selected the last one), so it may skip `makeFirstResponder`.
        if let active = activePaneManager {
            active.activePane.view.window?.makeFirstResponder(active.activePane.focusTarget)
        }
    }

    // MARK: - Workspace sessions (decision #61 slice 7)

    private func tearDownLiveTabs() {
        activePaneManager?.containerView.removeFromSuperview()
        paneManagers.removeAll()
        activeIndex = -1
    }

    /// Switch to another workspace-session: park the current live tabs as a
    /// snapshot, tear them down, and rebuild the target from its snapshot
    /// (tmux-backed terminals reattach; nothing is lost).
    func switchWorkspace(to index: Int) {
        guard index != activeWorkspaceIndex, workspaces.indices.contains(index) else { return }
        syncActiveWorkspace()
        suppressSessionSave = true
        tearDownLiveTabs()
        activeWorkspaceIndex = index
        if let snap = workspaces[index].snapshot, !snap.subTabs.isEmpty {
            rebuildLiveTabs(from: snap)
        } else {
            // Empty workspace → seed a terminal in its declared directory.
            newTerminalTab(cwd: workspaces[index].workspace, initialInput: nil)
        }
        suppressSessionSave = false
        scheduleSessionSave()
        eventBus?.broadcast(event: "workspace.switched", data: ["id": workspaces[index].id])
    }

    /// Create a new workspace-session (seeded with one terminal opened in the
    /// workspace directory) and switch to it.
    @discardableResult
    func newWorkspace(workspace: String?) -> String {
        syncActiveWorkspace()
        suppressSessionSave = true
        tearDownLiveTabs()
        let id = UUID().uuidString
        workspaces.append(WorkspaceRuntime(id: id, workspace: workspace, snapshot: nil))
        activeWorkspaceIndex = workspaces.count - 1
        suppressSessionSave = false
        newTerminalTab(cwd: workspace, initialInput: nil)  // open in the workspace dir
        scheduleSessionSave()
        eventBus?.broadcast(event: "workspace.switched", data: ["id": id])
        return id
    }

    /// Switch by workspace id (for `workspace.switch`).
    @discardableResult
    func switchWorkspace(id: String) -> Bool {
        guard let idx = workspaces.firstIndex(where: { $0.id == id }) else { return false }
        switchWorkspace(to: idx)
        return true
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
            self?.scheduleSessionSave()  // split / focus changes alter the tree
        }
        manager.onPaneAdded = { [weak self] panel in
            // Fan window-level state onto the new pane. Right now
            // that's just background-active gate so default-bg cells
            // stay transparent and the shared wallpaper shows
            // through. Future runtime state (per-pane theme, etc.)
            // would route through here too.
            guard let self else { return }
            if currentBackgroundPath != nil {
                panel.applyBackground(path: "", tint: 0, opacity: 0)
            }
            scheduleSessionSave()  // a split added a pane
        }
        manager.onLayoutChanged = { [weak self] in
            self?.scheduleSessionSave()  // a pane closed (incl. non-active)
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
            syncVerticalBarWidth()
        }

        switchTab(to: tabIndex)
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
        scheduleSessionSave()  // active sub-tab changed
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
        syncVerticalBarWidth()
        refreshTabBar()
        eventBus?.broadcast(event: "tab.bar_toggled", data: ["collapsed": isBarCollapsed])
    }

    /// Vertical (left/right) bars carry their collapsed/expanded state in
    /// the bar *width* (horizontal bars keep a fixed height and only swap
    /// tab-pill widths, handled inside `TabBarView`). Re-point the stored
    /// width constraint whenever `isBarCollapsed` changes. No-op for
    /// horizontal bars.
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
        tabBar.setActiveWorkspace(
            name: Self.workspaceDisplayName(dir: activeWorkspaceDir, index: activeWorkspaceIndex),
        )
    }

    /// Short label for a workspace: the directory's last path component, or
    /// `session N` when the workspace has no declared directory.
    static func workspaceDisplayName(dir: String?, index: Int) -> String {
        if let dir, let name = dir.split(separator: "/").last, !name.isEmpty {
            return String(name)
        }
        return "session \(index + 1)"
    }

    /// ⌘⇧W / switcher menu → tell the tab bar to drop its workspace list.
    func showWorkspaceMenu() { tabBar.showWorkspaceMenu() }

    /// Cycle to the next workspace (wraps) — param-free, so it works from the
    /// command palette and `[keybindings]`.
    func cycleWorkspace() {
        guard workspaceCount > 1 else { return }
        switchWorkspace(to: (activeWorkspaceIndex + 1) % workspaceCount)
    }

    /// "New Workspace…" — ask for a directory, then open a workspace seeded
    /// with a terminal in it. Cancelling the picker is a no-op. Public so the
    /// param-free `workspace.new` (command palette) routes here instead of
    /// silently creating a directory-less workspace.
    func newWorkspaceInteractive() { promptNewWorkspace() }

    private func promptNewWorkspace() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "New Workspace"
        panel.message = "Choose a directory for the new workspace"
        if let dir = activeWorkspaceDir { panel.directoryURL = URL(fileURLWithPath: dir) }
        let complete: (NSApplication.ModalResponse) -> Void = { [weak self] response in
            guard response == .OK, let path = panel.url?.path else { return }
            self?.newWorkspace(workspace: path)
        }
        if let window = view.window {
            panel.beginSheetModal(for: window, completionHandler: complete)
        } else {
            complete(panel.runModal())
        }
    }

    // MARK: - Config Hot-Reload

    /// Called when the config file changes at runtime. Applies theme and font to all
    /// running terminals. Background is re-applied only if the path/tint changed.
    /// Shell changes do not affect existing terminals — only new ones pick them up.
    func applyConfig(_ newConfig: CopadConfig, theme: CopadTheme) {
        // Update stored config/theme so tabs spawned AFTER hot-reload pick up the new values.
        config = newConfig
        self.theme = theme

        // Root view bg follows window opacity — opaque blocks both the
        // semi-transparent window backgroundColor and the blur view
        // installed beneath it. Same conditional as initial loadView.
        view.layer?.backgroundColor = Self.rootBg(theme: theme, opacity: newConfig.windowOpacity)

        // Chrome: rebuild bg colors for new theme + window opacity.
        // Pills stay opaque (read inside their own draw paths); only the
        // outer bar bgs pick up the alpha.
        tabBar?.applyWindowOpacity(newConfig.windowOpacity, theme: theme)
        statusBar?.applyWindowOpacity(newConfig.windowOpacity, theme: theme)

        // Fan out to existing pane trees (theme/font/security; current zoom preserved).
        for paneManager in paneManagers {
            paneManager.applyConfig(newConfig, theme: theme)
        }

        // Re-scale the window-level bg / tint alpha against the new
        // window.opacity. Pane-level applyWindowOpacity used to do
        // this per-pane; image is window-owned now.
        refreshBackgroundForWindowOpacity()

        // Background: apply/clear based on new config
        if let path = newConfig.backgroundPath {
            applyBackground(path: path, tint: newConfig.backgroundTint, opacity: newConfig.backgroundOpacity)
        } else if currentBackgroundPath != nil, !currentBackgroundFromList {
            clearBackground()
        } else {
            // No config image, or a rotated wallpaper we must preserve (a
            // reload that only touched tint/opacity/interval must not wipe
            // a rotation pick — matches Linux apply_config's showing_list_
            // image guard). Only the tint knob may have changed.
            setTint(newConfig.backgroundTint)
        }

        // Update window background to match new theme
        view.window?.backgroundColor = theme.background.nsColor
    }

    // MARK: - Background

    /// Window-level background. Renders one image across the whole
    /// `contentArea` (under all splits within the active tab) so
    /// splits no longer duplicate the wallpaper. Async decode +
    /// monotonic load token to drop stale loads on a fast
    /// path-change. Image alpha + tint alpha are scaled by the
    /// configured `window.opacity` so the desktop bleeds through
    /// when the user opts into window transparency.
    func applyBackground(path: String, tint: Double, opacity: Double = 1.0, fromList: Bool = false) {
        currentBackgroundPath = path
        currentBackgroundTint = tint
        currentBackgroundOpacity = opacity
        currentBackgroundFromList = fromList
        ensureBackgroundViews()
        backgroundLoadToken &+= 1
        let token = backgroundLoadToken
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let image = NSImage(contentsOfFile: path)
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                // Stale: newer applyBackground / clearBackground won
                // the race; drop this decode.
                guard token == backgroundLoadToken else { return }
                guard let image else { return }
                // NSImage → CGImage so the layer can render it under
                // its `contentsGravity` rule. `forProposedRect: nil`
                // asks for the image's natural representation; aspect-
                // fill scaling happens in CoreAnimation, not in the
                // bitmap, so passing the original-size CGImage is
                // correct (and cheaper than pre-rasterizing at view
                // size). Skip the assignment if conversion fails —
                // we don't want to wipe a previously-good image.
                guard let cgImage = image.cgImage(
                    forProposedRect: nil,
                    context: nil,
                    hints: nil,
                ) else { return }
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

    /// `window.opacity` hot-reload (called from AppDelegate config
    /// watcher path). Re-scales image + tint alpha so the user-knob
    /// stays load-bearing on top of an active background image.
    func refreshBackgroundForWindowOpacity() {
        let windowOpacity = currentWindowOpacity()
        backgroundView?.alphaValue = CGFloat(currentBackgroundOpacity * windowOpacity)
        tintView?.layer?.backgroundColor = NSColor.black
            .withAlphaComponent(CGFloat(currentBackgroundTint * windowOpacity)).cgColor
    }

    /// Lazily insert the bg + tint views under `contentArea`. Both
    /// pinned to the contentArea edges so they fill the entire pane
    /// area regardless of split layout.
    private func ensureBackgroundViews() {
        if backgroundView != nil { return }
        // Use autoresizing (not Auto Layout). NSImageView reported its
        // intrinsic content size as the image's pixel dimensions even
        // with explicit edge constraints, and that fittingSize
        // propagated up to the window. Switching to a plain layer-
        // backed NSView side-steps the issue entirely — plain NSView
        // has no intrinsic size. The aspect-fill rendering moves to
        // the layer (`contentsGravity = .resizeAspectFill`, set
        // below), matching Linux's `gtk4::Picture` Cover content fit.
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

    /// Walk every pane in every tab and flip its renderer's
    /// transparent-default-bg gate. Called when the window-level
    /// background image appears / disappears.
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

    /// Read the current `window.opacity` from the live config. The
    /// active config lives on each PaneManager (since it gets hot-
    /// reloaded there) — any pane's value is authoritative because
    /// they all share it.
    private func currentWindowOpacity() -> Double {
        paneManagers.first?.configWindowOpacity() ?? 1.0
    }

    // MARK: - Socket Commands

    //
    // These dispatch through `activeTerminalPanel`.
    // AppDelegate's `resolveTerminalPanel` is the preferred path for
    // any caller that needs id-based panel lookup or Linux-style error
    // reporting; these are thin convenience wrappers for the "active
    // terminal, no id-resolution, no error reporting" case.

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

    /// Pick the root view layer bg. `.clear` when opacity < 1.0 so the
    /// non-opaque window's backgroundColor and any blur view installed
    /// by AppDelegate show through; otherwise opaque theme.background.
    private static func rootBg(theme: CopadTheme, opacity: Double) -> CGColor {
        opacity < 1.0 ? NSColor.clear.cgColor : theme.background.nsColor.cgColor
    }
}
