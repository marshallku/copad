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
    /// Removal tokens for the controller-scoped notification observers.
    private var globalObservers: [any NSObjectProtocol] = []
    private var suppressSessionSave = false

    /// Reopens a plugin panel by name on session restore (decision #61 slice 6).
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
    /// Where the docked notification panel takes room from the terminal; zero while it floats
    /// over the terminal instead (a window too narrow for both panels and 40 columns).
    private var comuxNotifSlot: NSView!
    private var comuxNotifSlotWidth: NSLayoutConstraint!
    /// Closes the floating notification panel on Esc or a click outside it.
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
    private var paneManagers: [PaneManager] = []
    private(set) var activeIndex: Int = -1

    /// Reopens the agent cockpit pane on restore (a `.cockpit` leaf). Injected by
    /// AppDelegate, which owns the cockpit model. nil → placeholder terminal.
    var cockpitFactory: (() -> (any CopadPanel)?)?

    /// Number of tabs. Exposed so the `cmd+shift+N` numeric jump only swallows the
    /// key when that tab actually exists — otherwise `cmd+shift+5` with 3 tabs
    /// should fall through as a normal keystroke.
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

    /// Panel id of the focused pane in the active tab, for diagnostics and the
    /// focus audit.
    var activePaneID: String? {
        activePaneManager?.activePane.panelID
    }

    /// True when ANY webview in this window is in protected mode.
    ///
    /// Protection freezes the whole profile, not one tab: a concurrent tab on
    /// the same profile is another window onto the same shared storage, so
    /// freezing one while its sibling stays scriptable would be a boundary with
    /// a door in it. Panes are single-profile until work unit B3, so "this
    /// window" and "this profile" are the same set today.
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
        root.addSubview(tabBar)

        contentArea = NSView()
        contentArea.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(contentArea)
        installComuxChrome()

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

    /// Build a wire snapshot of every live tab (v3 — decision #64): the tab list
    /// plus each tab's split tree of typed panes with normalized divider ratios.
    /// Terminals carry only their cwd; process/scrollback is the user's own tmux.
    func snapshotSession() -> Session.Snapshot {
        var tabs: [Session.TabSnap] = []
        for manager in paneManagers {
            guard let root = manager.snapshotTree() else { continue }
            tabs.append(Session.TabSnap(customTitle: manager.customTabTitle(), root: root))
        }
        let clamped = max(0, min(activeIndex, tabs.count - 1))
        return Session.Snapshot(version: Session.version, tabs: tabs, currentTab: clamped)
    }

    /// Reclaim history blobs no longer referenced by a COMMITTED snapshot.
    ///
    /// Called only after `Session.save` returns. Running it against a set
    /// gathered before the write would delete the very blob the new session
    /// document points at — the reason blob generations are immutable in the
    /// first place is that these two writes are not atomic together.
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

    /// Build tabs + splits to mirror `snap` (v3). Restored panels start fresh —
    /// only cwd + layout + split ratios are replayed, not process/scrollback.
    /// Caller falls back to `openInitialTab` if `snap` has zero tabs.
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

    /// Debounced persistence trigger (v3) — call after any structural mutation
    /// (new/close/switch tab, split, rename). Coalesces bursts into one atomic
    /// write ~800ms later on the main run loop. No-op while restoring.
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
        let sidebar = ChromeHostingView(rootView: ComuxSidebar(host: host, settle: settle, toggleNotifications: { [weak self] in
            self?.comuxNotificationsOpen.toggle()
            self?.refreshComuxChrome()
        }))
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
        for v in [slot, sidebar, notifications, status, card, grip] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            contentArea.addSubview(v)
        }
        comuxSidebar = sidebar
        comuxNotifications = notifications
        comuxStatus = status
        comuxNotifSlot = slot
        comuxStatusHeight = status.heightAnchor.constraint(equalToConstant: 0)
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
            notifications.trailingAnchor.constraint(equalTo: contentArea.trailingAnchor),
            notifications.topAnchor.constraint(equalTo: contentArea.topAnchor),
            notifications.bottomAnchor.constraint(equalTo: status.topAnchor),
            comuxNotificationsWidth,
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
        layoutComuxChrome()
        presentComuxOverlay()
    }

    /// Panel sizes follow the terminal font (decisions #178) within a budget of the window: the
    /// sidebar takes at most 30% of it, the notification panel 35%, and when the two would
    /// leave the terminal under 40 columns the notification panel floats over it instead.
    private func layoutComuxChrome() {
        guard comuxSidebar != nil else { return }
        let on = comuxWindow.state != nil
        let st = comuxWindow.style
        let width = contentArea.bounds.width
        let natural = min(max(30 * st.advance, 200), max(0.3 * width, 160))
        // A width the user dragged to (decisions #181) wins, within what leaves the terminal usable.
        let dragged = UserDefaults.standard.double(forKey: Self.comuxSidebarWidthKey)
        let side = on ? (dragged > 0 ? min(max(dragged, 160), max(0.6 * width, 160)) : natural).rounded() : 0
        let notifOpen = on && comuxNotificationsOpen
        let notif = notifOpen ? min(max(38 * st.advance, 260), max(0.35 * width, 220)).rounded() : 0
        // Always over the terminal, never beside it: opening it must not reflow the panes, and
        // opening a notification closes it anyway.
        let floating = notifOpen
        set(comuxSidebarWidth, side)
        set(comuxCardWidth, on ? (72 * st.advance + 40).rounded() : 0)
        set(comuxStatusHeight, on ? st.statusHeight : 0)
        set(comuxNotificationsWidth, notif)
        set(comuxNotifSlotWidth, floating ? 0 : notif)
        comuxNotifications.isHidden = !notifOpen
        if comuxWindow.notificationsFloating != floating { comuxWindow.notificationsFloating = floating }
        watchFloatingNotifications(floating)
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
                // The sidebar (whose bell toggles this panel) is not "outside": closing here
                // and then running the bell's toggle would reopen it.
                let inside = [self.comuxNotifications, self.comuxSidebar].contains { v in
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
        guard comuxOverlayShown == nil, let o = mine else { return }
        settleComuxInput()
        comuxOverlayShown = o.overlay
        if case let .menu(_, items) = o.spec {
            popUpComuxMenu(o.overlay, items: items)
            return
        }
        let layer = NSHostingView(rootView: ComuxOverlayHost(host: host, id: o.overlay) { [weak self] in
            self?.closeComuxOverlay()
        })
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

    /// Commit the focused terminal's IME composition before a panel click moves comux's view,
    /// so a half-typed syllable is not delivered to the pane comux switches to.
    private func settleComuxInput() {
        if let client = activePaneManager?.activePane.focusTarget as? NSTextInputClient,
           client.hasMarkedText() {
            client.unmarkText()
        }
    }

    /// Notification observers that are per-CONTROLLER, not per-tab.
    ///
    /// These used to be registered inside `addTab`, which meant one extra
    /// observer for every tab ever opened — none of them removed when a tab
    /// closed. Every navigation then enqueued a growing number of main-actor
    /// tasks and timer resets, quietly making the app slower the longer it ran.
    /// Registered once here, with the tokens kept so `deinit` can remove them.
    private func registerGlobalObserversIfNeeded() {
        guard globalObservers.isEmpty else { return }
        globalObservers = [
            NotificationCenter.default.addObserver(
                forName: .terminalTitleChanged, object: nil, queue: .main,
            ) { [weak self] _ in
                Task { @MainActor in self?.refreshTabBar() }
            },
            // A browser pane's URL is part of the persisted layout, so a
            // navigation must schedule a save the same way a split or a tab
            // switch does. Without this, the only URL that ever reached disk
            // was whatever the pane happened to show at the next structural
            // change — and a pane opened blank and then navigated persisted
            // nothing at all.
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

    func scheduleSessionSave() {
        guard !suppressSessionSave else { return }
        sessionSaveTimer?.invalidate()
        sessionSaveTimer = Timer.scheduledTimer(withTimeInterval: 0.8, repeats: false) { [weak self] _ in
            // The timer fires on the main run loop, so the main-actor snapshot
            // is safe; assumeIsolated makes that explicit for the checker.
            MainActor.assumeIsolated {
                guard let self else { return }
                let snap = self.snapshotSession()
                if snap.tabs.isEmpty {
                    Session.clear()
                } else if Session.save(snap) {
                    // Only after a save that COMMITTED. Reclaiming against a
                    // snapshot that never reached disk would delete the
                    // generations the previously committed session still points
                    // at — turning a failed write into lost history.
                    Self.gcHistoryBlobs(after: snap)
                }
            }
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

    /// `background: true` creates the copad tab without switching to it, so an
    /// agent opening a page does not yank the user out of what they were
    /// looking at.
    func newWebViewTab(url: URL? = nil, background: Bool = false) {
        let manager = PaneManager(config: config, theme: theme, initialPanel: .webview(url: url))
        addTab(manager: manager, background: background)
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

        registerGlobalObserversIfNeeded()

        manager.eventBus = eventBus
        paneManagers.append(manager)
        let tabIndex = paneManagers.count - 1

        // Auto-expand when going from 1 to 2 tabs (unless user manually toggled)
        if paneManagers.count == 2, isBarCollapsed, !userToggledBar {
            isBarCollapsed = false
            tabBar.setCollapsed(false)
            syncVerticalBarWidth()
        }

        if background {
            // `switchTab` is what STARTS a tab's panes, so skipping it left a
            // background browser pane with an empty tab array: page commands
            // answered `tab_closed` and autosave persisted an empty URL instead
            // of the destination that was asked for. Start it here without
            // selecting it — which is the whole point of `background`.
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

        // Under the comux chrome, so a floating notification panel stays on top of it.
        contentArea.addSubview(manager.containerView, positioned: .below, relativeTo: comuxNotifSlot)
        NSLayoutConstraint.activate([
            manager.containerView.topAnchor.constraint(equalTo: contentArea.topAnchor),
            // Between the comux panels, which are zero-width unless this window hosts a comux.
            manager.containerView.leadingAnchor.constraint(equalTo: comuxSidebar.trailingAnchor),
            manager.containerView.trailingAnchor.constraint(equalTo: comuxNotifSlot.leadingAnchor),
            manager.containerView.bottomAnchor.constraint(equalTo: comuxStatus.topAnchor),
        ])

        view.layoutSubtreeIfNeeded()
        // The tab's split views now have a real size — apply any persisted
        // divider ratios that couldn't be set while the tab was off-screen.
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

    /// Tier 1.1 — proxy to active tab's PaneManager.focusNextPane. No-op
    /// when no tab is active (no panes to cycle).
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
        comuxWindow.style = ComuxStyle.make(config: newConfig, theme: theme)
        layoutComuxChrome()

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
        // Read the screen geometry HERE: AppKit is main-thread-only, and the decode below
        // runs on a background queue.
        let envelope = Self.backingPixelEnvelope()
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let cgImage = Self.decodeBackground(path: path, envelope: envelope)
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                // Stale: newer applyBackground / clearBackground won
                // the race; drop this decode.
                guard token == backgroundLoadToken else { return }
                // Skip the assignment if the decode failed — we don't want to wipe a
                // previously-good image.
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
    /// `.resizeAspectFill`.
    ///
    /// The old path was `NSImage(contentsOfFile:)` + `cgImage(forProposedRect: nil)`, which
    /// hands CoreAnimation the image at its NATURAL size and lets the layer scale it. That is
    /// correct, and it is also why a wallpaper directory of 6000x4000 camera JPEGs cost ~96 MB
    /// of RGBA each once resident — the dominant term in this process's memory. Nothing leaked
    /// (one `backgroundView` per window, and replacing `contents` releases the old image); the
    /// bitmap was simply far larger than any screen could show.
    ///
    /// Sizing is aspect-fill-aware, NOT a plain longest-side cap. Capping the long side of a
    /// 4000x6000 portrait at a 3440-wide screen yields 2293x3440, and filling a 3440x1440 view
    /// with that upscales it — despite the source having pixels to spare. So the scale is
    /// computed the way the layer will actually use it (`max(W/Iw, H/Ih)`, clamped to 1 so a
    /// small image is never enlarged) and only then turned back into a longest-side bound.
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
            // Apply EXIF orientation. `NSImage` used to do this for us; dropping it would
            // silently rotate every photo shot in portrait.
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

/// Records which comux menu item was picked (NSMenu actions need an object target).
private final class ComuxMenuPicker: NSObject {
    var picked: String?
    @objc func pick(_ sender: NSMenuItem) { picked = sender.representedObject as? String }
}

/// A thin strip on the comux sidebar's right edge: the resize cursor over it, and dragging it sets
/// the sidebar's width (decisions #181).
final class ComuxResizeGrip: NSView {
    var onDrag: ((CGFloat) -> Void)?
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
        onDrag?(max(120, start.width + e.locationInWindow.x - start.x))
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
