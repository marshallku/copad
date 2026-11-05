import AppKit
import CCopadFFI
import CopadCore
import SwiftUI
@preconcurrency import WebKit

/// The comux work graph in the window's right panel (decisions #204): the shared page
/// (`copad-graph-web`) in a transparent web view, fed from the window's comux, its messages
/// validated (`ComuxGraphMessage`) before anything acts on them.
@MainActor
final class ComuxGraphPanel: NSView, WKNavigationDelegate {
    private let web: WKWebView
    private weak var host: ComuxWindow?
    private(set) var loaded = false
    private var fedEpoch = ""
    private var fedRev: UInt64?
    private var fedAgentsRev: UInt64?
    private var fedPage: UInt64 = 0
    private var fedTheme: ComuxStyle?
/// A pick in the page holds for `pickHolds`, then the panel follows focus again.
    private var followed: String?
    private var pickedAt: Date?
    private let pickHolds: TimeInterval = 30
    var refocusPane: () -> Void = {}
    var modeChanged: (String) -> Void = { _ in }

    init(host: ComuxWindow) {
        self.host = host
        let config = WKWebViewConfiguration()
        let proxy = WeakGraphHandler()
        config.userContentController.add(proxy, name: "comux")
        web = WKWebView(frame: .zero, configuration: config)
        super.init(frame: .zero)
        proxy.panel = self
        web.navigationDelegate = self
        web.setValue(false, forKey: "drawsBackground")
        web.translatesAutoresizingMaskIntoConstraints = false
        web.setAccessibilityIdentifier("comuxGraphWeb")
        addSubview(web)
        let rule = NSBox()
        rule.boxType = .custom
        rule.borderWidth = 0
        rule.translatesAutoresizingMaskIntoConstraints = false
        addSubview(rule)
        self.rule = rule
        NSLayoutConstraint.activate([
            rule.leadingAnchor.constraint(equalTo: leadingAnchor),
            rule.topAnchor.constraint(equalTo: topAnchor),
            rule.bottomAnchor.constraint(equalTo: bottomAnchor),
            rule.widthAnchor.constraint(equalToConstant: 1),
            web.leadingAnchor.constraint(equalTo: rule.trailingAnchor),
            web.trailingAnchor.constraint(equalTo: trailingAnchor),
            web.topAnchor.constraint(equalTo: topAnchor),
            web.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
        setAccessibilityIdentifier("comuxGraph")
        if let p = copad_graph_page() {
            let html = String(cString: p)
            copad_ffi_free_string(p)
            web.loadHTMLString(html, baseURL: nil)
        }
    }

    private var rule: NSBox!

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    // MARK: feeding

    func feed() {
        guard loaded, let host else { return }
        applyTheme(host.style)
        guard let key = host.key, let meta = host.state?.graph else {
            if !fedEpoch.isEmpty {
                js("comuxGraph.reset()")
                fedEpoch = ""
            }
            return
        }
        let epoch = "\(key)#\(meta.epoch)"
        if epoch != fedEpoch {
            js("comuxGraph.reset()")
            js("comuxGraph.setHost(\(json(["local": key.isEmpty, "epoch": epoch])))")
            fedEpoch = epoch
            fedRev = nil
            fedAgentsRev = nil
            // An answer to a request of the previous connection is not this page's.
            fedPage = meta.pages[pageConsumer] ?? 0
            followed = nil
            pickedAt = nil
        }
        let page = meta.pages[pageConsumer] ?? 0
        if meta.rev != fedRev || meta.agentsRev != fedAgentsRev || page != fedPage,
           let raw = ComuxHost.shared.graphJSON(key: key, consumer: pageConsumer),
           let all = try? JSONSerialization.jsonObject(with: Data(raw.utf8)) as? [String: Any] {
            if meta.rev != fedRev || meta.agentsRev != fedAgentsRev, let data = all["data"] {
                js("comuxGraph.setData(\(json(data)))")
                fedRev = meta.rev
                fedAgentsRev = meta.agentsRev
            }
            if page != fedPage, let answer = all["page"] as? [String: Any] {
                if answer["ok"] as? Bool == true, let p = answer["page"] {
                    js("comuxGraph.addHistory(\(json(p)))")
                } else {
                    js("comuxGraph.historyFailed(\(json(["code": answer["code"] ?? "error"])))")
                }
            }
            fedPage = page
        }
        followFocus()
    }

    private func followFocus() {
        guard let focused = host?.state?.agents?.agents.first(where: { $0.focused })?.token else { return }
        if let at = pickedAt, Date().timeIntervalSince(at) < pickHolds { return }
        pickedAt = nil
        guard focused != followed else { return }
        followed = focused
        js("comuxGraph.setAgent(\(json(focused)))")
    }

    private func applyTheme(_ st: ComuxStyle) {
        guard st != fedTheme else { return }
        fedTheme = st
        rule.fillColor = NSColor(st.rule)
        let hex = { (c: Color) -> String in
            let n = NSColor(c).usingColorSpace(.sRGB) ?? .gray
            return String(format: "#%02x%02x%02x", Int(n.redComponent * 255), Int(n.greenComponent * 255), Int(n.blueComponent * 255))
        }
        let theme: [String: Any] = [
            "base": hex(st.base), "text": hex(st.text), "subtext": hex(st.subtext), "dim": hex(st.dim),
            "rule": hex(st.rule), "hover": hex(st.hover), "accent": hex(st.accent), "mauve": hex(st.mauve),
            "peach": hex(st.peach), "green": hex(st.green), "yellow": hex(st.yellow), "blue": hex(st.blue),
            "red": hex(st.red), "font": st.font.familyName ?? st.font.fontName, "fontSize": Double(st.font.pointSize),
        ]
        js("comuxGraph.setTheme(\(json(theme)))")
        js("comuxGraph.setReduceMotion(\(NSWorkspace.shared.accessibilityDisplayShouldReduceMotion))")
    }

    func setMode(_ mode: String) {
        guard loaded else { return }
        js("comuxGraph.setMode(\(json(mode)))")
    }

    func pageState(_ done: @escaping (Any?) -> Void) {
        guard loaded else { return done(nil) }
        web.evaluateJavaScript("JSON.stringify(comuxGraph.state())") { v, _ in
            done((v as? String).flatMap { try? JSONSerialization.jsonObject(with: Data($0.utf8)) })
        }
    }

    private var pageConsumer: String { (host?.consumer ?? "") + "-graph" }

    private func js(_ source: String) {
        web.evaluateJavaScript(source, completionHandler: nil)
    }

    private func json(_ v: Any) -> String {
        if let s = v as? String {
            return (try? String(data: JSONSerialization.data(withJSONObject: [s]), encoding: .utf8))
                .map { String($0.dropFirst().dropLast()) } ?? "null"
        }
        guard JSONSerialization.isValidJSONObject(v), let d = try? JSONSerialization.data(withJSONObject: v),
              let s = String(data: d, encoding: .utf8) else { return "null" }
        return s
    }

    // MARK: messages

    fileprivate func received(_ message: WKScriptMessage) {
        guard message.frameInfo.isMainFrame, let host, let key = host.key else {
            if (message.body as? [String: Any])?["type"] as? String == "ready" { ready() }
            return
        }
        // Validated against the window's connection NOW, not the one the page was last fed: a
        // message queued for a connection the window has since left (a remote comux's `open`)
        // is dropped, also before the page has been fed the new one.
        let current = host.state?.graph.map { "\(key)#\($0.epoch)" } ?? ""
        guard let m = ComuxGraphMessage.parse(message.body, epoch: current == fedEpoch ? current : "") else { return }
        switch m {
        case .ready:
            ready()
        case .jump(let token):
            host.command(["cmd": "jump", "target": token])
            refocusPane()
        case .open(let path):
            // Only a file on THIS machine: a remote comux's paths name another computer's disk.
            guard key.isEmpty, let url = ComuxGraphMessage.openableFile(path) else { return }
            NSWorkspace.shared.open(url)
        case .history(let token, let before, let limit):
            ComuxHost.shared.graphHistory(key: key, consumer: pageConsumer, token: token,
                                          before: before ?? -1, limit: Int64(limit ?? 0))
        case .mode(let mode):
            modeChanged(mode)
        case .agent:
            // Held for a while; when the hold ends the panel goes back to the focused agent,
            // even one that never changed meanwhile.
            pickedAt = Date()
            followed = nil
            // Nothing else may come along to feed the panel when the hold ends.
            DispatchQueue.main.asyncAfter(deadline: .now() + pickHolds + 0.1) { [weak self] in self?.feed() }
        }
    }

    private func ready() {
        loaded = true
        fedEpoch = ""
        fedTheme = nil
        feed()
        modeChanged(host?.graphMode ?? "agent")
        setMode(host?.graphMode ?? "agent")
    }

    // MARK: navigation — the page is the only document this view ever shows

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
        let initial = action.navigationType == .other && (action.request.url?.absoluteString ?? "about:blank") == "about:blank"
        decisionHandler(initial ? .allow : .cancel)
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        loaded = false
        if let p = copad_graph_page() {
            let html = String(cString: p)
            copad_ffi_free_string(p)
            web.loadHTMLString(html, baseURL: nil)
        }
    }
}

/// `WKUserContentController` retains its handlers: this one holds the panel weakly so the
/// panel (and the web view inside it) can go away with the window.
private final class WeakGraphHandler: NSObject, WKScriptMessageHandler {
    weak var panel: ComuxGraphPanel?

    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        MainActor.assumeIsolated { panel?.received(message) }
    }
}

