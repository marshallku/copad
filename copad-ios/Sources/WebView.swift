// The web-bridge shell is a Debug-only mode (CopadApp): store builds do not carry it.
#if DEBUG
import SwiftUI
import WebKit

/// Load state for the WebView, so ContentView can overlay a failure/retry UI. `reloadToken` is
/// bumped to force a reload of the same URL (Retry); a URL-equality guard alone would skip it.
@MainActor
final class WebViewState: ObservableObject {
    @Published var failed: Bool = false
    @Published var failureMessage: String = ""
    @Published var reloadToken: Int = 0

    func retry() { failed = false; reloadToken &+= 1 }
}

/// A WKWebView wrapper that loads the configured server URL.
///
/// * Loads ONLY when the normalized URL changes or a reload is requested, so unrelated SwiftUI
///   updates never reload and drop the PWA's WebSocket session.
/// * Navigation is limited to https / loopback-http on the configured origin; file:,
///   custom-scheme and off-origin navigations are cancelled.
/// * `activeNavigation` tracks the LATEST navigation, so failures apply to in-page PWA
///   navigations too while a superseded navigation's late failure is ignored.
struct WebView: UIViewRepresentable {
    let url: URL
    @ObservedObject var state: WebViewState
    /// Bearer token to seed into the PWA's sessionStorage; empty means no seeding.
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

        // At document-start, scoped to the configured origin, SET the token into sessionStorage
        // or REMOVE it when blank (sessionStorage survives reloads, so dropping the script would
        // leave a stale token). Origin-scoped so no other page gets the token.
        let ucc = web.configuration.userContentController
        ucc.removeAllUserScripts()
        if let origin = Self.expectedOrigin(url) {
            ucc.addUserScript(Self.storageScript(token: token, origin: origin))
        }
        web.load(URLRequest(url: url))
    }

    private static func expectedOrigin(_ url: URL) -> String? {
        guard let scheme = url.scheme?.lowercased(), let host = url.host?.lowercased() else {
            return nil
        }
        // Bracket IPv6 literals (`::1` -> `[::1]`) as `window.location.origin` does.
        let hostPart = host.contains(":") ? "[\(host)]" : host
        var origin = "\(scheme)://\(hostPart)"
        if let port = url.port,
           !(scheme == "https" && port == 443),
           !(scheme == "http" && port == 80) {
            origin += ":\(port)"
        }
        return origin
    }

    /// Document-start, origin-scoped script that SETs the token, or REMOVEs it when empty. Values
    /// are JSON-encoded so nothing breaks out of the literal; encoding failure yields a no-op.
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
            // Show the retry UI and STOP. Clearing loadedURL would make updateUIView see a
            // changed URL and silently auto-reload past the failure state; Retry (reloadToken)
            // is the explicit way back.
            state.failed = true
            state.failureMessage = "The web view stopped. Tap retry."
        }

        private func reportIfActive(_ navigation: WKNavigation!, _ error: Error) {
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

        // Treat `https://h` and `https://h:443` (and http :80) as one origin, or a canonical
        // redirect between the two forms gets cancelled.
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
#endif
