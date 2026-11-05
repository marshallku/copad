import AppKit
import CopadCore
@preconcurrency import WebKit

/// One tab inside a browser pane: a `WKWebView` plus all state that belongs to that page
/// rather than the pane around it.
///
/// Background tabs are plain detached views: `takeSnapshot` was measured to render correctly
/// at the right size on a web view in a hidden, off-screen, zero-alpha or no window, so an
/// inactive tab is simply not in the view hierarchy. What does NOT work off-screen is
/// `requestAnimationFrame` (an occluded web view never gets a frame); injected code must
/// assume that (see `scrollReporterJS`).
@MainActor
final class BrowserTab: NSObject {
    /// Stable across restarts, and the key its history blob is filed under.
    let id: String
    private(set) var webView: WKWebView!
    private(set) var title: String = "Web"

    private(set) var tabMode: TabMode = .automation

    /// Bumped on every main-frame commit AND every mode change. Binds a fill authorization to a
    /// DOCUMENT rather than an origin: an origin check taken before an async keychain read is
    /// stale by the time the read returns.
    private(set) var documentGeneration: UInt64 = 0

    /// Origin whose executable caches were purged for the CURRENT protected session, or nil if
    /// the purge did not complete. A fill is refused unless this matches the page's origin.
    /// Closes two holes: a purge that TIMED OUT still entered protected mode (an agent-planted
    /// service worker could survive), and only the origin present at protect-time was purged
    /// (protecting a blank tab, then navigating somewhere pre-poisoned, bypassed it).
    private(set) var purgedOrigin: String?

    /// Mirrors `copad_core::browser::secrets::TabMode`.
    enum TabMode: String {
        case automation
        case protected
    }

    private weak var owner: WebViewController?
    private var startURL: URL?
    private var started = false
    private var observations: [NSKeyValueObservation] = []

    // MARK: Navigation lifecycle
    //
    // See `BrowserSnapshot.resolveURL` for why a pending destination outranks the live URL:
    // `WKWebView.url` is nil until WebKit has one and stale after a failed navigation, and the
    // session save is on a timer, so trusting it persisted `""` or the page before last.

    private var pendingURL: String?
    /// True once `pendingURL`'s navigation was reported as failed. It is still worth persisting
    /// (the user asked for it) but must not outrank a later same-document navigation.
    private var pendingFailed = false
    /// Identity of the navigation `pendingURL` belongs to. Delegate callbacks hop to the main
    /// actor and run after WebKit called them, so without this page A committing and the user
    /// then navigating to B would let A's queued callback clear B's destination.
    private var pendingNavigationID: ObjectIdentifier?

    // MARK: History + scroll

    private var historyGeneration: UInt64 = 0
    private var restoredHistory: Data?
    private var restoredHistoryDepth: Int?
    private var restoredScrollY: Double?
    private var lastKnownScrollY: Double = 0
    private var lastNotifiedScrollY: Double = 0

    /// Size a tab's web view starts at, and falls back to when the pane has no laid-out size
    /// yet; roughly a browser window.
    static let defaultFrame = NSRect(x: 0, y: 0, width: 1280, height: 800)

    func setBackgroundSize(_ size: NSSize) {
        guard size.width > 1, size.height > 1, webView?.superview == nil else { return }
        webView?.frame = NSRect(origin: .zero, size: size)
    }

    /// Distance the page must scroll before it is worth re-persisting; the offset is a restore
    /// hint, so saving for every few pixels would be pure cost.
    private static let scrollPersistDelta: Double = 50

    // MARK: - Lifecycle

    init(id: String, url: URL?, restored: BrowserTabSnap?, owner: WebViewController) {
        self.id = id
        self.owner = owner
        super.init()

        let restoredURL = restored.flatMap { $0.url.isEmpty ? nil : URL(string: $0.url) } ?? url
        startURL = restoredURL
        pendingURL = restoredURL?.absoluteString
        if let restored, !restored.title.isEmpty { title = restored.title }

        // Read the opaque history blob now. A read failure (absent, unreadable, oversize, a
        // planted symlink) is not an error: the tab restores as a plain URL load.
        if let restored, let generation = restored.historyGeneration {
            historyGeneration = generation
            restoredHistory = BrowserFFI.readHistory(tabID: id, generation: generation)
            restoredHistoryDepth = restored.historyDepth
            restoredScrollY = restored.scrollY
        }

        buildWebView()
    }

