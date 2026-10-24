import SwiftUI

/// copad iOS. The default is the NATIVE comux client (decisions #168): SSH straight to a
/// machine running comux, its screen drawn natively, its sidebar/status/notifications as
/// native UI. The web-bridge PWA shell (`ContentView`) stays reachable as "Web". See README.md.
@main
struct CopadApp: App {
    @AppStorage("copad.webMode") private var webMode = false

    var body: some Scene {
        WindowGroup {
            if webMode {
                ContentView()
                    .overlay(alignment: .topLeading) {
                        Button("Native") { webMode = false }.padding(10)
                    }
            } else {
                HostsScreen(openWeb: { webMode = true })
            }
        }
    }
}
