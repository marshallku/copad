import AppKit
import CopadCore
@preconcurrency import WebKit

// MARK: - WebViewController

/// A browser pane: a tab strip, a toolbar, and one visible `BrowserTab`. Per-page state lives on
/// `BrowserTab`. Inactive tabs stay alive outside the view hierarchy; `takeSnapshot` renders
/// correctly for a web view in no window, so they need no hidden-window trick.
@MainActor
final class WebViewController: NSViewController, CopadPanel {
    let panelID: String

    private(set) var tabs: [BrowserTab] = []
    private(set) var activeIndex = 0

    var activeTab: BrowserTab { tabs[activeIndex] }

    var webView: WKWebView! { tabs.indices.contains(activeIndex) ? tabs[activeIndex].webView : nil }

    /// Focus target for `panel.focusTarget`: the WKWebView is the real keyboard receiver.
    var focusTarget: NSView { webView ?? view }

    private(set) var currentTitle: String = "Web"

    private var urlField: NSTextField!
    private var backButton: NSButton!
    private var forwardButton: NSButton!
    private var reloadButton: NSButton!
    private var tabStrip: NSStackView!
    private var tabStripScroll: NSScrollView!
    private var webContainer: NSView!
    private var webConstraints: [NSLayoutConstraint] = []

    weak var eventBus: EventBus?

    /// `[browser] capture_bodies`. Off by default: bodies are the likeliest place for a secret.
    var captureBodies = false

    /// Held until `startIfNeeded` so the pane's tabs are built in one place.
    private var pendingRestore: BrowserPaneSnap?
    private var pendingInitialURL: URL?
    private var started = false

    // MARK: - Lifecycle

