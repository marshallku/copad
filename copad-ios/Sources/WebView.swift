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
        let reloadRequested = coord.lastReloadToken != state.reloadToken
        guard urlChanged || reloadRequested else { return }
        coord.loadedURL = url
        coord.lastReloadToken = state.reloadToken
        state.failed = false
        web.load(URLRequest(url: url))
    }

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate {
        let state: WebViewState
        var loadedURL: URL?
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
