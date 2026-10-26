import SwiftUI
import UIKit
import UserNotifications

/// APNs for agent notifications (decisions #175, #177). comux pushes a notification it raises to
/// every phone that registered with it, sealed with the key this app gave it for that host; this
/// app registers its device token and key with each comux it connects to, keeps a banner from
/// showing for the host already on screen, and turns a tap into "open that host, jump to that
/// pane" — only for a push that opens with that host's key.
@MainActor
final class PushRegistry: ObservableObject {
    static let shared = PushRegistry()

    /// This install's APNs token (hex), once iOS has handed one over.
    @Published private(set) var token: String?
    /// Why there is no token, or why a host refused it — shown on the hosts screen.
    @Published private(set) var status: [String: String] = [:]
    /// A tapped notification waiting for its host to be opened: (host id, pane token).
    @Published var tapped: (host: String, pane: String)?
    /// The host whose session is on screen (no banner for it — the user is looking).
    var visibleHost: String?

    private var asked = false

    /// The sandbox in debug builds, production in release (TestFlight / App Store) — it must
    /// match the `aps-environment` the build is signed with.
    static var environment: String {
        #if DEBUG
        return "sandbox"
        #else
        return "production"
        #endif
    }

    /// Ask once for permission to show alerts, then for a device token (granted or not: the
    /// token is what comux needs; iOS decides whether a push is shown).
    func enable() {
        guard !asked else { return }
        asked = true
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in
            Task { @MainActor in UIApplication.shared.registerForRemoteNotifications() }
        }
    }

    func didRegister(_ deviceToken: Data) {
        token = deviceToken.map { String(format: "%02x", $0) }.joined()
        status["token"] = nil
    }

    func didFail(_ error: Error) {
        status["token"] = "No push token: \(error.localizedDescription)"
    }

    func setStatus(host: String, _ text: String?) { status[host] = text }

    /// What's wrong with a `push-register` reply, or nil. A comux that doesn't say it seals
    /// pushes (`e2e`) is older than encryption: count it as no pushes rather than trust it.
    nonisolated static func registrationProblem(_ resp: [String: Any]) -> String? {
        guard resp["ok"] as? Bool ?? false else { return resp["error"] as? String ?? "push refused" }
        guard (resp["e2e"] as? Int ?? 0) >= 1 else { return "update comux on this host to get pushes" }
        return nil
    }
}

/// The app delegate SwiftUI lacks: APNs token callbacks and notification presentation.
final class AppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    func application(_ application: UIApplication,
                     didFinishLaunchingWithOptions _: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        return true
    }

    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken token: Data) {
        Task { @MainActor in PushRegistry.shared.didRegister(token) }
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        Task { @MainActor in PushRegistry.shared.didFail(error) }
    }

    /// A push for the host on screen shows nothing: the user is already looking at it.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent n: UNNotification,
                                withCompletionHandler done: @escaping (UNNotificationPresentationOptions) -> Void) {
        let host = PushEnvelope.open(n.request.content.userInfo)?.host
        Task { @MainActor in
            done(host != nil && host == PushRegistry.shared.visibleHost ? [] : [.banner, .list, .sound])
        }
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive r: UNNotificationResponse,
                                withCompletionHandler done: @escaping () -> Void) {
        // Routing is trusted only from a push that opens with the host's key: the relay could
        // otherwise send any host and pane, with or without the extension having run.
        if let opened = PushEnvelope.open(r.notification.request.content.userInfo) {
            Task { @MainActor in PushRegistry.shared.tapped = (opened.host, opened.pane) }
        }
        done()
    }
}