/// The top bar of the native chrome (decisions #204): where the view is (session › tab › the
/// focused agent and what it is doing) on the left; the work graph, the all-sessions view and
/// the notifications on the right. Like the rest of the chrome it never takes the keys.
struct ComuxTopBar: View {
    @ObservedObject var host: ComuxWindow
    var settle: () -> Void
    var toggleGraph: (String) -> Void
    var toggleNotifications: () -> Void

    var body: some View {
        let st = host.style
        let tree = host.state?.tree
        let session = tree?.sessions.first { $0.active }
        let tab = session?.tabs.first { $0.active }
        let agent = host.state?.agents?.agents.first { $0.focused }
        HStack(spacing: 8) {
            if let session {
                Text(session.name).font(st.strong).foregroundStyle(st.mauve).lineLimit(1)
            }
            if let tab {
                Text("›").foregroundStyle(st.dim)
                Text(tab.title).font(st.body).foregroundStyle(st.subtext).lineLimit(1)
            }
            if let agent {
                let (glyph, color) = st.status(agent.unseen == true ? "done" : agent.status)
                Text("›").foregroundStyle(st.dim)
                Text(glyph).foregroundStyle(color)
                Text(agent.tool).font(st.body).foregroundStyle(st.text).lineLimit(1)
                if let d = agent.detail, !d.isEmpty {
                    Text(d).font(st.body).foregroundStyle(st.dim).lineLimit(1).truncationMode(.tail)
                        .accessibilityIdentifier("comuxTopBarDetail")
                }
            }
            Spacer(minLength: 8)
            Button { toggleGraph("all") } label: { Image(systemName: "circle.grid.cross") }
                .foregroundStyle(host.graphOpen && host.graphMode == "all" ? st.accent : st.subtext)
                .help("Every agent's work, across sessions")
                .accessibilityIdentifier("comuxGraphAll")
            Button { toggleGraph("agent") } label: { Image(systemName: "point.3.connected.trianglepath.dotted") }
                .foregroundStyle(host.graphOpen && host.graphMode == "agent" ? st.accent : st.subtext)
                .help("Work graph (⌘⇧G)")
                .accessibilityIdentifier("comuxGraphToggle")
            Button(action: toggleNotifications) {
                HStack(spacing: 2) {
                    Image(systemName: host.notificationsShown ? "bell.fill" : "bell")
                    if let n = host.state?.notifications?.unread, n > 0 {
                        Text("\(n)").font(st.strong).foregroundStyle(st.yellow)
                    }
                }
            }
            .foregroundStyle(host.notificationsShown ? st.accent : st.subtext)
            .help("Notifications (Ctrl-b a)")
            .accessibilityIdentifier("comuxNotificationsToggle")
        }
        .buttonStyle(.borderless)
        .font(st.body)
        .padding(.horizontal, 10)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .overlay(alignment: .bottom) { Rectangle().fill(st.rule).frame(height: 1) }
        .accessibilityIdentifier("comuxTopBar")
    }
}