    init(url: URL? = nil, restoreID: String? = nil, pane: BrowserPaneSnap? = nil) {
        panelID = restoreID ?? UUID().uuidString
        pendingInitialURL = url
        // A restored pane arrives through `BrowserFFI.normalize`, which already clamped `active`
        // and dropped ids failing the Rust charset rule; do not validate again.
        pendingRestore = (pane?.tabs.isEmpty == false) ? pane : nil
        super.init(nibName: nil, bundle: nil)
        WebViewController.livePanes.add(self)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    override func loadView() {
        let back = makeToolbarButton(symbol: "chevron.left", tooltip: "Back", action: #selector(backTapped))
        let forward = makeToolbarButton(symbol: "chevron.right", tooltip: "Forward", action: #selector(forwardTapped))
        let reload = makeToolbarButton(symbol: "arrow.clockwise", tooltip: "Reload", action: #selector(reloadTapped))
        let devtools = makeToolbarButton(symbol: "wrench.and.screwdriver", tooltip: "DevTools", action: #selector(devtoolsTapped))
        let newTab = makeToolbarButton(symbol: "plus", tooltip: "New tab", action: #selector(newTabTapped))
        back.isEnabled = false
        forward.isEnabled = false
        backButton = back
        forwardButton = forward
        reloadButton = reload

        let field = NSTextField()
        field.placeholderString = "Enter URL or search…"
        field.bezelStyle = .roundedBezel
        field.font = .systemFont(ofSize: 12)
        field.usesSingleLineMode = true
        field.lineBreakMode = .byTruncatingTail
        field.cell?.sendsActionOnEndEditing = false
        field.target = self
        field.action = #selector(urlFieldSubmit(_:))
        field.translatesAutoresizingMaskIntoConstraints = false
        urlField = field

        let toolbar = NSStackView(views: [back, forward, reload, field, newTab, devtools])
        toolbar.orientation = .horizontal
        toolbar.spacing = 4
        toolbar.edgeInsets = NSEdgeInsets(top: 4, left: 8, bottom: 4, right: 8)
        toolbar.translatesAutoresizingMaskIntoConstraints = false

        // Scrolls horizontally: an unreadable 12-pixel chip is worse than one that scrolls.
        let strip = NSStackView()
        strip.orientation = .horizontal
        strip.spacing = 4
        strip.edgeInsets = NSEdgeInsets(top: 0, left: 6, bottom: 0, right: 6)
        strip.translatesAutoresizingMaskIntoConstraints = false
        tabStrip = strip

        let scroll = NSScrollView()
        scroll.hasHorizontalScroller = false
        scroll.hasVerticalScroller = false
        scroll.drawsBackground = false
        scroll.documentView = strip
        scroll.translatesAutoresizingMaskIntoConstraints = false
        tabStripScroll = scroll

        let container = NSView()
        let host = NSView()
        host.translatesAutoresizingMaskIntoConstraints = false
        webContainer = host

        container.addSubview(scroll)
        container.addSubview(toolbar)
        container.addSubview(host)

        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: container.topAnchor),
            scroll.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            scroll.heightAnchor.constraint(equalToConstant: 28),
            strip.heightAnchor.constraint(equalTo: scroll.heightAnchor),

            toolbar.topAnchor.constraint(equalTo: scroll.bottomAnchor),
            toolbar.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            toolbar.trailingAnchor.constraint(equalTo: container.trailingAnchor),

            host.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
            host.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            host.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            host.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])

        view = container
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        // Only when the pane opened empty; a programmatic creation must not steal the keyboard.
        if tabs.count == 1, activeTab.snapshotSourceURL.isEmpty,
           urlField?.stringValue.isEmpty == true
        {
            view.window?.makeFirstResponder(urlField)
        }
    }

    // MARK: - CopadPanel

    func startIfNeeded() {
        guard !started else { return }
        started = true

        if let restore = pendingRestore {
            for snap in restore.tabs {
                tabs.append(BrowserTab(id: snap.id, url: nil, restored: snap, owner: self))
            }
            activeIndex = min(restore.active, tabs.count - 1)
        } else {
            tabs.append(BrowserTab(
                id: BrowserSnapshot.freshTabID(),
                url: pendingInitialURL,
                restored: nil,
                owner: self,
            ))
            activeIndex = 0
        }
        pendingRestore = nil
        pendingInitialURL = nil

        // Every tab loads, not just the visible one: a background tab that loaded on first
        // selection would have no history blob to save and come back as a bare URL.
        for tab in tabs { tab.startIfNeeded() }
        showActiveTab()
        refreshTabStrip()
    }

    func applyBackground(path _: String, tint _: Double, opacity _: Double) {}
    func clearBackground() {}
    func setTint(_: Double) {}

    // MARK: - Tabs

    /// Open a tab. `background: true` leaves the user where they are; agent-driven opens must not
    /// replace the visible page. Mirrors `copad_core::browser::tabs::MAX_TABS_PER_PANE`, enforced
    /// at creation as well as restore: otherwise tab 101 would work and vanish after a restart,
    /// when core's normalizer truncates the list.
    static let maxTabs = 100

    @discardableResult
    func newTab(url: URL?, background: Bool) -> String? {
        guard tabs.count < Self.maxTabs else { return nil }
        let tab = BrowserTab(id: BrowserSnapshot.freshTabID(), url: url, restored: nil, owner: self)
        tabs.append(tab)
        tab.startIfNeeded()
        if !background {
            activeIndex = tabs.count - 1
            showActiveTab()
        }
        refreshTabStrip()
        NotificationCenter.default.post(name: .webviewStateChanged, object: self)
        return tab.id
    }

    func tab(id: String) -> BrowserTab? {
        tabs.first { $0.id == id }
    }

    @discardableResult
    func selectTab(id: String) -> Bool {
        guard let index = tabs.firstIndex(where: { $0.id == id }),
              !tabs[index].isRebuilding
        else { return false }
        activeIndex = index
        showActiveTab()
        refreshTabStrip()
        NotificationCenter.default.post(name: .webviewStateChanged, object: self)
        return true
    }

    /// Close a tab. Refuses the pane's last tab (closing the pane is the tab manager's job, and a
    /// pane with no tabs has no coherent snapshot). Returns an error message, or nil on success.
    func closeTab(id: String) -> String? {
        guard tabs.count > 1 else { return "cannot close the pane's last tab" }
        guard let index = tabs.firstIndex(where: { $0.id == id }) else {
            return "no such tab: \(id)"
        }
        // A tab mid-protection-transition has no web view; dereferencing it crashed the app when
        // a second RPC connection arrived during the rebuild.
        guard !tabs[index].isRebuilding else {
            return "the tab is changing protection mode; try again"
        }
        let wasActive = index == activeIndex
        tabs[index].webView?.removeFromSuperview()
        tabs.remove(at: index)
        // Keep the same tab selected when an earlier one closes.
        if wasActive {
            activeIndex = min(index, tabs.count - 1)
            showActiveTab()
        } else if index < activeIndex {
            activeIndex -= 1
        }
        refreshTabStrip()
        NotificationCenter.default.post(name: .webviewStateChanged, object: self)
        return nil
    }

    func moveTab(id: String, to destination: Int) -> String? {
        guard let from = tabs.firstIndex(where: { $0.id == id }) else {
            return "no such tab: \(id)"
        }
        let to = max(0, min(destination, tabs.count - 1))
        guard to != from else { return nil }
        // Track the active tab, not its index: reordering shifts indices.
        let activeID = activeTab.id
        let tab = tabs.remove(at: from)
        tabs.insert(tab, at: to)
        activeIndex = tabs.firstIndex { $0.id == activeID } ?? 0
        refreshTabStrip()
        NotificationCenter.default.post(name: .webviewStateChanged, object: self)
        return nil
    }

    /// True when any tab in this pane is protected. Protection freezes the whole profile, since a
    /// sibling tab is a window onto the same shared storage.
    var hasProtectedTab: Bool {
        tabs.contains { $0.tabMode == .protected }
    }

    /// Is any live browser pane protected? Every pane shares `WKWebsiteDataStore.default()` and so
    /// the logged-in session, so protection has to hold across all of them.
    ///
    /// Answered from a registry, not the window: a pane in an inactive copad tab has no
    /// `view.window`, and the window-based lookup let its capture script keep writing the shared
    /// session's activity into an agent-readable file while another pane was protected.
    var profileHasProtectedTab: Bool {
        WebViewController.anyProtected
    }

    /// Every live browser pane, weakly held so a closed pane drops out without a deregistration
    /// that some teardown path might miss (which would pin protection on forever).
    private static let livePanes = NSHashTable<WebViewController>.weakObjects()

    static var anyProtected: Bool {
        livePanes.allObjects.contains { $0.hasProtectedTab }
    }

    private func showActiveTab() {
        guard let host = webContainer, tabs.indices.contains(activeIndex) else { return }
        NSLayoutConstraint.deactivate(webConstraints)
        for subview in host.subviews { subview.removeFromSuperview() }
        guard let wv = tabs[activeIndex].webView else { return }
        host.addSubview(wv)
        webConstraints = [
            wv.topAnchor.constraint(equalTo: host.topAnchor),
            wv.leadingAnchor.constraint(equalTo: host.leadingAnchor),
            wv.trailingAnchor.constraint(equalTo: host.trailingAnchor),
            wv.bottomAnchor.constraint(equalTo: host.bottomAnchor),
        ]
        NSLayoutConstraint.activate(webConstraints)
        syncBackgroundTabSizes()
        refreshChrome(for: tabs[activeIndex])
    }

    /// Keep background tabs the size of the visible one. A tab outside the view hierarchy gets no
    /// layout, so a screenshot of it would use a viewport the user never had, and a zero frame
    /// makes `takeSnapshot` return an image that cannot be encoded.
    private func syncBackgroundTabSizes() {
        guard let host = webContainer else { return }
        let size = host.bounds.size.width > 1 ? host.bounds.size : BrowserTab.defaultFrame.size
        for (index, tab) in tabs.enumerated() where index != activeIndex {
            tab.setBackgroundSize(size)
        }
    }

    override func viewDidLayout() {
        super.viewDidLayout()
        syncBackgroundTabSizes()
    }

    private func refreshTabStrip() {
        guard let strip = tabStrip else { return }
        for chip in strip.arrangedSubviews { strip.removeArrangedSubview(chip); chip.removeFromSuperview() }
        // A lone chip tells the user nothing the URL bar does not.
        tabStripScroll?.isHidden = tabs.count < 2
        guard tabs.count > 1 else { return }
        for (index, tab) in tabs.enumerated() {
            strip.addArrangedSubview(makeTabChip(tab: tab, index: index, active: index == activeIndex))
        }
    }

    private func makeTabChip(tab: BrowserTab, index: Int, active: Bool) -> NSView {
        let button = NSButton()
        let label = tab.title.isEmpty ? "New tab" : tab.title
        button.title = label.count > 24 ? String(label.prefix(23)) + "…" : label
        button.bezelStyle = .recessed
        button.setButtonType(.pushOnPushOff)
        button.state = active ? .on : .off
        button.font = .systemFont(ofSize: 11)
        button.toolTip = tab.currentURL.isEmpty ? label : tab.currentURL
        button.tag = index
        button.target = self
        button.action = #selector(tabChipClicked(_:))
        button.translatesAutoresizingMaskIntoConstraints = false
        return button
    }

    @objc private func tabChipClicked(_ sender: NSButton) {
        guard tabs.indices.contains(sender.tag) else { return }
        selectTab(id: tabs[sender.tag].id)
    }

    // MARK: - Callbacks from BrowserTab

    /// A background tab's navigation updates its chip but not the URL bar, which belongs to the
    /// visible page.
    func refreshChrome(for tab: BrowserTab?) {
        guard let tab else { return }
        if tab === tabs[safe: activeIndex] {
            backButton?.isEnabled = tab.canGoBack
            forwardButton?.isEnabled = tab.canGoForward
            syncURLField(tab.currentURL)
        }
        refreshTabStrip()
    }

    func tabDidFinishLoading(_ tab: BrowserTab) {
        if tab === tabs[safe: activeIndex] {
            // A protected page's title is page-derived and travels on the event bus, which any
            // subscriber can read ("Reset password for ...").
            currentTitle = tab.underProtection ? "Protected" : tab.title
            NotificationCenter.default.post(name: .terminalTitleChanged, object: self)
            eventBus?.broadcast(event: "webview.loaded", data: ["panel_id": panelID])
            eventBus?.broadcast(
                event: "webview.title_changed",
                data: ["panel_id": panelID, "title": currentTitle],
            )
            eventBus?.broadcast(
                event: "panel.title_changed",
                data: ["panel_id": panelID, "title": currentTitle],
            )
        }
        refreshTabStrip()
    }

    /// A tab entered or left protected mode: its web view was replaced, so re-install it.
    func tabModeChanged(_ tab: BrowserTab) {
        if tab === tabs[safe: activeIndex] { showActiveTab() }
        refreshTabStrip()
        eventBus?.broadcast(event: "webview.tab_mode_changed", data: [
            "panel_id": panelID,
            "tab_id": tab.id,
            "mode": tab.tabMode.rawValue,
        ])
    }

    func tabDidCommit(_ tab: BrowserTab, url: String) {
        // An event carrying a protected page's URL is a channel out, like the log.
        guard !tab.underProtection else { return }
        eventBus?.broadcast(
            event: "webview.navigated",
            data: ["panel_id": panelID, "tab_id": tab.id, "url": url],
        )
    }

    // MARK: - Snapshot

    func snapshotPane(policy: String) -> BrowserPaneSnap {
        BrowserPaneSnap(
            tabs: tabs.map { $0.snapshot(policy: policy) },
            active: activeIndex,
        )
    }

    /// URL a snapshot records for the pane as a whole (the active tab's). Blanked for a protected
    /// tab: `PaneManager.paneContent` persists this independently as `PaneContent.webview.url`,
    /// so blanking only `BrowserTabSnap.url` leaked a protected `/reset/<token>` URL.
    var snapshotSourceURL: String {
        guard let tab = tabs[safe: activeIndex], !tab.underProtection else { return "" }
        return tab.snapshotSourceURL
    }

    // MARK: - Toolbar

    private func makeToolbarButton(symbol: String, tooltip: String, action: Selector) -> NSButton {
        let btn = NSButton()
        btn.image = NSImage(systemSymbolName: symbol, accessibilityDescription: tooltip)
        btn.bezelStyle = .regularSquare
        btn.isBordered = false
        btn.imageScaling = .scaleProportionallyDown
        btn.toolTip = tooltip
        btn.target = self
        btn.action = action
        btn.translatesAutoresizingMaskIntoConstraints = false
        btn.widthAnchor.constraint(equalToConstant: 24).isActive = true
        btn.heightAnchor.constraint(equalToConstant: 24).isActive = true
        return btn
    }

    private func syncURLField(_ urlString: String) {
        guard let urlField else { return }
        guard !urlString.isEmpty, urlString != "about:blank" else { return }
        if view.window?.firstResponder === urlField.currentEditor() { return }
        urlField.stringValue = urlString
    }

    @objc private func backTapped() { goBack() }
    @objc private func forwardTapped() { goForward() }
    @objc private func reloadTapped() { reload() }
    @objc private func devtoolsTapped() { toggleDevTools() }

    /// Unlike `webview.tab.new` (background by default), the `+` button switches to the new tab.
    @objc private func newTabTapped() {
        guard newTab(url: nil, background: false) != nil else {
            NSSound.beep()
            return
        }
        view.window?.makeFirstResponder(urlField)
    }

    @objc private func urlFieldSubmit(_ sender: NSTextField) {
        let text = sender.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        navigate(to: text)
        view.window?.makeFirstResponder(webView)
    }

    // MARK: - Navigation (active tab)

    func navigate(to urlString: String) { activeTab.navigate(to: urlString) }
    func goBack() { activeTab.goBack() }
    func goForward() { activeTab.goForward() }
    func reload() { activeTab.reload() }

    func executeJS(_ script: String, completion: @escaping (Any?, Error?) -> Void) {
        activeTab.executeJS(script, completion: completion)
    }

    func getContent(completion: @escaping (String) -> Void) {
        activeTab.getContent(completion: completion)
    }

    // MARK: - State

    func toggleDevTools() {
        guard let webView else { return }
        let current = webView.configuration.preferences
            .value(forKey: "developerExtrasEnabled") as? Bool ?? false
        webView.configuration.preferences.setValue(!current, forKey: "developerExtrasEnabled")
    }

    var currentURL: String { tabs[safe: activeIndex]?.currentURL ?? "" }
    var canGoBack: Bool { tabs[safe: activeIndex]?.canGoBack ?? false }
    var canGoForward: Bool { tabs[safe: activeIndex]?.canGoForward ?? false }
    var isLoading: Bool { tabs[safe: activeIndex]?.isLoading ?? false }
}

private extension Array {
    /// Bounds-checked access: the tab list is mutated from socket commands, so a queued callback's
    /// index may be stale.
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}
