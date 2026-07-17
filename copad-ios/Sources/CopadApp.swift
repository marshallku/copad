import SwiftUI

/// copad iOS shell — a thin native wrapper around the web-bridge PWA. The whole
/// UI (terminal attach, presence, events, pilot) comes from web-bridge over
/// WKWebView; the native layer adds the app shell, a validated server-URL
/// setting, and a local-notification permission scaffold. See README.md.
@main
struct CopadApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