    /// Enter or leave protected mode. Entering DESTROYS and rebuilds the web view: a brand-new
    /// `WKWebView` (new content world, new web content process) cannot carry a script the agent
    /// installed into the old document, while sealing the existing document would guarantee
    /// nothing, since the agent could have installed a listener before the fill (decision #100).
    /// Leaving destroys it too: there is no safe "unseal this document" transition (a
    /// cancelled submit, a swallowed SPA event or a back-navigation all leave the password
    /// recoverable). The session survives (cookies live in the data store) while
    /// `sessionStorage` and in-page JS state do not.
    func setProtected(_ on: Bool, completion: (() -> Void)? = nil) {
        guard (tabMode == .protected) != on else { completion?(); return }
        let url = webView?.url ?? startURL
        tabMode = on ? .protected : .automation
        documentGeneration &+= 1

        // Tear the old view down FIRST so nothing from the previous document runs during the
        // purge.
        observations = []
        webView?.removeFromSuperview()
        webView?.navigationDelegate = nil
        webView = nil

        let rebuild = { [weak self] (purged: String?) in
            guard let self else { return }
            self.purgedOrigin = purged
            self.buildWebView()
            self.started = false
            self.startURL = url
            self.pendingURL = url?.absoluteString
            self.pendingFailed = false
            self.pendingNavigationID = nil
            // Never restore history into a protected document, nor carry one out of it.
            self.restoredHistory = nil
            self.restoredHistoryDepth = nil
            self.restoredScrollY = nil
            self.startIfNeeded()
            self.owner?.tabModeChanged(self)
            completion?()
        }

        guard on else {
            rebuild(nil)
            return
        }
        // A fresh view is not enough: an agent that ran `execute_js` on this origin could have
        // poisoned persistent state (a Cache API entry, a service worker) that the rebuilt view
        // would load and execute. The purge is ASYNCHRONOUS, so the load must WAIT for it, or a
        // live service worker could serve the protected document first. Cookies and
        // localStorage are deliberately KEPT: purging them signs the user out, and neither is
        // executable.
        purgeExecutableCaches(for: url) { purgedOrigin in rebuild(purgedOrigin) }
    }

    /// Make sure this origin has been purged AND that the document currently loaded from it
    /// post-dates the purge. Purging after the fact is not enough: protect a blank tab, navigate
    /// to an origin with an agent-planted service worker, and the worker has already served the
    /// document. So a not-yet-purged origin is purged and the tab then REBUILT, reloading the
    /// destination through a fresh view with the caches gone.
    func ensurePurged(for origin: String, then done: @escaping (Bool) -> Void) {
        if let purgedOrigin, purgedOrigin.caseInsensitiveCompare(origin) == .orderedSame {
            done(true)
            return
        }
        purgeExecutableCaches(for: URL(string: origin)) { [weak self] purged in
            guard let self else { done(false); return }
            self.purgedOrigin = purged
            guard purged != nil else { done(false); return }
            self.rebuildForPurge { done(true) }
        }
    }

    /// Rebuild the web view in place, keeping protected mode, and reload the current URL, so
    /// the document the credential goes into was produced with the caches already gone.
    private func rebuildForPurge(then done: @escaping () -> Void) {
        let url = webView?.url ?? startURL
        documentGeneration &+= 1
        observations = []
        webView?.removeFromSuperview()
        webView?.navigationDelegate = nil
        webView = nil
        buildWebView()
        started = false
        startURL = url
        pendingURL = url?.absoluteString
        pendingFailed = false
        pendingNavigationID = nil
        restoredHistory = nil
        restoredHistoryDepth = nil
        restoredScrollY = nil
        startIfNeeded()
        owner?.tabModeChanged(self)
        // Let the reload commit before the caller validates against `documentGeneration`.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { done() }
    }

