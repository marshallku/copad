// Runs one scenario page of scripts/e2e-graph-page.sh in a real WKWebView (the engine copad's
// macOS and iOS hosts load the work-graph page in), with a real `comux` message handler.
//
//   swift scripts/lib/webkit-page-runner.swift <scenario.html> [snapshot.png]
//
// Prints the scenario's result JSON (what it wrote between @@OUT@@ … @@END@@), with `posts`
// replaced by the messages the page really posted to the handler. Exits 1 on a timeout.
import AppKit
import WebKit

final class Runner: NSObject, WKScriptMessageHandler {
    var posts: [Any] = []
    let web: WKWebView
    let shot: String?

    init(html: String, shot: String?) {
        let config = WKWebViewConfiguration()
        web = WKWebView(frame: NSRect(x: 0, y: 0, width: 900, height: 640), configuration: config)
        self.shot = shot
        super.init()
        config.userContentController.add(self, name: "comux")
        web.loadHTMLString(html, baseURL: nil)
    }

    func userContentController(_ c: WKUserContentController, didReceive m: WKScriptMessage) {
        posts.append(m.body)
    }

    func poll(started: Date) {
        web.evaluateJavaScript("(document.getElementById('e2e-out') || {}).textContent || ''") { v, _ in
            let text = v as? String ?? ""
            if let a = text.range(of: "@@OUT@@"), let b = text.range(of: "@@END@@"),
               var out = try? JSONSerialization.jsonObject(with: Data(text[a.upperBound..<b.lowerBound].utf8)) as? [String: Any] {
                out["posts"] = self.posts
                out["engine"] = "webkit"
                let finish = {
                    let d = try! JSONSerialization.data(withJSONObject: out)
                    print(String(data: d, encoding: .utf8)!)
                    exit(0)
                }
                guard let shot = self.shot else { return finish() }
                self.web.takeSnapshot(with: nil) { image, _ in
                    if let tiff = image?.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
                       let png = rep.representation(using: .png, properties: [:]) {
                        try? png.write(to: URL(fileURLWithPath: shot))
                    }
                    finish()
                }
                return
            }
            if Date().timeIntervalSince(started) > 30 {
                FileHandle.standardError.write("timeout\n".data(using: .utf8)!)
                exit(1)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { self.poll(started: started) }
        }
    }
}

let args = CommandLine.arguments
guard args.count >= 2, let html = try? String(contentsOfFile: args[1], encoding: .utf8) else {
    FileHandle.standardError.write("usage: webkit-page-runner.swift <scenario.html> [snapshot.png]\n".data(using: .utf8)!)
    exit(2)
}
let app = NSApplication.shared
app.setActivationPolicy(.prohibited)
let runner = Runner(html: html, shot: args.count > 2 ? args[2] : nil)
DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { runner.poll(started: Date()) }
app.run()
