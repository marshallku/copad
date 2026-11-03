import SwiftUI
@preconcurrency import WebKit

/// The comux work graph on iOS (decisions #204): the shared page (`copad-graph-web`, bundled as
/// resources and assembled here the way `copad_graph_web::page` does it) in a sheet over the
/// session, fed from the session's L0 model. The comux is always remote here, so the page is
/// told so and never offers to open a file.
struct GraphSheet: View {
    @ObservedObject var session: MuxSession
    var close: () -> Void
    @State private var readout = ""

    var body: some View {
        NavigationStack {
            GraphWebView(session: session, readout: $readout, jumped: close)
                .background(ComuxLook.base)
                .navigationTitle("Work graph")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done", action: close).accessibilityIdentifier("closeGraph")
                    }
                }
                // What the page reads out, for the UI test (it cannot reach into the web view).
                .overlay(alignment: .bottomLeading) {
                    Text(readout).font(.system(size: 1)).opacity(0.01)
                        .accessibilityIdentifier("graphState").accessibilityLabel(readout)
                }
        }
        .preferredColorScheme(.dark)
    }
}

/// The page markers `copad-graph-web/src/lib.rs` replaces; keep them in step.
enum GraphPage {
    static func html() -> String? {
        func read(_ name: String, _ ext: String) -> String? {
            Bundle.main.url(forResource: name, withExtension: ext).flatMap { try? String(contentsOf: $0, encoding: .utf8) }
        }
        guard let html = read("graph", "html"), let css = read("graph", "css"),
              let model = read("model", "js"), let graph = read("graph", "js") else { return nil }
        return html.replacingOccurrences(of: "/*COPAD_GRAPH_CSS*/", with: css)
            .replacingOccurrences(of: "//COPAD_GRAPH_JS", with: model + "\n" + graph)
    }
}

struct GraphWebView: UIViewRepresentable {
    @ObservedObject var session: MuxSession
    @Binding var readout: String
    var jumped: () -> Void

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.userContentController.add(WeakHandler(context.coordinator), name: "comux")
        let web = WKWebView(frame: .zero, configuration: config)
        web.isOpaque = false
        web.backgroundColor = .clear
        web.scrollView.isScrollEnabled = false
        web.navigationDelegate = context.coordinator
        context.coordinator.web = web
        if let html = GraphPage.html() { web.loadHTMLString(html, baseURL: nil) }
        context.coordinator.startReadout()
        return web
    }

    func updateUIView(_ web: WKWebView, context: Context) {
        context.coordinator.parent = self
        context.coordinator.feed()
    }

    static func dismantleUIView(_ web: WKWebView, coordinator: Coordinator) {
        coordinator.timer?.invalidate()
    }

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate {
        var parent: GraphWebView
        weak var web: WKWebView?
        var timer: Timer?
        private var loaded = false
        private var fedEpoch = ""
        private var fedGraph: Data?
        private var fedAgents: L0Agents?
        private var followed: String?
        private var pickedAt: Date?

        init(_ parent: GraphWebView) { self.parent = parent }

        private var session: MuxSession { parent.session }

        func feed() {
            guard loaded else { return }
            let epoch = "remote#\(session.welcomes)"
            if epoch != fedEpoch {
                js("comuxGraph.reset()")
                js("comuxGraph.setHost({\"local\":false,\"epoch\":\(quote(epoch))})")
                fedEpoch = epoch
                fedGraph = nil
                fedAgents = nil
                followed = nil
                js(themeJS())
            }
            let m = session.model
            if m.graph != fedGraph || m.agents != fedAgents {
                fedGraph = m.graph
                fedAgents = m.agents
                let graph = m.graph.flatMap { String(data: $0, encoding: .utf8) } ?? "{\"agents\":[]}"
                let agents = (try? JSONEncoder().encode(m.agents?.agents ?? [])).flatMap { String(data: $0, encoding: .utf8) } ?? "[]"
                js("comuxGraph.setData({\"graph\":\(graph),\"agents\":\(agents)})")
            }
            if let focused = m.agents?.agents.first(where: { $0.focused == true })?.token, focused != followed,
               pickedAt.map({ Date().timeIntervalSince($0) > 30 }) ?? true {
                pickedAt = nil
                followed = focused
                js("comuxGraph.setAgent(\(quote(focused)))")
            }
        }

        private func themeJS() -> String {
            "comuxGraph.setTheme({\"font\":\"JetBrainsMono Nerd Font Mono\",\"fontSize\":11})"
        }

        func startReadout() {
            timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self, self.loaded else { return }
                    self.web?.evaluateJavaScript("JSON.stringify(comuxGraph.state())") { v, _ in
                        if let s = v as? String { self.parent.readout = s }
                    }
                }
            }
        }

        fileprivate func received(_ message: WKScriptMessage) {
            guard message.frameInfo.isMainFrame else { return }
            guard let m = ComuxGraphMessage.parse(message.body, epoch: fedEpoch.isEmpty ? "" : "remote#\(session.welcomes)") else { return }
            switch m {
            case .ready:
                loaded = true
                fedEpoch = ""
                feed()
            case .jump(let token):
                session.jump(token)
                parent.jumped()
            case .history(let token, let before, let limit):
                var req: [String: Any] = ["cmd": "graph", "target": token]
                if let before { req["before"] = before }
                if let limit { req["limit"] = limit }
                let epoch = fedEpoch
                session.command(req) { [weak self] resp in
                    Task { @MainActor in
                        guard let self, self.fedEpoch == epoch else { return }
                        if resp["ok"] as? Bool == true, let page = resp["graph"],
                           let d = try? JSONSerialization.data(withJSONObject: page), let s = String(data: d, encoding: .utf8) {
                            self.js("comuxGraph.addHistory(\(s))")
                        } else {
                            let error = resp["error"] as? String ?? ""
                            let code = error.contains("unknown variant") ? "unsupported" : (resp["code"] as? String ?? "error")
                            self.js("comuxGraph.historyFailed({\"code\":\(self.quote(code))})")
                        }
                    }
                }
            case .agent:
                pickedAt = Date()
                followed = nil
            case .open, .mode:
                break // a remote comux's files are not on this device
            }
        }

        private func quote(_ s: String) -> String {
            (try? JSONSerialization.data(withJSONObject: [s])).flatMap { String(data: $0, encoding: .utf8) }
                .map { String($0.dropFirst().dropLast()) } ?? "\"\""
        }

        private func js(_ source: String) {
            web?.evaluateJavaScript(source, completionHandler: nil)
        }

        func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                     decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
            let initial = action.navigationType == .other && (action.request.url?.absoluteString ?? "about:blank") == "about:blank"
            decisionHandler(initial ? .allow : .cancel)
        }

        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
            loaded = false
            if let html = GraphPage.html() { webView.loadHTMLString(html, baseURL: nil) }
        }
    }

    /// `WKUserContentController` retains its handlers: hold the coordinator weakly.
    private final class WeakHandler: NSObject, WKScriptMessageHandler {
        weak var target: Coordinator?
        init(_ target: Coordinator) { self.target = target }
        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            MainActor.assumeIsolated { target?.received(message) }
        }
    }
}