    /// Purge the origin's script-writable code caches, reporting the origin actually cleared
    /// or nil. A timeout reports FAILURE rather than proceeding: an unanswered WebKit callback
    /// would otherwise silently downgrade protected mode to a fresh view over whatever the
    /// agent had already planted.
    private func purgeExecutableCaches(for url: URL?, then done: @escaping (String?) -> Void) {
        guard let host = url?.host, let origin = url.map({ u in
            var c = URLComponents()
            c.scheme = u.scheme
            c.host = u.host
            c.port = u.port
            return c.string ?? ""
        }), !origin.isEmpty else {
            // Nothing to purge (a blank tab): recorded as "no origin purged", so the first
            // navigation to a real origin must purge before a credential may be used there.
            done(nil)
            return
        }
        let types: Set<String> = [
            WKWebsiteDataTypeServiceWorkerRegistrations,
            WKWebsiteDataTypeFetchCache,
            WKWebsiteDataTypeDiskCache,
            WKWebsiteDataTypeMemoryCache,
            WKWebsiteDataTypeOfflineWebApplicationCache,
        ]
        let store = WKWebsiteDataStore.default()
        let box = SendableBox<(String?) -> Void>(done)
        var finished = false
        let finish = { [box] (purged: String?) in
            guard !finished else { return }
            finished = true
            if purged == nil {
                FileHandle.standardError.write(Data(
                    "[copad-browser] cache purge did not complete; credentials stay refused for this origin\n".utf8,
                ))
            }
            box.value(purged)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { finish(nil) }
        store.fetchDataRecords(ofTypes: types) { records in
            let mine = records.filter { $0.displayName == host || host.hasSuffix($0.displayName) }
            guard !mine.isEmpty else { finish(origin); return }
            store.removeData(ofTypes: types, for: mine) { finish(origin) }
        }
    }

    private func buildWebView() {
        let config = WKWebViewConfiguration()
        // Assert the DEFAULT data store rather than rely on it. `WKWebViewConfiguration()`
        // defaults to it, which is the only reason logins survive a restart. It must stay
        // `.default()`: an identified store is a DISTINCT store, so adopting one is a one-way
        // sign-out with no migration path (decision #100).
        config.websiteDataStore = .default()
        config.userContentController.addUserScript(WKUserScript(
            source: Self.scrollReporterJS,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true,
        ))
        config.userContentController.add(ScrollReporter(owner: self), name: Self.scrollHandlerName)

        // Capture is suppressed AT THE SOURCE in protected mode, not filtered at read time: the
        // JSONL log is agent-readable, so refusing the `webview.net` read while the file kept
        // filling would be no defence (a `console.log(password)` is read from the file).
        if tabMode != .protected {
            config.userContentController.addUserScript(WKUserScript(
                source: Self.captureJS,
                injectionTime: .atDocumentStart,
                forMainFrameOnly: false,
            ))
            config.userContentController.add(
                CaptureReporter(owner: self),
                name: Self.captureHandlerName,
            )
        }
        // Enable Safari Web Inspector (right-click → Inspect Element)
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")

        // A real starting frame, not `.zero`: a background tab is not in the view hierarchy, so
        // autolayout never sizes it, and `takeSnapshot` on a zero-sized view yields an image
        // that cannot be encoded ("could not encode PNG"). The pane resizes background tabs
        // (`syncBackgroundTabSizes`); this is the size before the first layout.
        let wv = WKWebView(frame: Self.defaultFrame, configuration: config)
        wv.navigationDelegate = self
        wv.translatesAutoresizingMaskIntoConstraints = false
        webView = wv

        observations = [
            wv.observe(\.canGoBack, options: [.new, .initial]) { [weak self] _, _ in
                Task { @MainActor in self?.owner?.refreshChrome(for: self) }
            },
            wv.observe(\.canGoForward, options: [.new, .initial]) { [weak self] _, _ in
                Task { @MainActor in self?.owner?.refreshChrome(for: self) }
            },
            wv.observe(\.url, options: [.new]) { [weak self] wv, _ in
                Task { @MainActor in
                    guard let self else { return }
                    self.owner?.refreshChrome(for: self)
                    // `history.pushState` / `replaceState` / a fragment change never fire
                    // `didCommit`, and WebKit's same-document delegate callback is SPI, so this
                    // KVO is the ONLY supported signal. Under `restore = "url"` those are the
                    // route changes a single-page app makes.
                    self.noteURLChanged(wv.url?.absoluteString)
                }
            },
        ]
    }

    func startIfNeeded() {
        guard !started, webView != nil else { return }
        started = true
        if restoreHistoryIfPossible() { return }
        if let url = startURL {
            pendingNavigationID = webView.load(URLRequest(url: url)).map(ObjectIdentifier.init)
        } else {
            loadBlankPage()
        }
    }

    // MARK: - Navigation

    /// True while a protected-mode transition is rebuilding the web view; commands that would
    /// touch it check this rather than trusting the implicitly-unwrapped property.
    var isRebuilding: Bool { webView == nil }

    func navigate(to urlString: String) {
        guard let webView else { return }
        let finalString: String = if urlString.hasPrefix("http://")
            || urlString.hasPrefix("https://") || urlString.hasPrefix("file://")
        {
            urlString
        } else {
            "https://" + urlString
        }
        guard let url = URL(string: finalString) else { return }
        // Record the destination BEFORE loading it, or a blank tab given an unreachable address
        // has no live and no pending URL and the autosave writes "".
        pendingURL = url.absoluteString
        pendingFailed = false
        pendingNavigationID = webView.load(URLRequest(url: url)).map(ObjectIdentifier.init)
        NotificationCenter.default.post(name: .webviewStateChanged, object: owner)
    }

    func goBack() {
        guard let webView else { return }
        if webView.canGoBack { adoptNavigation(webView.goBack()) }
    }

    func goForward() {
        guard let webView else { return }
        if webView.canGoForward { adoptNavigation(webView.goForward()) }
    }

    func reload() {
        guard let webView else { return }
        adoptNavigation(webView.reload())
    }

    /// Take over the pending state for a navigation this tab did not choose a URL for (back,
    /// forward, reload). Otherwise the identity check would reject the new navigation's
    /// `didCommit` (it still names the previous one) and nothing would clear the pending state,
    /// and a destination abandoned by pressing Back would still be persisted. The pending URL
    /// is dropped only when a usable live URL exists to fall back on: reloading a restored tab
    /// that never managed to load would otherwise snap the snapshot to `""`.
    private func adoptNavigation(_ navigation: WKNavigation?) {
        pendingNavigationID = navigation.map(ObjectIdentifier.init)
        let live = webView?.url?.absoluteString
        if let live, !live.isEmpty, live != "about:blank" {
            pendingURL = nil
            pendingFailed = false
        }
    }

    private func isCurrentNavigation(_ navID: ObjectIdentifier?) -> Bool {
        guard let pendingNavigationID else { return navID == nil || pendingURL == nil }
        return navID == pendingNavigationID
    }

    /// WebKit now reports `live`. Retire the pending destination once reached; schedule a save
    /// either way.
    private func noteURLChanged(_ live: String?) {
        if let live, !live.isEmpty, live != "about:blank" {
            // Reached, or superseded: a live URL arriving after a FAILED
            // destination means the document moved on without it.
            if live == pendingURL || pendingFailed {
                pendingURL = nil
                pendingFailed = false
                pendingNavigationID = nil
            }
        }
        NotificationCenter.default.post(name: .webviewStateChanged, object: owner)
    }

    // MARK: - Snapshot

    var snapshotSourceURL: String {
        BrowserSnapshot.resolveURL(live: webView?.url?.absoluteString, pending: pendingURL)
    }

    var currentURL: String { webView?.url?.absoluteString ?? "" }
    var canGoBack: Bool { webView?.canGoBack ?? false }
    var canGoForward: Bool { webView?.canGoForward ?? false }
    var isLoading: Bool { webView?.isLoading ?? false }

    func snapshot(policy: String) -> BrowserTabSnap {
        // A protected tab persists NOTHING page-derived. Writing the URL, title or
        // interaction-state blob into session.json would leave the back door open: an OAuth
        // redirect or a "Reset password for ..." title lands in an agent-readable file, and the
        // blob holds the full history. The tab keeps its identity so the layout survives.
        guard !underProtection else {
            return BrowserTabSnap(id: id, url: "", title: "")
        }
        let decision = BrowserFFI.canonicalize(snapshotSourceURL, policy: policy)
        var snap = BrowserTabSnap(
            id: id,
            url: decision.url,
            // A page title carries the same exposure as a URL; whether it may be persisted is
            // Rust's call, not a string comparison here.
            title: decision.persistTitle ? title : "",
        )
        if let captured = captureHistory(policy: policy) {
            snap.historyGeneration = captured.generation
            snap.historyDepth = captured.depth
            snap.scrollY = captured.scrollY
        }
        return snap
    }

    /// Persist the opaque back/forward + scroll blob, if the policy keeps it. Returns nil (and
    /// records NO generation) when the blob is unavailable or the write failed: a generation
    /// referencing a blob never written would turn a write failure into a silent restore
    /// failure later.
    private func captureHistory(policy: String)
        -> (generation: UInt64, depth: Int, scrollY: Double)?
    {
        guard BrowserFFI.keepsHistory(policy: policy), let webView else { return nil }
        // The blob describes the document WebKit currently holds. With a pending destination
        // outstanding (typically unreachable), the persisted URL is that destination and the
        // blob is the PREVIOUS page: on restore the blob would pass the depth check, the URL
        // load would be skipped, and the user would land back on the page they left. Persist
        // the URL alone then.
        guard pendingURL == nil else { return nil }
        // `interactionState` is documented as `Any?`, not `Data`; take it only when it is `Data`.
        guard let blob = webView.interactionState as? Data, !blob.isEmpty else { return nil }

        let next = historyGeneration &+ 1
        guard BrowserFFI.writeHistory(tabID: id, generation: next, data: blob) else { return nil }
        historyGeneration = next
        return (next, webView.backForwardList.backList.count, lastKnownScrollY)
    }

    /// Restore the opaque back/forward list instead of re-loading the URL. Assigning
    /// `interactionState` is not evidence it worked: a blob from another WebKit version, or a
    /// corrupt one, fails SILENTLY and the view can still report the right URL. Success is
    /// judged on the back-list depth, which a plain `load(url)` could not fake.
    @discardableResult
    private func restoreHistoryIfPossible() -> Bool {
        guard let blob = restoredHistory, let expectedDepth = restoredHistoryDepth else {
            return false
        }
        restoredHistory = nil
        webView.interactionState = blob

        // A depth of zero proves nothing: a view that restored NOTHING also has an empty back
        // list, so a corrupt blob for a single-page history would "verify" and leave a blank tab
        // with no URL load to fall back on. There the evidence has to be the current item
        // existing at all.
        if expectedDepth == 0, webView.backForwardList.currentItem == nil {
            FileHandle.standardError.write(Data(
                "[copad-browser] history blob restored no current item — loading the URL instead\n".utf8,
            ))
            restoredHistoryDepth = nil
            restoredScrollY = nil
            return false
        }

        let actualDepth = webView.backForwardList.backList.count
        guard actualDepth == expectedDepth else {
            FileHandle.standardError.write(Data(
                "[copad-browser] history blob rejected (depth \(actualDepth) != \(expectedDepth)) — loading the URL instead\n".utf8,
            ))
            restoredHistoryDepth = nil
            restoredScrollY = nil
            return false
        }
        return true
    }

    /// Put the page back where the user left it once the restored document has laid out.
    /// Deliberately does not verify: a changed page clamps the offset, and discarding a working
    /// back/forward stack over that would be a bug.
    private func applyRestoredScroll() {
        guard let y = restoredScrollY, y > 0 else { return }
        restoredScrollY = nil
        webView.evaluateJavaScript("window.scrollTo(0, \(y)); 1") { _, _ in }
    }

    // MARK: - Scroll reporting

    static let scrollHandlerName = "copadScroll"

    /// Trailing-edge throttle on a TIMER, deliberately not `requestAnimationFrame`: a WKWebView
    /// in an occluded or off-screen window (a copad window behind another app, any background
    /// tab) never gets an animation frame, so every scroll report would be lost. `setTimeout` is
    /// throttled in the background but never stopped.
    static let scrollReporterJS = """
    (() => {
        let timer = null;
        const report = () => {
            timer = null;
            try {
                window.webkit.messageHandlers.\(scrollHandlerName).postMessage(window.scrollY);
            } catch (e) {}
        };
        const onScroll = () => {
            if (timer !== null) return;
            timer = setTimeout(report, 150);
        };
        window.addEventListener('scroll', onScroll, { passive: true });
        window.addEventListener('load', report);
    })();
    """

    static let captureHandlerName = "copadCapture"

    /// Patch `fetch`, `XMLHttpRequest` and `console.*` at document start. Coverage is declared,
    /// not implied: JS-initiated requests only (no subresources, `sendBeacon`, WebSocket frames
    /// or service-worker traffic; navigations are captured natively), and every `webview.net`
    /// response carries that coverage string so an empty list is never read as "no request was
    /// made". Everything is wrapped so a page with a frozen `console` or throwing `fetch` stub
    /// can't break its own capture, or be broken by it.
    static let captureJS = """
    (() => {
        const post = (payload) => {
            try {
                window.webkit.messageHandlers.\(captureHandlerName).postMessage(payload);
            } catch (e) {}
        };
        const now = () => Math.floor(Date.now() / 1000);

        // --- console ---
        for (const level of ['debug', 'log', 'info', 'warn', 'error']) {
            const original = console[level];
            if (typeof original !== 'function') continue;
            console[level] = function (...args) {
                try {
                    const text = args.map((a) => {
                        if (typeof a === 'string') return a;
                        try { return JSON.stringify(a); } catch (e) { return String(a); }
                    }).join(' ');
                    post({ kind: 'console', ts: now(), level, text });
                } catch (e) {}
                return original.apply(this, args);
            };
        }
        window.addEventListener('error', (e) => {
            post({ kind: 'console', ts: now(), level: 'error',
                   text: String(e.message || e), source: String(e.filename || '') });
        });
        window.addEventListener('unhandledrejection', (e) => {
            post({ kind: 'console', ts: now(), level: 'error',
                   text: 'unhandled rejection: ' + String(e.reason) });
        });

        // --- fetch ---
        const originalFetch = window.fetch;
        if (typeof originalFetch === 'function') {
            window.fetch = function (input, init) {
                const started = Date.now();
                const url = (typeof input === 'string') ? input : (input && input.url) || '';
                const method = (init && init.method) || (input && input.method) || 'GET';
                return originalFetch.apply(this, arguments).then((res) => {
                    post({ kind: 'net', ts: now(), source: 'script', method, url,
                           status: res.status, duration_ms: Date.now() - started });
                    return res;
                }, (err) => {
                    post({ kind: 'net', ts: now(), source: 'script', method, url,
                           duration_ms: Date.now() - started,
                           error: String(err) });
                    throw err;
                });
            };
        }

        // --- XMLHttpRequest ---
        // Still patched even though fetch is everywhere: plenty of libraries
        // use XHR underneath, and a log that quietly missed them would be worse
        // than no log.
        const XHR = window.XMLHttpRequest;
        if (XHR && XHR.prototype) {
            const open = XHR.prototype.open;
            const send = XHR.prototype.send;
            XHR.prototype.open = function (method, url) {
                this.__copad = { method, url, started: 0 };
                return open.apply(this, arguments);
            };
            XHR.prototype.send = function () {
                const meta = this.__copad;
                if (meta) {
                    meta.started = Date.now();
                    this.addEventListener('loadend', () => {
                        post({ kind: 'net', ts: now(), source: 'script',
                               method: meta.method || 'GET', url: meta.url || '',
                               status: this.status || undefined,
                               duration_ms: Date.now() - meta.started });
                    });
                }
                return send.apply(this, arguments);
            };
        }
    })();
    """

    /// Record a natively-observed navigation; the user script cannot see a document load, and
    /// without this the log would claim a navigated page made no requests.
    func recordNavigation(url: String, status: Int?) {
        // Capture is suppressed at the SOURCE under protection, and this is a source: the
        // record is written natively, so not installing the user script doesn't stop it. A
        // protected login redirect carries exactly the token the mode exists to contain, and
        // the JSONL log is agent-readable.
        guard !captureSuppressed else { return }
        guard let panelID = owner?.panelID, !url.isEmpty else { return }
        BrowserFFI.appendLog(
            panelID: panelID,
            kind: "net",
            record: [
                "ts": UInt64(Date().timeIntervalSince1970),
                "tab_id": id,
                "source": "navigation",
                "method": "GET",
                "url": url,
                "status": status as Any,
            ].compactMapValues { $0 is NSNull ? nil : $0 },
            captureBodies: owner?.captureBodies ?? false,
        )
    }

    /// Does protection apply to this tab right now: its own mode, or anything sharing its
    /// session being protected? Protection freezes the PROFILE, and must hold at capture,
    /// event and persistence time, not only at RPC-read time. Checking only the emitting tab
    /// would let an agent navigate an automation SIBLING to an authenticated endpoint and
    /// receive its URL through `webview.navigated`, or read it from session.json, while the
    /// RPC gate refused the same read.
    var underProtection: Bool {
        tabMode == .protected || owner?.profileHasProtectedTab == true
    }

    private var captureSuppressed: Bool {
        // Asked of the WINDOW: two browser panes share `WKWebsiteDataStore.default()`, so a
        // second pane's capture script would keep logging the protected session into a file the
        // agent reads directly. Protection freezes the profile; the profile is the window.
        tabMode == .protected || owner?.profileHasProtectedTab == true
    }

    /// Separate object because `WKUserContentController` retains its handlers.
    private final class CaptureReporter: NSObject, WKScriptMessageHandler {
        private weak var owner: BrowserTab?
        init(owner: BrowserTab) { self.owner = owner }

        func userContentController(
            _: WKUserContentController,
            didReceive message: WKScriptMessage,
        ) {
            guard var body = message.body as? [String: Any],
                  let kind = body.removeValue(forKey: "kind") as? String
            else { return }
            Task { @MainActor [weak owner] in
                guard let owner, let panelID = owner.owner?.panelID else { return }
                // Checked at delivery, not only at injection: this record may have been queued
                // before protection began.
                guard !owner.captureSuppressed else { return }
                body["tab_id"] = owner.id
                BrowserFFI.appendLog(
                    panelID: panelID,
                    kind: kind,
                    record: body,
                    captureBodies: owner.owner?.captureBodies ?? false,
                )
            }
        }
    }

    func noteScroll(_ y: Double) {
        lastKnownScrollY = y
        // Scrolling schedules its own save: only layout changes and navigations did, so the
        // offset would otherwise update in memory only.
        guard abs(y - lastNotifiedScrollY) >= Self.scrollPersistDelta else { return }
        lastNotifiedScrollY = y
        NotificationCenter.default.post(name: .webviewStateChanged, object: owner)
    }

    /// Separate object because `WKUserContentController` RETAINS its handlers; a tab
    /// registering itself would never deallocate.
    private final class ScrollReporter: NSObject, WKScriptMessageHandler {
        private weak var owner: BrowserTab?
        init(owner: BrowserTab) { self.owner = owner }

        func userContentController(
            _: WKUserContentController,
            didReceive message: WKScriptMessage,
        ) {
            guard let y = message.body as? Double else { return }
            Task { @MainActor [weak owner] in owner?.noteScroll(y) }
        }
    }

    // MARK: - Credentials

    /// Fill a credential into this tab's page.
    ///
    /// The secret's whole lifetime is inside this function (keychain, local `String`, JS
    /// assignment): it is in no parameter, return value, event or log. Order matters: the
    /// keychain read happens FIRST, and probe + validation + injection are a single JS
    /// evaluation, so the element that is checked is the element that is written. The
    /// preconditions come from `copad-core`, not Swift, since they decide whether a password
    /// reaches a page.
    func fillCredential(
        _ credential: CredentialRef,
        completion: @escaping (Result<String, RPCError>) -> Void,
    ) {
        guard tabMode == .protected else {
            completion(.failure(RPCError(
                code: "requires_protected",
                message: "a credential may only be filled into a protected tab",
            )))
            return
        }
        let generationAtStart = documentGeneration
        let origin = BrowserFFI.canonicalize(currentURL, policy: "origin").url

        BrowserKeychain.read(id: credential.id) { [weak self] result in
            guard let self else { return }
            switch result {
            case let .failure(err):
                completion(.failure(Self.keychainError(err)))
            case let .success(stored):
                // Re-validate against the document as it is NOW: the keychain read is the long
                // step, and exactly when a page could have navigated.
                guard self.documentGeneration == generationAtStart else {
                    completion(.failure(RPCError(
                        code: "document_changed",
                        message: "the tab navigated during the keychain read — fill aborted",
                    )))
                    return
                }
                // The ORIGIN and SLOT come from the keychain, never from the index: the index is
                // a plain JSON file anything running as the user can rewrite, and re-pointing an
                // entry at an attacker-chosen origin would make every downstream check agree.
                guard stored.origin.caseInsensitiveCompare(origin) == .orderedSame else {
                    completion(.failure(RPCError(
                        code: "origin_mismatch",
                        message: "this credential is bound to \(stored.origin), not \(origin)",
                    )))
                    return
                }
                // The purge is a PRECONDITION, re-established for whatever origin the tab is on
                // now, not just the one when protection started.
                self.ensurePurged(for: origin) { purged in
                    guard purged else {
                        completion(.failure(RPCError(
                            code: "purge_incomplete",
                            message: "this origin's service workers and code caches could not be cleared; refusing to fill",
                        )))
                        return
                    }
                    // The generation is re-read HERE, not compared to the earlier value: a late
                    // purge deliberately rebuilds the view and bumps it, so an equality check
                    // would reject exactly the case the purge makes safe. The origin must still
                    // hold; the injection re-asserts it inside the page.
                    let generationNow = self.documentGeneration
                    guard self.tabMode == .protected else {
                        completion(.failure(RPCError(
                            code: "requires_protected",
                            message: "the tab left protected mode during the purge",
                        )))
                        return
                    }
                    self.injectAfterValidating(
                        credential, stored: stored, origin: origin,
                        generation: generationNow, completion: completion,
                    )
                }
            }
        }
    }

    /// Probe, validate and write in ONE evaluation. The injection script re-asserts the probed
    /// element's identity and type before assigning, so a swap between probe and write is
    /// caught by the page itself, and it REPORTS whether it wrote (a silent no-op must not
    /// count as success).
    private func injectAfterValidating(
        _ credential: CredentialRef,
        stored: BrowserKeychain.Stored,
        origin: String,
        generation: UInt64,
        completion: @escaping (Result<String, RPCError>) -> Void,
    ) {
        let wantsPassword = stored.slot == "password"
        // A nonce stamped on the element during the probe and required by the write.
        // Re-querying a selector is not element identity: a same-origin navigation, or the page
        // swapping in a different password input, yields a NEW element the selector still
        // matches.
        let nonce = UUID().uuidString
        let probe = """
        (() => {
            const el = \(wantsPassword)
                ? document.querySelector('input[type="password"]')
                : document.querySelector('input[type="text"], input:not([type])');
            if (!el) return JSON.stringify(null);
            const sel = el.id ? '#' + CSS.escape(el.id)
                : (el.name ? 'input[name="' + CSS.escape(el.name) + '"]' : null);
            if (!sel) return JSON.stringify(null);
            el.dataset.copadFillNonce = \(Self.jsString(nonce));
            return JSON.stringify({ selector: sel, is_password_input: el.type === 'password' });
        })()
        """
        let box = SendableBox<(Result<String, RPCError>) -> Void>(completion)
        webView.evaluateJavaScript(probe) { [weak self] result, _ in
            Task { @MainActor in
                guard let self else { return }
                guard let raw = result as? String,
                      let data = raw.data(using: .utf8),
                      let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let selector = obj["selector"] as? String
                else {
                    box.value(.failure(RPCError(
                        code: "no_target",
                        message: "no fillable input element on this page",
                    )))
                    return
                }
                let isPassword = obj["is_password_input"] as? Bool ?? false

                if let problem = BrowserFFI.validateFill(
                    request: [
                        "credential_id": credential.id,
                        "profile": BrowserPaneSnap.defaultProfile,
                        "tab_id": self.id,
                        "origin": stored.origin,
                        "document_generation": generation,
                        "slot": stored.slot,
                    ],
                    // The credential as the TRUSTED store describes it, with the
                    // index contributing only the id and the display name.
                    credential: CredentialRef(
                        id: credential.id,
                        origin: stored.origin,
                        username: credential.username,
                        label: credential.label,
                        slot: stored.slot,
                        createdAt: credential.createdAt,
                        lastUsed: credential.lastUsed,
                    ),
                    live: [
                        "tab_id": self.id,
                        "profile": BrowserPaneSnap.defaultProfile,
                        "mode": self.tabMode.rawValue,
                        "origin": origin,
                        "document_generation": self.documentGeneration,
                    ],
                    target: ["selector": selector, "is_password_input": isPassword],
                ) {
                    box.value(.failure(problem))
                    return
                }

                // The injection re-asserts the ORIGIN and the element inside the page itself.
                // Re-querying a selector is not enough: a navigation between validation and
                // execution lands on a different document that may have its own matching
                // password field. The page can only answer for the document it is in.
                let js = """
                (() => {
                    if (location.origin !== \(Self.jsString(stored.origin))) return 'wrong-origin';
                    // By NONCE, not by selector: the element that was validated
                    // is the element that gets written, even if the page has
                    // since grown another one the selector would also match.
                    const el = document.querySelector('[data-copad-fill-nonce=' +
                        JSON.stringify(\(Self.jsString(nonce))) + ']');
                    if (!el) return 'gone';
                    delete el.dataset.copadFillNonce;
                    if (\(wantsPassword) && el.type !== 'password') return 'retyped';
                    el.focus({ preventScroll: true });
                    el.value = \(Self.jsString(stored.secret));
                    el.dispatchEvent(new Event('input', { bubbles: true }));
                    el.dispatchEvent(new Event('change', { bubbles: true }));
                    return 'ok';
                })()
                """
                guard let webView = self.webView, self.documentGeneration == generation else {
                    box.value(.failure(RPCError(
                        code: "document_changed",
                        message: "the tab changed between validation and injection",
                    )))
                    return
                }
                webView.evaluateJavaScript(js) { outcome, error in
                    Task { @MainActor in
                        // A fill that did nothing must not report success.
                        switch outcome as? String {
                        case "ok":
                            box.value(.success(selector))
                        case "wrong-origin":
                            box.value(.failure(RPCError(
                                code: "origin_mismatch",
                                message: "the document navigated to another origin before the write",
                            )))
                        case "gone":
                            box.value(.failure(RPCError(
                                code: "document_changed",
                                message: "the target element disappeared before the value was written",
                            )))
                        case "retyped":
                            box.value(.failure(RPCError(
                                code: "not_a_password_input",
                                message: "the target element stopped being a password input",
                            )))
                        default:
                            // A FIXED message: `el.value = ...` runs a page-defined setter, and
                            // one that throws `new Error(value)` would put the password into
                            // `localizedDescription` and out through the RPC response.
                            box.value(.failure(RPCError(
                                code: "fill_failed",
                                message: "the page did not accept the value",
                            )))
                        }
                    }
                }
            }
        }
    }

    static func keychainError(_ failure: BrowserKeychain.Failure) -> RPCError {
        switch failure {
        case .notFound:
            RPCError(code: "not_found", message: "no secret stored for that credential")
        case let .unavailable(detail):
            RPCError(
                code: "secret_backend_unavailable",
                message: "platform keychain unavailable, password features are disabled: \(detail)",
            )
        }
    }

    /// JSON-encode a string so it can be embedded literally in JS source. The
    /// secret goes through this, so a password containing a quote or a
    /// backslash cannot break out into executable JS.
    static func jsString(_ s: String) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: s, options: [.fragmentsAllowed]),
              let str = String(data: data, encoding: .utf8)
        else { return "\"\"" }
        return str
    }

    // MARK: - Misc

    func executeJS(_ script: String, completion: @escaping (Any?, Error?) -> Void) {
        guard let webView else {
            completion(nil, NSError(
                domain: "copad.browser", code: 1,
                userInfo: [NSLocalizedDescriptionKey: "the tab is rebuilding"],
            ))
            return
        }
        // WKWebView's completionHandler is @Sendable in the Swift 6 SDK but the socket command
        // chain that owns `completion` is not. Box the callback; WebKit invokes it on the main
        // thread, so the unchecked-sendable bridge is sound.
        let box = SendableBox(completion)
        webView.evaluateJavaScript(script) { result, error in
            box.value(result, error)
        }
    }

    /// Re-apply `developerExtrasEnabled` on THIS tab's configuration (right-click, Inspect).
    func toggleDevTools() {
        guard let webView else { return }
        let current = webView.configuration.preferences
            .value(forKey: "developerExtrasEnabled") as? Bool ?? false
        webView.configuration.preferences.setValue(!current, forKey: "developerExtrasEnabled")
    }

    func getContent(completion: @escaping (String) -> Void) {
        guard let webView else { completion(""); return }
        let box = SendableBox(completion)
        webView.evaluateJavaScript("document.documentElement.outerHTML") { result, _ in
            box.value(result as? String ?? "")
        }
    }

    private func loadBlankPage() {
        let html = """
        <html>
        <body style="background:#1e1e2e;color:#cdd6f4;font-family:system-ui;
                     display:flex;align-items:center;justify-content:center;
                     height:100vh;margin:0">
          <p style="opacity:0.4">Open a URL to get started</p>
        </body>
        </html>
        """
        webView.loadHTMLString(html, baseURL: nil)
    }
}

