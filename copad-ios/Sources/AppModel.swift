import Foundation
import UserNotifications

/// App state: the validated server URL (persisted) + local-notification
/// authorization status. No bearer token lives here — auth is entirely the
/// web-bridge PWA's job (it stores the token in sessionStorage and shows its
/// own token page); a native token field would be a nonfunctional duplicate
/// of a high-authority secret.
@MainActor
final class AppModel: ObservableObject {
    /// The validated, ready-to-load server URL. `nil` → show settings.
    @Published private(set) var serverURL: URL?
    @Published private(set) var notifStatus: UNAuthorizationStatus = .notDetermined

    private let defaultsKey = "serverURLString"

    init() {
        // A `-ServerURL <url>` launch argument overrides persisted state on
        // EVERY launch, so headless verification runs are deterministic
        // regardless of what a prior install left in UserDefaults.
        let args = ProcessInfo.processInfo.arguments
        if let i = args.firstIndex(of: "-ServerURL"), i + 1 < args.count {
            UserDefaults.standard.set(args[i + 1], forKey: defaultsKey)
        }
        let stored = UserDefaults.standard.string(forKey: defaultsKey) ?? ""
        serverURL = Self.validate(stored)
    }

    var serverURLString: String {
        UserDefaults.standard.string(forKey: defaultsKey) ?? ""
    }

    /// Persist + apply a new server URL. Returns false (and does not persist)
    /// if it fails validation.
    @discardableResult
    func apply(_ raw: String) -> Bool {
        guard let url = Self.validate(raw) else { return false }
        UserDefaults.standard.set(raw.trimmingCharacters(in: .whitespacesAndNewlines),
                                  forKey: defaultsKey)
        serverURL = url
        return true
    }

    func clear() {
        UserDefaults.standard.removeObject(forKey: defaultsKey)
        serverURL = nil
    }

    /// HTTPS is required except for explicit loopback dev hosts. A plain-`http`
    /// URL to an arbitrary host would send the PWA's bearer token — which can
    /// drive the workstation terminal — over cleartext; ATS's
    /// `NSAllowsLocalNetworking` does not stop that (it permits IP-literal
    /// loads). So the trust decision lives here, not only in ATS.
    static func validate(_ raw: String) -> URL? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased(),
              let host = url.host?.lowercased()
        else { return nil }
        if scheme == "https" { return url }
        let loopback: Set<String> = ["127.0.0.1", "::1", "localhost"]
        if scheme == "http", loopback.contains(host) { return url }
        return nil
    }

    // MARK: - Local notifications (permission scaffold only)
    //
    // v1 requests LOCAL-notification permission and surfaces the status. It does
    // NOT call registerForRemoteNotifications — real APNs needs an
    // `aps-environment` entitlement + server support (web-bridge WU2b) + a real
    // device, and calling it now would only hit didFailToRegister. Remote push
    // is shown as "pending" in the UI.
    func refreshNotifStatus() async {
        let settings = await UNUserNotificationCenter.current().notificationSettings()
        notifStatus = settings.authorizationStatus
    }

    func requestNotifications() async {
        _ = try? await UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge])
        await refreshNotifStatus()
    }
}
