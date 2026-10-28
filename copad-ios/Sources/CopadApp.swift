import SwiftUI

/// copad iOS. The default is the NATIVE comux client (decisions #168): SSH straight to a
/// machine running comux, its screen drawn natively, its sidebar/status/notifications as
/// native UI. The web-bridge PWA shell (`ContentView`) stays reachable as "Web" in Debug builds
/// only: it needs the owner's own web-bridge, and a store build must not route there even when an
/// earlier TestFlight build left `copad.webMode` on. See README.md.
@main
struct CopadApp: App {
    /// APNs token callbacks and notification presentation (decisions #175).
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @AppStorage("copad.webMode") private var webMode = false

    var body: some Scene {
        WindowGroup {
            #if DEBUG
            if webMode {
                ContentView()
                    .overlay(alignment: .topLeading) {
                        Button("Native") { webMode = false }.padding(10)
                    }
            } else {
                HostsScreen(openWeb: { webMode = true })
            }
            #else
            HostsScreen(openWeb: nil)
            #endif
        }
    }
}