// MARK: - WKNavigationDelegate

extension BrowserTab: WKNavigationDelegate {
    nonisolated func webView(_ webView: WKWebView, didFinish _: WKNavigation!) {
        Task { @MainActor in
            self.applyRestoredScroll()
            let pageTitle = webView.title
            let host = webView.url?.host
            self.title = (pageTitle?.isEmpty == false ? pageTitle! : host) ?? "Web"
            self.owner?.tabDidFinishLoading(self)
        }
    }

    /// Any navigation starting, including one the PAGE began (link click, `<meta refresh>`, JS
    /// redirect). Those never pass through `navigate`/`goBack`/`reload`, so without adopting
    /// them here the tab would reject the new navigation's `didCommit` and leave the pending
    /// destination stuck. The equality guard keeps this from eating our own navigations:
    /// `navigate` assigns `pendingNavigationID` synchronously from `load()`'s return value.
    nonisolated func webView(
        _ webView: WKWebView,
        didStartProvisionalNavigation navigation: WKNavigation!,
    ) {
        let navID = navigation.map(ObjectIdentifier.init)
        Task { @MainActor in
            guard self.pendingNavigationID != navID else { return }
            self.pendingNavigationID = navID
            let live = webView.url?.absoluteString
            if let live, !live.isEmpty, live != "about:blank" {
                self.pendingURL = nil
                self.pendingFailed = false
            }
        }
    }

