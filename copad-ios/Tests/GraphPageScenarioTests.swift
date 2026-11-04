import WebKit
import XCTest

/// One scenario of `scripts/e2e-graph-page.sh` (`ENGINE=ios`) in the simulator's WKWebView, with a
/// real `comux` message handler — the iOS twin of `scripts/lib/webkit-page-runner.swift`. The
/// script passes the scenario page and where to write its result; run alone it skips.
@MainActor
final class GraphPageScenarioTests: XCTestCase, WKScriptMessageHandler {
    private var posts: [Any] = []

    nonisolated func userContentController(_ c: WKUserContentController, didReceive m: WKScriptMessage) {
        let body = m.body
        MainActor.assumeIsolated { posts.append(body) }
    }

    func testScenario() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let scenario = env["GRAPH_SCENARIO"], let outPath = env["GRAPH_OUT"] else {
            throw XCTSkip("run by scripts/e2e-graph-page.sh with ENGINE=ios")
        }
        let html = try String(contentsOfFile: scenario, encoding: .utf8)
        let config = WKWebViewConfiguration()
        config.userContentController.add(self, name: "comux")
        let web = WKWebView(frame: CGRect(x: 0, y: 0, width: 900, height: 640), configuration: config)
        web.loadHTMLString(html, baseURL: nil)
        let deadline = Date().addingTimeInterval(30)
        while Date() < deadline {
            try await Task.sleep(nanoseconds: 200_000_000)
            let text = (try? await web.evaluateJavaScript("(document.getElementById('e2e-out') || {}).textContent || ''") as? String) ?? ""
            guard let a = text.range(of: "@@OUT@@"), let b = text.range(of: "@@END@@") else { continue }
            var out = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(text[a.upperBound..<b.lowerBound].utf8)) as? [String: Any])
            out["posts"] = posts
            out["engine"] = "ios"
            try JSONSerialization.data(withJSONObject: out).write(to: URL(fileURLWithPath: outPath))
            return
        }
        XCTFail("the scenario never finished")
    }
}
