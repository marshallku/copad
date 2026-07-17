import SwiftUI
import WebKit

/// Observable load state for the WebView, so ContentView can overlay a
/// failure/retry UI instead of a blank web view. `reloadToken` is bumped to
/// force a reload of the same URL (the Retry button) — a URL-equality guard
/// alone would otherwise skip it.
@MainActor
final class WebViewState: ObservableObject {
    @Published var failed: Bool = false
    @Published var failureMessage: String = ""
    @Published var reloadToken: Int = 0

    func retry() { failed = false; reloadToken &+= 1 }
}

/// A WKWebView wrapper that loads the configured server URL.
///
/// Load-bearing behaviors (from the plan's review):
///  - loads ONLY when the normalized URL changes OR a reload is explicitly
///    requested, so unrelated SwiftUI updates never reload and drop the PWA's
///    WebSocket session;
///  - a navigation policy that allows only https / loopback-http to the
///    configured origin and cancels file:/custom-scheme/off-origin navigations;
///  - `activeNavigation` tracks the LATEST navigation (set in
///    didStartProvisionalNavigation), so failure callbacks apply to in-page PWA
///    navigations too, while a superseded navigation's late failure is ignored.
struct WebView: UIViewRepresentable {
    let url: URL
    @ObservedObject var state: WebViewState
    /// Bearer token to seed into the PWA's sessionStorage. Empty → no seeding
    /// (the PWA shows its own token page).
    var token: String = ""

    func makeCoordinator() -> Coordinator { Coordinator(state: state) }

    func makeUIView(context: Context) -> WKWebView {
        let web = WKWebView(frame: .zero, configuration: WKWebViewConfiguration())
        web.navigationDelegate = context.coordinator
        web.allowsBackForwardNavigationGestures = true
        return web
    }

    func updateUIView(_ web: WKWebView, context: Context) {
        let coord = context.coordinator
        let urlChanged = coord.loadedURL?.absoluteString != url.absoluteString
        let tokenChanged = coord.lastToken != token
        let reloadRequested = coord.lastReloadToken != state.reloadToken
        guard urlChanged || tokenChanged || reloadRequested else { return }
        coord.loadedURL = url
        coord.lastToken = token
        coord.lastReloadToken = state.reloadToken
        state.failed = false

        // At document-start, scoped to the configured origin, either SET the
        // token into sessionStorage (so the PWA skips its token page) or REMOVE
        // it when the field is blank (sessionStorage survives reloads, so just
        // dropping the script would leave a stale token active). Origin-scoped so
        // the token is never written on any other page the web view reaches.
        let ucc = web.configuration.userContentController
        ucc.removeAllUserScripts()
        if let origin = Self.expectedOrigin(url) {
            ucc.addUserScript(Self.storageScript(token: token, origin: origin))
        }
        web.load(URLRequest(url: url))
    }

    /// The browser-normalized origin (default ports dropped) of `url`, matching
    /// what `window.location.origin` reports.
    private static func expectedOrigin(_ url: URL) -> String? {
        guard let scheme = url.scheme?.lowercased(), let host = url.host?.lowercased() else {
            return nil
        }
        // Bracket IPv6 literals so `::1` becomes `[::1]`, matching what
        // `window.location.origin` reports (URL.host strips the brackets).
        let hostPart = host.contains(":") ? "[\(host)]" : host
        var origin = "\(scheme)://\(hostPart)"
        if let port = url.port,
           !(scheme == "https" && port == 443),
           !(scheme == "http" && port == 80) {
            origin += ":\(port)"
        }
        return origin
    }

    /// A document-start, origin-scoped script that SETs the token when non-empty
    /// or REMOVEs it when empty. JSON-encodes values so nothing breaks out of the
    /// literal; on encoding failure it falls back to a no-op.
    private static func storageScript(token: String, origin: String) -> WKUserScript {
        let noop = WKUserScript(source: ";", injectionTime: .atDocumentStart, forMainFrameOnly: true)
        guard let originJSON = jsonString(origin) else { return noop }
        let op: String
        if token.isEmpty {
            op = "window.sessionStorage.removeItem(\"copad.token\");"
        } else if let tokenJSON = jsonString(token) {
            op = "window.sessionStorage.setItem(\"copad.token\",\(tokenJSON));"
        } else {
            return noop
        }
        let source = "(function(){try{if(window.location.origin===\(originJSON)){\(op)}}catch(e){}})();"
        return WKUserScript(source: source, injectionTime: .atDocumentStart, forMainFrameOnly: true)
    }

    private static func jsonString(_ s: String) -> String? {
        guard let data = try? JSONSerialization.data(withJSONObject: [s], options: []),
              let arr = String(data: data, encoding: .utf8)
        else { return nil }
        // `["..."]` → strip the array brackets to get the bare JSON string literal.
        return String(arr.dropFirst().dropLast())
    }

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate {
        let state: WebViewState
        var loadedURL: URL?
        var lastToken: String?
        var lastReloadToken: Int = 0
        private var activeNavigation: WKNavigation?

        init(state: WebViewState) { self.state = state }

        // Restrict navigation to https / loopback-http on the configured origin.
        func webView(_ webView: WKWebView,
                     decidePolicyFor navigationAction: WKNavigationAction,
                     decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
            guard let target = navigationAction.request.url,
                  AppModel.validate(target.absoluteString) != nil,
                  loadedURL == nil || sameOrigin(target, loadedURL!)
            else {
                decisionHandler(.cancel)
                return
            }
            decisionHandler(.allow)
        }

        func webView(_ webView: WKWebView,
                     didStartProvisionalNavigation navigation: WKNavigation!) {
            // The most recent navigation is the one whose outcome we care about.
            activeNavigation = navigation
            state.failed = false
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            guard navigation == activeNavigation else { return }
            state.failed = false
        }

        func webView(_ webView: WKWebView,
                     didFail navigation: WKNavigation!, withError error: Error) {
            reportIfActive(navigation, error)
        }

        func webView(_ webView: WKWebView,
                     didFailProvisionalNavigation navigation: WKNavigation!,
                     withError error: Error) {
            reportIfActive(navigation, error)
        }

        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
            // Show the retry UI and STOP. Do not clear loadedURL — that would
            // make updateUIView see a "changed" URL and silently auto-reload,
            // bypassing the failure state. The Retry button (reloadToken) is the
            // explicit path back, which reloads and revives the content process.
            state.failed = true
            state.failureMessage = "The web view stopped. Tap retry."
        }

        private func reportIfActive(_ navigation: WKNavigation!, _ error: Error) {
            // Ignore a late failure from a navigation we've since superseded.
            guard navigation == activeNavigation else { return }
            let ns = error as NSError
            if ns.domain == NSURLErrorDomain && ns.code == NSURLErrorCancelled { return }
            state.failed = true
            state.failureMessage = error.localizedDescription
        }

        private func sameOrigin(_ a: URL, _ b: URL) -> Bool {
            a.scheme?.lowercased() == b.scheme?.lowercased()
                && a.host?.lowercased() == b.host?.lowercased()
                && effectivePort(a) == effectivePort(b)
        }

        // Normalize the implicit default port so `https://h` and `https://h:443`
        // (and http :80) count as the same origin — otherwise a canonical
        // redirect between the two equivalent forms gets cancelled.
        private func effectivePort(_ u: URL) -> Int? {
            if let p = u.port { return p }
            switch u.scheme?.lowercased() {
            case "https": return 443
            case "http": return 80
            default: return nil
            }
        }
    }
}