    /// A load that never got off the ground (unreachable host, DNS failure, refused connection).
    /// WebKit reverts `url` to the previously committed page, so the pending destination must be
    /// re-armed or the tab would autosave the OLD page.
    nonisolated func webView(
        _: WKWebView,
        didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error,
    ) {
        let navID = navigation.map(ObjectIdentifier.init)
        let ns = error as NSError
        // A cancellation is THIS navigation being superseded; re-arming from it would
        // resurrect the abandoned destination.
        let cancelled = ns.domain == NSURLErrorDomain && ns.code == NSURLErrorCancelled
        let failing = ns.userInfo[NSURLErrorFailingURLStringErrorKey] as? String
        Task { @MainActor in
            guard self.isCurrentNavigation(navID) else { return }
            if !cancelled, let failing, !failing.isEmpty, failing != "about:blank",
               self.pendingURL == nil || self.pendingURL == failing
            {
                self.pendingURL = failing
                self.pendingFailed = true
            }
            NotificationCenter.default.post(name: .webviewStateChanged, object: self.owner)
        }
    }

    nonisolated func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) {
        let navID = navigation.map(ObjectIdentifier.init)
        Task { @MainActor in
            guard self.isCurrentNavigation(navID) else { return }
            let urlStr = webView.url?.absoluteString ?? ""
            // A committed document supersedes the pending destination whether
            // or not the two strings match (a redirect lands elsewhere), which
            // is why this does not go through `noteURLChanged`'s equality check.
            if !urlStr.isEmpty, urlStr != "about:blank" {
                self.pendingURL = nil
                self.pendingFailed = false
                self.pendingNavigationID = nil
                self.documentGeneration &+= 1
            }
            NotificationCenter.default.post(name: .webviewStateChanged, object: self.owner)
            self.recordNavigation(url: urlStr, status: nil)
            self.owner?.tabDidCommit(self, url: urlStr)
        }
    }
}
